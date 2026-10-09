//! The review orchestrator (§3).

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use henk_agent::{AgentConfig, StopCause, Tool as _, ToolSet, prompts};
use henk_domain::allowlist::Platform;
use henk_domain::diff::ReviewDiff;
use henk_domain::finding::{Finding, FindingKey, FindingRegistry};
use henk_domain::ignore::PathFilter;
use henk_domain::marker::{Marker, MarkerKind, ModelId};
use henk_domain::review::{CommitSha, LaneOutcome, LaneResult, LaneSpec, ReviewOutcome};
use henk_domain::run::{RunId, RunKind};
use henk_domain::scope::Scope;
use henk_llm::ChatMessage;
use henk_mcp::McpSession;
use henk_platform::{PlatformWriter, PullRequestState, ReviewHandle, ReviewTarget};
use henk_session::{SessionSpec, model_id, platform_tools, run_session};
use henk_store::{FindingAction, NewRun, RunStatus, Stage, StageState};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, instrument, warn};

use crate::app::App;
use crate::fact_check::FactChecker;
use crate::ids::new_run_id;
use crate::lane_workspace::ReviewWorkspaces;
use crate::liveness::KeepAlive;
use crate::review_tools::{
    DiffFiles, GetFileDiff, ImproveFinding, LaneContext, ListChangedFiles, ListExistingFindings,
    PostFinding, ReadFile, WithdrawFinding, lane_continuation,
};
use crate::stages;

/// The review was cancelled because a newer commit arrived.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("superseded by a review of a newer commit")]
pub struct Superseded;

/// A person cancelled the review from the dashboard (#69).
#[derive(Debug, Clone, thiserror::Error)]
#[error("cancelled from the dashboard by {0}")]
pub struct CancelledBy(pub String);

/// The review was cancelled because Henk was told to stop.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("interrupted")]
pub struct Interrupted;

/// The pull request is not one Henk reviews: closed, a draft, not on the
/// allowlist, or no lanes configured. Not Henk's failure, unlike a read that
/// failed (#262). The reason is Henk's own words and safe to show.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct NotReviewable(pub String);

/// Turns left when a lane is told to wrap up (#12).
const LANE_TURN_WARNING_AT: u32 = 3;

/// Appended to the fact-checker's prompt when it has a workspace: one copy
/// serves every check session of the review in turn, and nothing resets it
/// between them.
const SHARED_COPY: &str = "Your copy is shared with the checks of this review that run after you, and they judge their drafts against it: run what you need, but do not change files in it. A file you write goes outside it, at a fresh path from `mktemp`.";

/// What a lane is told then. Lanes that run into the turn limit otherwise
/// end mid-review, with what they were sure of never posted.
const LANE_TURN_WARNING: &str = "3 turns left. Draft each finding you are sure of with post_finding now, one call per finding, then end your turn.";

/// One review run, as every step of it sees it: known once `run_review`
/// has the commit.
#[derive(Clone, Copy)]
struct ReviewRun<'a> {
    app: &'a App,
    writer: &'a Arc<dyn PlatformWriter>,
    target: &'a ReviewTarget,
    commit: &'a CommitSha,
    run: &'a RunId,
    link: &'a str,
    /// Whether the review posts on the pull request: the lanes do, a review
    /// loop does not (#285). A loop that declines hands over to the lanes,
    /// which post, so their cancel and failure notices post too.
    posts: &'a AtomicBool,
    /// The last commit a review loop pushed (#284), the pull request's new
    /// head, which every ending of the review reports its check on too.
    pushed_head: &'a Mutex<Option<CommitSha>>,
}

/// What every lane of a review shares, ready once the read session is up.
struct LaneInputs {
    session: Arc<dyn McpSession>,
    scope: Scope,
    registry: Arc<Mutex<FindingRegistry>>,
    /// The drafts every lane queues, checked and written after the lanes
    /// (#189).
    drafts: Arc<Mutex<henk_domain::draft::DraftBook>>,
    diff: Arc<ReviewDiff>,
    excluded: Vec<&'static str>,
    title: String,
    base_ref: String,
    cancel: CancellationToken,
    /// The profile's limits on commands in the review's workspaces.
    limits: henk_domain::workspace::Limits,
}

/// A request to review one pull/merge request.
#[derive(Debug, Clone)]
pub struct ReviewRequest {
    /// The target.
    pub target: ReviewTarget,
    /// The commit to review; `None` means the current head.
    pub commit: Option<CommitSha>,
    /// What started it, for the run record.
    pub trigger: String,
    /// Who asked, as a stable id, when someone did.
    pub requester: Option<String>,
    /// A comment to react 👀 to first: (id, is review comment).
    pub acknowledge: Option<(String, bool)>,
    /// The run id to use, when the caller already announced one.
    pub run: Option<RunId>,
    /// When the coordinator took the request, so the run can show how long
    /// it waited for a slot (#226). `None` outside the coordinator.
    pub submitted_at: Option<time::OffsetDateTime>,
    /// The check the coordinator opened as queued while it waited for a
    /// slot (#262); the review moves it to in progress instead of opening
    /// another.
    pub queued_check: Option<ReviewHandle>,
}

/// How a review ended, for the caller.
#[derive(Debug)]
pub struct ReviewReport {
    /// The run id.
    pub run: RunId,
    /// The outcome, when the review got far enough to have one.
    pub outcome: Option<ReviewOutcome>,
    /// The summary text posted.
    pub summary: Option<String>,
}

/// Runs one review end to end. Never panics; every failure ends up on the
/// pull request and in the run record (§8.8).
#[instrument(skip_all, fields(repo = %request.target.repo.path(), number = request.target.number))]
pub async fn run_review(
    app: &App,
    request: ReviewRequest,
    cancel: CancellationToken,
) -> anyhow::Result<ReviewReport> {
    let platform = request.target.platform();
    let writer = app.writer(platform)?;
    let run = request.run.clone().unwrap_or_else(new_run_id);
    let link = app.settings.run_link(&run);

    // Until the run is stored, a queued check links where the queue is
    // shown, as one that ends while queued does.
    let queue_link = app.settings.queue_link(&run);
    let (info, commit) = match preflight(app, &writer, &request).await {
        Ok(read) => read,
        Err(error) => {
            close_queued_check(&writer, &request, &queue_link, &error).await;
            return Err(error);
        }
    };

    let created = app
        .store
        .create_run(&NewRun {
            id: run.clone(),
            kind: RunKind::Review,
            platform,
            repo: request.target.repo.path(),
            target: request.target.number,
            commit: Some(commit.as_str().to_owned()),
            requester: request.requester.clone(),
            trigger: request.trigger.clone(),
            link: link.clone(),
        })
        .await;
    if let Err(error) = created {
        let error = anyhow::Error::from(error);
        close_queued_check(&writer, &request, &queue_link, &error).await;
        return Err(error);
    }
    info!(run = %run, commit = %commit.short(), "review started");
    let _alive = KeepAlive::start(Arc::clone(&app.store), &app.live_runs, run.clone());
    request_stages(app, &run, &request).await;

    if let Some((comment_id, is_review_comment)) = &request.acknowledge
        && let Err(error) = writer
            .acknowledge(&request.target, comment_id, *is_review_comment)
            .await
    {
        warn!(%error, "could not react to the request comment");
    }

    let handle = open_check(app, &writer, &request, &commit, &run, &link).await;
    started_stage(app, &run, handle.as_ref(), &commit).await;

    let pushed_head = Mutex::new(None);
    let review = ReviewRun {
        app,
        writer: &writer,
        target: &request.target,
        commit: &commit,
        run: &run,
        link: &link,
        posts: &AtomicBool::new(posts_comments(app)),
        pushed_head: &pushed_head,
    };
    let result = review_body(review, &info.title, &info.base_ref, cancel).await;

    match result {
        Ok((outcome, summary)) => {
            let status = if outcome.completed() {
                RunStatus::Finished
            } else {
                RunStatus::Failed
            };
            let closed = finish_checks(review, handle.as_ref(), &outcome).await;
            if let Err(error) = &closed {
                error!(%error, "could not finish the check");
            }
            let completed = outcome.completed();
            end_stages(app, &run, closed.is_ok(), handle.as_ref(), completed).await;
            app.store
                .finish_run(&run, status, Some(&summary), None)
                .await?;
            info!(run = %run, open = outcome.open_findings, "review ended");
            Ok(ReviewReport {
                run,
                outcome: Some(outcome),
                summary: Some(summary),
            })
        }
        Err(error) if error.is::<Interrupted>() => {
            report_interrupted(review, handle.as_ref()).await?;
            Err(error)
        }
        Err(error) if error.is::<Superseded>() => {
            report_superseded(review, handle.as_ref()).await?;
            Err(error)
        }
        Err(error) if error.is::<CancelledBy>() => {
            let by = error
                .downcast_ref::<CancelledBy>()
                .map_or_else(String::new, |c| c.0.clone());
            report_cancelled(review, handle.as_ref(), &by).await?;
            Err(error)
        }
        Err(error) => {
            report_failure(review, handle.as_ref(), &error).await?;
            Err(error)
        }
    }
}

/// The review's check, in progress: the queued one moved on (#262), or a
/// new one. Its id goes on the run. A platform that will not have it costs
/// the check, never the review; a queued check that could not be moved on
/// is still the review's, so its end completes it and it does not stay
/// queued (#262).
async fn open_check(
    app: &App,
    writer: &Arc<dyn PlatformWriter>,
    request: &ReviewRequest,
    commit: &CommitSha,
    run: &RunId,
    link: &str,
) -> Option<ReviewHandle> {
    let handle = match writer
        .start_review(&request.target, commit, link, request.queued_check.as_ref())
        .await
    {
        Ok(handle) => handle,
        Err(error) => {
            warn!(%error, "could not mark the review as started");
            request.queued_check.clone()
        }
    };
    if let Some(ReviewHandle(check_id)) = &handle
        && let Err(error) = app.store.set_check(run, check_id).await
    {
        warn!(%error, "could not store the check id");
    }
    handle
}

/// A review that waited with its check queued and then could not run
/// (#262), so the check does not stay queued. A pull request Henk does not
/// review closes it neutral with why; anything else, such as a read that
/// failed, is Henk's failure and closes it as one (§3.3), with the error in
/// the log only. Without a queued check nothing was shown, and nothing is.
async fn close_queued_check(
    writer: &Arc<dyn PlatformWriter>,
    request: &ReviewRequest,
    link: &str,
    error: &anyhow::Error,
) {
    let (Some(handle), Some(commit)) = (&request.queued_check, &request.commit) else {
        return;
    };
    let outcome = if let Some(NotReviewable(why)) = error.downcast_ref::<NotReviewable>() {
        ReviewOutcome::not_reviewed(commit.clone(), why.clone())
    } else {
        error!(error = %format!("{error:#}"), "a queued review could not start");
        ReviewOutcome {
            commit: commit.clone(),
            lanes: Vec::new(),
            open_findings: 0,
            nothing_to_review: false,
            stopped: None,
            not_reviewed: None,
        }
    };
    if let Err(finish_error) = writer
        .finish_review(&request.target, commit, Some(handle), &outcome, link)
        .await
    {
        error!(%finish_error, "could not close the queued check");
    }
}

async fn preflight(
    app: &App,
    writer: &Arc<dyn PlatformWriter>,
    request: &ReviewRequest,
) -> anyhow::Result<(henk_platform::PullRequestInfo, CommitSha)> {
    let platform = request.target.platform();
    if !app.settings.allowlist.allows(&request.target.repo) {
        return Err(
            NotReviewable(format!("{} is not on the allowlist", request.target.repo)).into(),
        );
    }
    if app.settings.lanes.is_empty() && app.settings.review.r#loop.is_none() {
        return Err(NotReviewable("no review lanes are configured".to_owned()).into());
    }

    let info = writer
        .pull_request(&request.target)
        .await
        .context("reading the pull request")?;
    if info.state != PullRequestState::Open {
        return Err(NotReviewable(format!(
            "pull request #{} is not open",
            request.target.number
        ))
        .into());
    }
    if info.draft && platform == Platform::GitHub && !app.settings.review.github_drafts {
        return Err(NotReviewable(format!(
            "pull request #{} is a draft; drafts are not reviewed",
            request.target.number
        ))
        .into());
    }
    let commit = request.commit.clone().unwrap_or_else(|| info.head.clone());

    Ok((info, commit))
}

/// A newer commit arrived: no comment, a neutral check, and the run ends
/// `superseded`, naming the run that replaced it when that is known, with
/// the reason (§3.3, #231). Not Henk's failure, so nothing says it is.
async fn report_superseded(
    review: ReviewRun<'_>,
    handle: Option<&henk_platform::ReviewHandle>,
) -> anyhow::Result<()> {
    let ReviewRun {
        app, commit, run, ..
    } = review;
    info!(run = %run, "review superseded by a newer commit");
    let outcome = ReviewOutcome::superseded(commit.clone());
    if let Err(finish_error) = finish_checks(review, handle, &outcome).await {
        error!(%finish_error, "could not finish the check of a superseded review");
    }
    let reason = Superseded.to_string();
    let newer = app.cancels.superseded_by(run).map_or_else(
        || "superseded by a newer commit".to_owned(),
        |by| format!("superseded by {by}"),
    );
    stages::end(
        &*app.store,
        run,
        StageState::Skipped,
        &newer,
        "a newer commit arrived",
    )
    .await;
    match app.cancels.superseded_by(run) {
        Some(by) => app.store.supersede_run(run, &by, &reason).await?,
        None => {
            app.store
                .finish_run(run, RunStatus::Superseded, None, Some(&reason))
                .await?;
        }
    }
    Ok(())
}

/// A person cancelled the review from the dashboard (#69): one comment
/// saying who, a neutral check, and the run ends `cancelled`. Not Henk's
/// failure, so nothing says it is.
async fn report_cancelled(
    review: ReviewRun<'_>,
    handle: Option<&henk_platform::ReviewHandle>,
    by: &str,
) -> anyhow::Result<()> {
    let ReviewRun {
        app,
        writer,
        target,
        commit,
        run,
        link,
        ..
    } = review;
    info!(run = %run, by, "review cancelled from the dashboard");
    let body = Marker {
        run: run.clone(),
        model: ModelId::parse("none").unwrap_or_else(|_| unreachable!("constant")),
        requested_by: None,
        kind: Some(MarkerKind::Reply),
        checked_by: None,
        withdrawn: None,
    }
    .attach(&crate::cancel::cancelled_notice(by, link, false));
    if review.posts.load(Ordering::Relaxed)
        && let Err(post_error) = writer.post_comment(target, &body).await
    {
        error!(%post_error, "could not post that the review was cancelled");
    }
    let outcome = ReviewOutcome::cancelled(commit.clone());
    if let Err(finish_error) = finish_checks(review, handle, &outcome).await {
        error!(%finish_error, "could not finish the check of a cancelled review");
    }
    let cancelled = format!("cancelled by {by}");
    stages::end(
        &*app.store,
        run,
        StageState::Skipped,
        &cancelled,
        &cancelled,
    )
    .await;
    app.store
        .finish_run(
            run,
            RunStatus::Cancelled,
            None,
            Some(&CancelledBy(by.to_owned()).to_string()),
        )
        .await?;
    Ok(())
}

/// A person cancelled a review from the dashboard while it waited for a
/// slot (#69). It never started; its run is still recorded and ends
/// `cancelled`, so the run page the dashboard links to exists and says
/// who, and one comment says so. Its queued check, if it has one, closes
/// as cancelled (#262), also when the run could not be recorded.
///
/// # Errors
///
/// Returns an error when the run could not be recorded.
pub async fn report_cancelled_while_queued(
    app: &App,
    request: &ReviewRequest,
    by: &str,
) -> anyhow::Result<()> {
    let run = request.run.clone().unwrap_or_else(new_run_id);
    let link = app.settings.run_link(&run);
    let platform = request.target.platform();
    let created = app
        .store
        .create_run(&NewRun {
            id: run.clone(),
            kind: RunKind::Review,
            platform,
            repo: request.target.repo.path(),
            target: request.target.number,
            commit: request.commit.as_ref().map(|c| c.as_str().to_owned()),
            requester: request.requester.clone(),
            trigger: request.trigger.clone(),
            link: link.clone(),
        })
        .await;
    if let Err(error) = created {
        // No run page: the check links where the queue is shown.
        match app.writer(platform) {
            Ok(writer) => {
                close_queued_as_cancelled(&writer, request, &app.settings.queue_link(&run)).await;
            }
            Err(writer_error) => error!(%writer_error, "could not close the queued check"),
        }
        return Err(error.into());
    }
    info!(run = %run, by, "review cancelled from the dashboard before it started");
    stages::request(
        &*app.store,
        &run,
        request.submitted_at,
        &request.trigger,
        request.requester.as_deref(),
    )
    .await;
    let cancelled = format!("cancelled by {by} while waiting for a slot");
    // It never got a slot: the queue stage ends skipped, not done.
    stages::mark_span(
        &*app.store,
        &run,
        Stage::Queued,
        StageState::Skipped,
        &cancelled,
        request
            .submitted_at
            .unwrap_or_else(time::OffsetDateTime::now_utc),
        None,
    )
    .await;
    stages::end(
        &*app.store,
        &run,
        StageState::Skipped,
        &cancelled,
        &cancelled,
    )
    .await;
    match app.writer(platform) {
        Ok(writer) => {
            let body = Marker {
                run: run.clone(),
                model: ModelId::parse("none").unwrap_or_else(|_| unreachable!("constant")),
                requested_by: None,
                kind: Some(MarkerKind::Reply),
                checked_by: None,
                withdrawn: None,
            }
            .attach(&crate::cancel::cancelled_notice(by, &link, false));
            if posts_comments(app)
                && let Err(post_error) = writer.post_comment(&request.target, &body).await
            {
                error!(%post_error, "could not post that the review was cancelled");
            }
            close_queued_as_cancelled(&writer, request, &link).await;
        }
        Err(writer_error) => {
            error!(%writer_error, "could not post that the review was cancelled");
        }
    }
    app.store
        .finish_run(
            &run,
            RunStatus::Cancelled,
            None,
            Some(&CancelledBy(by.to_owned()).to_string()),
        )
        .await?;
    Ok(())
}

/// A queued check of a review cancelled before it started closes as
/// cancelled, as a running one's does (#262).
async fn close_queued_as_cancelled(
    writer: &Arc<dyn PlatformWriter>,
    request: &ReviewRequest,
    link: &str,
) {
    if let (Some(handle), Some(commit)) = (&request.queued_check, &request.commit) {
        let outcome = ReviewOutcome::cancelled(commit.clone());
        if let Err(finish_error) = writer
            .finish_review(&request.target, commit, Some(handle), &outcome, link)
            .await
        {
            error!(%finish_error, "could not close the queued check");
        }
    }
}

/// Henk was told to stop: the check completes as interrupted, the run
/// ends `failed`, and nothing is posted; the next review posts (§3.3).
async fn report_interrupted(
    review: ReviewRun<'_>,
    handle: Option<&henk_platform::ReviewHandle>,
) -> anyhow::Result<()> {
    let ReviewRun {
        app, commit, run, ..
    } = review;
    warn!(run = %run, "review interrupted");
    let outcome = ReviewOutcome::interrupted(commit.clone());
    if let Err(finish_error) = finish_checks(review, handle, &outcome).await {
        error!(%finish_error, "could not finish the check of an interrupted review");
    }
    stages::end(
        &*app.store,
        run,
        StageState::Failed,
        "interrupted",
        "interrupted",
    )
    .await;
    app.store
        .finish_run(run, RunStatus::Failed, None, Some(&Interrupted.to_string()))
        .await?;
    Ok(())
}

async fn report_failure(
    review: ReviewRun<'_>,
    handle: Option<&henk_platform::ReviewHandle>,
    error: &anyhow::Error,
) -> anyhow::Result<()> {
    let ReviewRun {
        app,
        writer,
        target,
        commit,
        run,
        link,
        ..
    } = review;
    let message = format!("{error:#}");
    error!(error = %message, "review failed");
    let failed = ReviewOutcome {
        commit: commit.clone(),
        lanes: Vec::new(),
        open_findings: 0,
        nothing_to_review: false,
        stopped: None,
        not_reviewed: None,
    };
    let body = Marker {
        run: run.clone(),
        model: ModelId::parse("none").unwrap_or_else(|_| unreachable!("constant")),
        requested_by: None,
        kind: Some(MarkerKind::Failure),
        checked_by: None,
        withdrawn: None,
    }
    .attach(&format!(
        "Review did not complete. That is my failure, not the code's.\n\nRun: {link}"
    ));
    if review.posts.load(Ordering::Relaxed)
        && let Err(post_error) = writer.post_comment(target, &body).await
    {
        error!(%post_error, "could not post the failure comment");
    }
    if let Err(finish_error) = finish_checks(review, handle, &failed).await {
        error!(%finish_error, "could not finish the check after failure");
    }
    stages::failed(&*app.store, run).await;
    app.store
        .finish_run(run, RunStatus::Failed, None, Some(&message))
        .await?;
    Ok(())
}

async fn review_body(
    review: ReviewRun<'_>,
    title: &str,
    base_ref: &str,
    cancel: CancellationToken,
) -> anyhow::Result<(ReviewOutcome, String)> {
    if review.app.settings.review.r#loop.is_some() {
        return loop_body(review, title, base_ref, cancel).await;
    }
    let store = &*review.app.store;
    stages::mark(store, review.run, Stage::Diff, StageState::Running, "").await;
    let diff = fetch_diff(review, base_ref).await?;
    lanes_body(review, title, base_ref, cancel, diff, None).await
}

/// The lane review of `diff`: the lanes post their findings on the pull
/// request, and the summary goes on it too, with `note` after it.
async fn lanes_body(
    review: ReviewRun<'_>,
    title: &str,
    base_ref: &str,
    cancel: CancellationToken,
    diff: Arc<ReviewDiff>,
    note: Option<String>,
) -> anyhow::Result<(ReviewOutcome, String)> {
    let ReviewRun {
        writer,
        target,
        commit,
        run,
        link,
        ..
    } = review;

    // Seed the shared registry from what is already on the pull request.
    let existing = writer
        .existing_findings(target)
        .await
        .context("listing existing findings")?;
    let registry = Arc::new(Mutex::new(FindingRegistry::seeded(existing.iter().map(
        |f| Finding {
            key: FindingKey {
                path: f.path.clone(),
                line: f.line.unwrap_or(0),
            },
            comment_id: f.comment_id.clone(),
            body: f.body.clone(),
            lane: None,
            answered_by_person: f.answered_by_person,
            resolved: f.resolved,
            in_diff: f.line.is_some(),
        },
    ))));

    let store = &*review.app.store;
    // Every changed file left out by review.ignore: nothing for a lane to
    // read. The review still counts, folds and summarises (§3.2).
    let nothing_to_review = diff.is_empty();
    let results = if nothing_to_review {
        for stage in [Stage::Checkout, Stage::Lanes, Stage::FactCheck] {
            stages::mark(store, run, stage, StageState::Skipped, "nothing to review").await;
        }
        Vec::new()
    } else {
        run_lanes(review, registry, diff, title, base_ref, &cancel).await?
    };
    stop_if_ended(review, &cancel)?;

    // The count comes from the platform, not from memory (§3.3).
    let after = writer
        .existing_findings(target)
        .await
        .context("re-listing findings")?;
    let open_findings = after
        .iter()
        .filter(|f| f.line.is_some() && !f.resolved)
        .count();
    let outcome = ReviewOutcome {
        commit: commit.clone(),
        lanes: results,
        open_findings,
        nothing_to_review,
        stopped: None,
        not_reviewed: None,
    };

    fold_outdated(writer, target, &after).await;

    let mut summary_text = outcome.summary();
    if let Some(note) = note {
        summary_text = format!("{summary_text} {note}");
    }
    let body = Marker {
        run: run.clone(),
        model: ModelId::parse("orchestrator").unwrap_or_else(|_| unreachable!("constant")),
        requested_by: None,
        kind: Some(MarkerKind::Summary),
        checked_by: None,
        withdrawn: None,
    }
    .attach(&format!("{summary_text}\n\nRun: {link}"));
    stages::mark(store, run, Stage::Publish, StageState::Running, "").await;
    writer
        .post_comment(target, &body)
        .await
        .context("posting the summary")?;
    let published = published_line(store, run).await;
    stages::mark(store, run, Stage::Publish, StageState::Done, published).await;
    Ok((outcome, summary_text))
}

/// The error a cancelled review ends with: who cancelled it, a shutdown, or
/// a newer review that superseded it.
fn stop_if_ended(review: ReviewRun<'_>, cancel: &CancellationToken) -> anyhow::Result<()> {
    if !cancel.is_cancelled() {
        return Ok(());
    }
    if let Some(by) = review.app.cancels.cancelled_by(review.run) {
        return Err(CancelledBy(by).into());
    }
    // A shutdown cancels every review; superseding cancels only one.
    Err(if review.app.shutdown.is_cancelled() {
        Interrupted.into()
    } else {
        Superseded.into()
    })
}

/// A review as a reviewer↔fixer loop (#284, #285): the reviewer's findings
/// go to the fixer, never to the pull request. Nothing is posted; what the
/// loop did is on the run, and its summary goes to the check. A pull request
/// the loop may not push to (a fork, the default or a protected branch) or
/// whose branch moved gets the lane review instead, as without the loop; the
/// line for the summary says which ran.
async fn loop_body(
    review: ReviewRun<'_>,
    title: &str,
    base_ref: &str,
    cancel: CancellationToken,
) -> anyhow::Result<(ReviewOutcome, String)> {
    let ReviewRun {
        app,
        target,
        commit,
        run,
        ..
    } = review;
    let store = &*app.store;
    stages::mark(store, run, Stage::Diff, StageState::Running, "").await;
    let diff = fetch_diff(review, base_ref).await?;
    let nothing_to_review = diff.is_empty();
    let (results, open_findings, line) = if nothing_to_review {
        for stage in [Stage::Checkout, Stage::Lanes, Stage::FactCheck] {
            stages::mark(store, run, stage, StageState::Skipped, "nothing to review").await;
        }
        (Vec::new(), 0, None)
    } else {
        let on = crate::review_loop::LoopTarget {
            app,
            target,
            commit,
            run,
            pushed_head: review.pushed_head,
        };
        match crate::review_loop::run_loop(on, Arc::clone(&diff), &cancel).await? {
            crate::review_loop::LoopRun::Ran(report) => {
                (report.results, report.open, Some(report.line))
            }
            crate::review_loop::LoopRun::Declined(why) => {
                info!(run = %run, %why, "the review loop did not run");
                if !app.settings.lanes.is_empty() {
                    let note =
                        format!("The review loop did not run: {why}. The lanes reviewed instead.");
                    review.posts.store(true, Ordering::Relaxed);
                    return lanes_body(review, title, base_ref, cancel, diff, Some(note)).await;
                }
                for stage in [Stage::Checkout, Stage::Lanes, Stage::FactCheck] {
                    stages::mark(store, run, stage, StageState::Skipped, &why).await;
                }
                let line = format!(
                    "The review loop did not run: {why}. No lanes are configured to review instead."
                );
                (Vec::new(), 0, Some(line))
            }
        }
    };
    stop_if_ended(review, &cancel)?;
    let outcome = ReviewOutcome {
        commit: commit.clone(),
        lanes: results,
        open_findings,
        nothing_to_review,
        stopped: None,
        not_reviewed: None,
    };
    let mut summary_text = outcome.summary();
    if let Some(line) = line {
        summary_text = format!("{summary_text} {line}");
    }
    let skipped = "a review loop posts nothing on the pull request";
    stages::mark(store, run, Stage::Publish, StageState::Skipped, skipped).await;
    Ok((outcome, summary_text))
}

/// Whether a review posts comments on the pull request: not as a review
/// loop, whose findings go to the fixer and whose record is the run (#285).
fn posts_comments(app: &App) -> bool {
    app.settings.review.r#loop.is_none()
}

/// Closes the review's check on its commit with `outcome`, and, when a
/// review loop pushed (#284), reports the same outcome on the last commit
/// it pushed: the pull request's head now, which this run reviewed and
/// which starts no other review, so it does not wait for a check. That one
/// is best effort and logged; the result is the review's own check.
async fn finish_checks(
    review: ReviewRun<'_>,
    handle: Option<&ReviewHandle>,
    outcome: &ReviewOutcome,
) -> Result<(), henk_platform::PlatformError> {
    let ReviewRun {
        writer,
        target,
        commit,
        link,
        pushed_head,
        ..
    } = review;
    let closed = writer
        .finish_review(target, commit, handle, outcome, link)
        .await;
    let head = pushed_head.lock().ok().and_then(|head| head.clone());
    if let Some(head) = head.filter(|head| head != commit) {
        let reported = match writer.start_review(target, &head, link, None).await {
            Ok(started) => {
                writer
                    .finish_review(target, &head, started.as_ref(), outcome, link)
                    .await
            }
            Err(error) => Err(error),
        };
        if let Err(error) = reported {
            warn!(%error, head = %head.short(), "could not report the check on the loop's head");
        }
    }
    closed
}

/// The review's request and queue stages, from the coordinator's submit time.
async fn request_stages(app: &App, run: &RunId, request: &ReviewRequest) {
    let requester = request.requester.as_deref();
    let submitted = request.submitted_at;
    stages::requested(&*app.store, run, submitted, &request.trigger, requester).await;
}

/// The started stage: the check it opened, when it did.
async fn started_stage(app: &App, run: &RunId, handle: Option<&ReviewHandle>, commit: &CommitSha) {
    let started = match handle {
        Some(ReviewHandle(check_id)) => format!("check {check_id} opened"),
        None => format!("review of {} started", commit.short()),
    };
    stages::mark(&*app.store, run, Stage::Started, StageState::Done, started).await;
}

/// The done stage of a review that ran to its end: the check closed (or
/// not), done when the review completed and failed when it did not.
async fn end_stages(
    app: &App,
    run: &RunId,
    closed: bool,
    handle: Option<&ReviewHandle>,
    completed: bool,
) {
    let done = match (closed, handle) {
        (false, _) => "the check could not be closed".to_owned(),
        (true, Some(ReviewHandle(check_id))) => format!("check {check_id} closed"),
        (true, None) => "ended".to_owned(),
    };
    let state = if completed {
        StageState::Done
    } else {
        StageState::Failed
    };
    stages::end(&*app.store, run, state, &done, "the review ended first").await;
}

/// What the review wrote on the pull request, in a line: `2 line comments,
/// 1 improved, and the summary`.
async fn published_line(store: &dyn henk_store::RunStore, run: &RunId) -> String {
    let findings = store.findings(run).await.unwrap_or_default();
    let count = |action: FindingAction| {
        findings
            .iter()
            .filter(|f| f.action == action.as_str())
            .count()
    };
    let mut parts = Vec::new();
    let posted = count(FindingAction::Posted);
    if posted > 0 {
        parts.push(format!(
            "{posted} line {}",
            if posted == 1 { "comment" } else { "comments" }
        ));
    }
    for (action, word) in [
        (FindingAction::Improved, "improved"),
        (FindingAction::Withdrawn, "withdrawn"),
    ] {
        let n = count(action);
        if n > 0 {
            parts.push(format!("{n} {word}"));
        }
    }
    if parts.is_empty() {
        "the summary".to_owned()
    } else {
        format!("{} and the summary", parts.join(", "))
    }
}

/// Runs every configured lane on the diff, sharing one read session, and
/// returns how each ended, in lane-name order.
async fn run_lanes(
    review: ReviewRun<'_>,
    registry: Arc<Mutex<FindingRegistry>>,
    diff: Arc<ReviewDiff>,
    title: &str,
    base_ref: &str,
    cancel: &CancellationToken,
) -> anyhow::Result<Vec<LaneResult>> {
    let ReviewRun {
        app,
        writer,
        target,
        commit,
        run,
        ..
    } = review;
    let platform = target.platform();

    // One read-only MCP session shared by the lanes of this review.
    let session = app.read_session(platform).await?;
    let scope = Scope::Review {
        repo: target.repo.clone(),
        number: target.number,
        commit: commit.clone(),
    };
    let excluded = withheld_tools(review, &session, &scope).await;
    let mut lanes = LaneInputs {
        session,
        scope,
        registry,
        drafts: Arc::new(Mutex::new(henk_domain::draft::DraftBook::new())),
        diff,
        excluded,
        title: title.to_owned(),
        base_ref: base_ref.to_owned(),
        cancel: cancel.clone(),
        limits: henk_domain::workspace::Limits::default(),
    };
    // Each lane's own workspace, when the profile reviews in one (#170),
    // and the fact-checker's. Lanes close theirs when they end; the rest
    // closes once all have. On an early error they are dropped, which
    // destroys them too.
    let mut workspaces = ReviewWorkspaces::open(app, target, commit, run, cancel).await;
    lanes.limits = workspaces.limits();
    let fact_check = build_fact_check(review, &lanes, workspaces.fact_check()).await?;
    let store = &*app.store;
    let configured = app.settings.lanes.len();
    stages::mark(
        store,
        run,
        Stage::Lanes,
        StageState::Running,
        format!(
            "{configured} {}",
            if configured == 1 { "lane" } else { "lanes" }
        ),
    )
    .await;
    let mut set = spawn_lanes(review, &lanes, &mut workspaces).await?;

    let mut results = Vec::new();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(result) => results.push(result),
            Err(error) => {
                error!(%error, "a lane task panicked");
                results.push(LaneResult {
                    lane: henk_domain::review::LaneName::new("unknown"),
                    outcome: LaneOutcome::Dropped,
                });
            }
        }
    }
    stages::mark(
        store,
        run,
        Stage::Lanes,
        lanes_state(&results),
        lanes_line(&results),
    )
    .await;
    // Every lane has ended: check what they drafted, together, and write
    // what holds (#189). Nothing was written before this.
    let book = lanes
        .drafts
        .lock()
        .map(|book| book.clone())
        .unwrap_or_default();
    let writes = crate::drafts::ReviewWrites {
        run: run.clone(),
        target: target.clone(),
        commit: commit.clone(),
        writer: Arc::clone(writer),
        store: Arc::clone(&app.store),
        registry: Arc::clone(&lanes.registry),
    };
    check_and_write(
        review,
        fact_check.as_ref(),
        &book,
        &lanes.registry,
        &writes,
        cancel,
    )
    .await;
    workspaces.close_all().await;
    results.sort_by(|a, b| a.lane.as_str().cmp(b.lane.as_str()));
    Ok(results)
}

/// Checks the lanes' drafts, together, and writes what holds (#189), with
/// the fact-check and publish stages (#226). Nothing was written before.
async fn check_and_write(
    review: ReviewRun<'_>,
    fact_check: Option<&FactChecker>,
    book: &henk_domain::draft::DraftBook,
    registry: &Arc<Mutex<FindingRegistry>>,
    writes: &crate::drafts::ReviewWrites,
    cancel: &CancellationToken,
) {
    let (store, run) = (&*review.app.store, review.run);
    if book.is_empty() {
        stages::mark(
            store,
            run,
            Stage::FactCheck,
            StageState::Skipped,
            "no drafts",
        )
        .await;
    } else {
        let verdicts = match fact_check {
            _ if cancel.is_cancelled() => None,
            Some(checker) => {
                stages::mark(store, run, Stage::FactCheck, StageState::Running, "").await;
                let verdicts = checker
                    .check_all(book, &crate::drafts::open_findings(registry))
                    .await;
                stages::mark(
                    store,
                    run,
                    Stage::FactCheck,
                    StageState::Done,
                    stages::fact_check_line(&verdicts),
                )
                .await;
                Some(verdicts)
            }
            None => {
                stages::mark(
                    store,
                    run,
                    Stage::FactCheck,
                    StageState::Skipped,
                    "no fact-check configured",
                )
                .await;
                Some(
                    book.iter()
                        .map(|d| (d.id, henk_domain::draft::Verdict::NoCheck))
                        .collect(),
                )
            }
        };
        match verdicts {
            Some(verdicts) if !cancel.is_cancelled() => {
                stages::mark(store, run, Stage::Publish, StageState::Running, "").await;
                crate::drafts::write_all(writes, book, &verdicts).await;
            }
            _ => crate::drafts::cancel_all(writes, book).await,
        }
    }
}

/// How the lanes ended, in a line: `2 of 3 finished, 1 did not finish`.
fn lanes_line(results: &[LaneResult]) -> String {
    let count = |outcome: LaneOutcome| results.iter().filter(|r| r.outcome == outcome).count();
    let mut line = format!(
        "{} of {} finished",
        count(LaneOutcome::Finished),
        results.len()
    );
    let stopped = count(LaneOutcome::Stopped);
    if stopped > 0 {
        let _ = write!(line, ", {stopped} timed out");
    }
    let dropped = count(LaneOutcome::Dropped);
    if dropped > 0 {
        let _ = write!(line, ", {dropped} did not finish");
    }
    line
}

/// The lanes stage failed only when no lane ran to an end: the review then
/// stands on nothing.
fn lanes_state(results: &[LaneResult]) -> StageState {
    if results.iter().any(|r| r.outcome != LaneOutcome::Dropped) {
        StageState::Done
    } else {
        StageState::Failed
    }
}

/// Server tools the lanes should not see on this repository. GitHub code
/// search indexes only some repositories; when one probe returns nothing,
/// the tool is withheld so lanes do not spend turns on empty answers.
async fn withheld_tools(
    review: ReviewRun<'_>,
    session: &Arc<dyn McpSession>,
    scope: &Scope,
) -> Vec<&'static str> {
    let ReviewRun {
        app, target, run, ..
    } = review;
    let platform = target.platform();
    if platform != Platform::GitHub {
        return Vec::new();
    }
    let Ok(tools) = platform_tools(Arc::clone(session), platform, scope.clone(), &[]).await else {
        return Vec::new();
    };
    let Some(search) = tools.iter().find(|t| t.server_tool() == "search_code") else {
        return Vec::new();
    };
    let output = search
        .call(serde_json::json!({"query": target.repo.name()}))
        .await;
    let total = serde_json::from_str::<serde_json::Value>(&output.content)
        .ok()
        .and_then(|v| v.get("total_count").and_then(serde_json::Value::as_u64));
    match (output.is_error, total) {
        (false, Some(0)) => {
            info!("code search returns nothing for this repository; withheld from lanes");
            let _ = app.store.event(
                run,
                "info",
                "code search returns nothing for this repository (not indexed); search_code withheld from lanes",
            ).await;
            vec!["search_code"]
        }
        _ => Vec::new(),
    }
}

/// The diff, once, for every lane: handed out per file, and the check every
/// finding passes before it is posted. Without it there is no review: the
/// error ends the run as Henk's own failure, which the summary says.
async fn fetch_diff(review: ReviewRun<'_>, base_ref: &str) -> anyhow::Result<Arc<ReviewDiff>> {
    let ReviewRun {
        app,
        writer,
        target,
        commit,
        run,
        ..
    } = review;
    let patches = writer
        .diff(target, commit, base_ref)
        .await
        .context("fetching the diff")?;
    let ignore = PathFilter::new(app.settings.review.ignore.iter().map(String::as_str));
    let diff = ReviewDiff::from_patches_filtered(&patches, &ignore);
    if !diff.has_changes() {
        return Err(anyhow!("the diff is empty; nothing to review"));
    }
    let (additions, deletions) = diff
        .files()
        .iter()
        .fold((0, 0), |(a, d), f| (a + f.additions, d + f.deletions));
    let numbers = format!(
        "{} files, +{additions} -{deletions}; {} not reviewed",
        diff.files().len(),
        diff.ignored().len()
    );
    let _ = app
        .store
        .event(run, "info", &format!("diff: {numbers} (review.ignore)"))
        .await;
    stages::mark(&*app.store, run, Stage::Diff, StageState::Done, numbers).await;
    Ok(Arc::new(diff))
}

async fn spawn_lanes(
    review: ReviewRun<'_>,
    lanes: &LaneInputs,
    workspaces: &mut ReviewWorkspaces,
) -> anyhow::Result<JoinSet<LaneResult>> {
    let ReviewRun { app, run, .. } = review;
    let mut set = JoinSet::new();
    for lane in &app.settings.lanes {
        let workspace = workspaces.take_lane(&lane.name);
        let spec = build_lane(review, lanes, lane, workspace).await?;
        let lane_name = lane.name.clone();
        let store = Arc::clone(&app.store);
        let run_id = run.clone();
        let cancel = lanes.cancel.clone();
        let context = Arc::clone(&spec.context);
        set.spawn(async move {
            let outcome = run_session(store.as_ref(), &run_id, spec.session, cancel).await;
            if let Some(workspace) = &context.workspace {
                workspace.close().await;
            }
            let unopened = context.unopened_files();
            if !unopened.is_empty() {
                let shown: Vec<&str> = unopened.iter().take(20).map(String::as_str).collect();
                let _ = store
                    .event(
                        &run_id,
                        "warn",
                        &format!(
                            "{lane_name}: never asked the diff of {} changed file(s): {}",
                            unopened.len(),
                            shown.join(", ")
                        ),
                    )
                    .await;
            }
            // The lane row (henk-session) says finished for a time limit;
            // the summary distinguishes it as stopped, from `stop`. A model
            // that declined (`StopCause::Refused`) or kept repeating one
            // tool call (`StopCause::Stuck`) is a dropped lane: the summary
            // names it instead of reading as a clean review (#40).
            let lane_outcome = match outcome.stop {
                StopCause::Timeout => LaneOutcome::Stopped,
                _ if outcome.finished() => LaneOutcome::Finished,
                _ => LaneOutcome::Dropped,
            };
            LaneResult {
                lane: lane_name,
                outcome: lane_outcome,
            }
        });
    }
    Ok(set)
}

async fn fold_outdated(
    writer: &Arc<dyn PlatformWriter>,
    target: &ReviewTarget,
    findings: &[henk_platform::ExistingFinding],
) {
    match writer.existing_summaries(target).await {
        Ok(summaries) => {
            for summary in summaries {
                if let Err(error) = writer.fold_summary(target, &summary).await {
                    warn!(%error, comment = %summary.comment_id, "could not fold an earlier summary");
                }
            }
        }
        Err(error) => warn!(%error, "could not list earlier summaries"),
    }
    for finding in findings.iter().filter(|f| f.resolved) {
        if let Err(error) = writer.fold_finding(target, finding).await {
            warn!(%error, comment = %finding.comment_id, "could not fold a resolved finding");
        }
    }
}

/// "pull request" or "merge request".
fn kind_name(platform: Platform) -> &'static str {
    match platform {
        Platform::GitHub => "pull request",
        Platform::GitLab => "merge request",
    }
}

/// "#7" or "!7".
fn target_ref(platform: Platform, number: u64) -> String {
    match platform {
        Platform::GitHub => format!("#{number}"),
        Platform::GitLab => format!("!{number}"),
    }
}

/// A review lane's session limits, from the settings.
fn lane_limits(settings: &crate::config::Settings) -> AgentConfig {
    AgentConfig {
        max_turns: settings.review.lane_max_turns,
        timeout: Duration::from_secs(settings.review.lane_timeout_secs),
        max_conversation_chars: settings.review.max_conversation_chars,
        keep_recent_turns: settings.review.keep_recent_turns,
        max_repeated_calls: settings.agent.max_repeated_calls,
        record_argument_bytes: settings.agent.record_argument_bytes,
        ..AgentConfig::default()
    }
}

async fn build_lane(
    review: ReviewRun<'_>,
    lanes: &LaneInputs,
    lane: &LaneSpec,
    workspace: Option<Arc<dyn crate::workspace::Workspace>>,
) -> anyhow::Result<Lane> {
    let ReviewRun {
        app,
        target,
        commit,
        run,
        ..
    } = review;
    let platform = target.platform();
    let model = app.model(lane.model.as_str())?;
    let tools = platform_tools(
        Arc::clone(&lanes.session),
        platform,
        lanes.scope.clone(),
        &lanes.excluded,
    )
    .await
    .context("listing MCP tools")?;
    if tools.is_empty() {
        return Err(anyhow!(
            "the MCP server exposes none of the tools a review needs"
        ));
    }
    // The platform's whole-file read is wrapped by read_file (a numbered
    // line range); every other read tool is exposed as it is. With a
    // workspace, read_file and the other code tools read that instead (#90).
    let mut set = ToolSet::new();
    let mut file_reader: Option<Arc<dyn henk_agent::Tool>> = None;
    for tool in tools {
        if tool.server_tool() == "get_file_contents" {
            file_reader = Some(Arc::new(tool));
        } else {
            set.add(tool);
        }
    }
    let context = Arc::new(LaneContext {
        run: run.clone(),
        lane: lane.name.clone(),
        model: model_id(model.as_ref()),
        registry: Arc::clone(&lanes.registry),
        drafts: Arc::clone(&lanes.drafts),
        store: Arc::clone(&app.store),
        files: Arc::new(DiffFiles::new(Arc::clone(&lanes.diff))),
        workspace,
    });
    set.add(ListChangedFiles(Arc::clone(&context.files)));
    set.add(GetFileDiff(Arc::clone(&context.files)));
    match (&context.workspace, file_reader) {
        (Some(workspace), _) => {
            crate::code_tools::add(&mut set, workspace);
            crate::code_tools::add_bash(
                &mut set,
                workspace,
                Duration::from_secs(lanes.limits.command_secs),
                crate::code_tools::BashUse::Review,
            );
        }
        (None, Some(inner)) => {
            set.add(ReadFile { inner });
        }
        (None, None) => {}
    }
    set.add(ListExistingFindings(Arc::clone(&context)));
    set.add(PostFinding(Arc::clone(&context)));
    set.add(ImproveFinding(Arc::clone(&context)));
    set.add(WithdrawFinding(Arc::clone(&context)));

    let mut system = lane_system(target, commit, lanes, context.workspace.is_some());
    crate::skill_tools::equip(
        &mut system,
        &mut set,
        app.settings.skills.select(&lane.skills),
    );
    let limits = lane_limits(&app.settings);
    let opening = lane_opening(target, commit, &context.files.diff);
    Ok(Lane {
        session: SessionSpec {
            name: lane.name.as_str().to_owned(),
            model,
            system,
            opening: vec![opening],
            tools: set,
            limits,
            continuation: Some(lane_continuation(Arc::clone(&context))),
            turn_warning: Some(henk_agent::TurnWarning {
                turns_left: LANE_TURN_WARNING_AT,
                message: LANE_TURN_WARNING.to_owned(),
            }),
        },
        context,
    })
}

/// A lane's system prompt: the persona and the lane's instructions, and
/// what its workspace offers when it has one (#90).
fn lane_system(
    target: &ReviewTarget,
    commit: &CommitSha,
    lanes: &LaneInputs,
    workspace: bool,
) -> String {
    let platform = target.platform();
    let mut system = format!(
        "{}\n\n{}",
        prompts::PERSONA,
        prompts::render(
            prompts::REVIEW_LANE,
            &[
                ("kind", kind_name(platform)),
                ("ref", &target_ref(platform, target.number)),
                ("repo", &target.repo.path()),
                ("commit", commit.as_str()),
                ("base", &lanes.base_ref),
                ("title", &lanes.title),
            ],
        )
    );
    if workspace {
        system = format!("{system}\n\n{}", prompts::REVIEW_WORKSPACE);
    }
    system
}

/// A lane's first message. The file list up front saves a turn, and the
/// hint to read several diffs per call saves one per file for models that
/// never batch calls.
fn lane_opening(target: &ReviewTarget, commit: &CommitSha, diff: &ReviewDiff) -> ChatMessage {
    let platform = target.platform();
    ChatMessage::user(format!(
        "Review {} {} at commit {}. The changed files:\n{}\nRead their diffs with get_file_diff, several paths per call.",
        kind_name(platform),
        target_ref(platform, target.number),
        commit.short(),
        diff.render_list().trim_end()
    ))
}

/// The fact-check every lane's writes pass, when one is configured (§3.2).
/// It reads the same diff and the same guarded file read as the lanes.
async fn build_fact_check(
    review: ReviewRun<'_>,
    lanes: &LaneInputs,
    workspace: Option<Arc<dyn crate::workspace::Workspace>>,
) -> anyhow::Result<Option<FactChecker>> {
    let ReviewRun {
        app,
        target,
        commit,
        run,
        ..
    } = review;
    let LaneInputs {
        session,
        scope,
        diff,
        excluded,
        cancel,
        ..
    } = lanes;
    let Some(config) = &app.settings.review.fact_check else {
        return Ok(None);
    };
    let mut models = vec![app.model(&config.model)?];
    if let Some(backup) = &config.backup_model {
        models.push(app.model(backup)?);
    }
    let platform = target.platform();
    let file_reader = platform_tools(Arc::clone(session), platform, scope.clone(), excluded)
        .await
        .context("listing MCP tools for the fact-check")?
        .into_iter()
        .find(|tool| tool.server_tool() == "get_file_contents")
        .map(|tool| Arc::new(tool) as Arc<dyn henk_agent::Tool>);
    let mut system = prompts::render(
        prompts::FACT_CHECK,
        &[
            ("kind", kind_name(platform)),
            ("ref", &target_ref(platform, target.number)),
            ("repo", &target.repo.path()),
            ("commit", commit.as_str()),
        ],
    );
    if workspace.is_some() {
        system = format!("{system}\n\n{}\n\n{SHARED_COPY}", prompts::REVIEW_WORKSPACE);
    }
    Ok(Some(FactChecker {
        store: Arc::clone(&app.store),
        run: run.clone(),
        models,
        diff: Arc::clone(diff),
        file_reader,
        workspace,
        check_limits: lanes.limits.clone(),
        system,
        skills: app.settings.skills.select(&config.skills),
        limits: AgentConfig {
            max_turns: config.max_turns,
            timeout: Duration::from_secs(config.timeout_secs),
            max_conversation_chars: app.settings.review.max_conversation_chars,
            keep_recent_turns: app.settings.review.keep_recent_turns,
            max_repeated_calls: app.settings.agent.max_repeated_calls,
            record_argument_bytes: app.settings.agent.record_argument_bytes,
            ..AgentConfig::default()
        },
        cancel: cancel.clone(),
    }))
}

/// A lane ready to run: its session and the context its tools share, kept
/// so the orchestrator can read what the lane did.
struct Lane {
    session: SessionSpec,
    context: Arc<LaneContext>,
}

#[cfg(test)]
mod lifecycle_tests {
    //! `run_review` end to end on fakes: the writer, the MCP session and the
    //! model are in-process, so every path that ends a review is exercised
    //! without a network.

    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::unnecessary_wraps,
        clippy::too_many_lines
    )]

    use std::time::Duration;

    use henk_domain::allowlist::{Platform, RepoRef};
    use henk_domain::marker::{Marker, MarkerKind};
    use henk_domain::review::CheckConclusion;
    use henk_llm::testing::ScriptedClient;
    use henk_llm::{Completion, ModelClient, StopReason, Usage};
    use henk_mcp::testing::{FakeServer, echo_behaviour};
    use henk_store::LaneStatus;

    use super::*;
    use crate::config::Config;
    use crate::listeners::testing::FakeWriter;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    const DIFF: &str = "\
diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,3 +1,3 @@
 fn main() {
-    let x = 1;
+    let x = 2;
 }
";

    const CONFIG: &str = r#"
[discord]
channel_id = 1
henk_user_id = 2
team_lead_ids = [3]
[mail]
address = "henk@example.com"
[allowlist]
github_owners = ["o"]
[github]
app_id = 1
installation_id = 2
bot_login = "h[bot]"
[mcp.github]
command = "unused"
[models.m]
provider = "open_ai"
base_url = "https://x.test/v1"
api_key_env = "UNUSED"
model = "scripted"
[review]
lanes = [{ name = "lane-a", model = "m" }]
"#;

    fn done() -> Result<Completion, henk_llm::LlmError> {
        Ok(Completion {
            message: henk_llm::ChatMessage::assistant("Nothing to report."),
            stop: StopReason::EndTurn,
            usage: Usage::default(),
        })
    }

    struct Fixture {
        app: App,
        writer: Arc<FakeWriter>,
        /// The model every lane and check talks to, with what it was asked.
        model: Arc<ScriptedClient>,
    }

    /// An app whose writer, read session and model are fakes. `patches`
    /// is the diff the writer serves; `model` answers the lane.
    async fn fixture(diff: &str, model: ScriptedClient) -> Fixture {
        fixture_on(
            diff,
            model,
            CONFIG,
            SHA,
            Arc::new(crate::workspace::host::HostProvider),
            None,
        )
        .await
    }

    /// [`fixture`] with its own configuration, reviewed head, workspace
    /// backend and address writer (the review's way to the source, #170).
    async fn fixture_on(
        diff: &str,
        model: ScriptedClient,
        config: &str,
        head: &str,
        provider: Arc<dyn crate::workspace::WorkspaceProvider>,
        source: Option<Arc<dyn henk_platform::address::AddressWriter>>,
    ) -> Fixture {
        let settings = Config::parse(config)
            .and_then(Config::into_settings)
            .unwrap_or_else(|e| panic!("{e}"));
        let writer = Arc::new(FakeWriter {
            head: head.to_owned(),
            patches: henk_domain::diff::split_unified(diff),
            ..FakeWriter::default()
        });
        let server = FakeServer::new(
            vec![FakeServer::tool("get_commit", "", &["sha"])],
            echo_behaviour(),
        );
        let session: Arc<dyn McpSession> = Arc::new(server.connect("github").await);
        let model = Arc::new(model);
        let mut models: std::collections::BTreeMap<String, Arc<dyn ModelClient>> =
            std::collections::BTreeMap::new();
        models.insert("m".to_owned(), Arc::clone(&model) as Arc<dyn ModelClient>);
        // The store announces its writes, as Henk's own does (#202).
        let feed = crate::live::Feed::default();
        Fixture {
            app: App {
                settings,
                store: Arc::new(crate::live::Announcing::new(
                    Arc::new(henk_store::SqliteStore::in_memory().unwrap()),
                    feed.clone(),
                )),
                models,
                github: None,
                gitlab: None,
                shutdown: CancellationToken::new(),
                live_runs: crate::liveness::LiveRuns::default(),
                feed,
                cancels: crate::cancel::Cancels::default(),
                workspace_provider: provider,
                own_pushes: crate::push::OwnPushes::default(),
                test_writer: Some(Arc::clone(&writer) as Arc<dyn PlatformWriter>),
                test_session: Some(session),
                test_address_writer: source,
                test_issue_writer: None,
            },
            writer,
            model,
        }
    }

    fn request(run: &RunId) -> ReviewRequest {
        ReviewRequest {
            target: ReviewTarget {
                repo: RepoRef::parse(Platform::GitHub, "o/r").unwrap(),
                number: 7,
            },
            commit: None,
            trigger: "test".to_owned(),
            requester: None,
            acknowledge: None,
            run: Some(run.clone()),
            submitted_at: None,
            queued_check: None,
        }
    }

    /// The kinds of the comments posted, from their hidden markers.
    fn posted_kinds(writer: &FakeWriter) -> Vec<Option<MarkerKind>> {
        writer
            .replies
            .lock()
            .unwrap()
            .iter()
            .map(|r| Marker::parse(&r.body).and_then(|m| m.kind))
            .collect()
    }

    /// The run's stages as (stage, state, detail), in order (#226).
    async fn stage_list(app: &App, run: &RunId) -> Vec<(henk_store::Stage, StageState, String)> {
        app.store
            .stages(run)
            .await
            .unwrap()
            .into_iter()
            .map(|s| (s.stage, s.state, s.detail))
            .collect()
    }

    /// Whatever way a review ends, no stage of it is left running.
    async fn assert_no_stage_running(app: &App, run: &RunId) {
        let stages = stage_list(app, run).await;
        assert!(
            stages
                .iter()
                .all(|(_, state, _)| *state != StageState::Running),
            "{stages:?}"
        );
        assert!(
            stages
                .iter()
                .any(|(stage, _, _)| *stage == henk_store::Stage::Done),
            "{stages:?}"
        );
    }

    #[tokio::test]
    async fn a_completed_review_goes_through_its_stages_in_order() {
        use henk_store::Stage;
        let f = fixture(DIFF, ScriptedClient::new("scripted", [done(), done()])).await;
        let run = RunId::parse("r-stages").unwrap();
        run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();
        let stages = stage_list(&f.app, &run).await;
        let order: Vec<_> = stages.iter().map(|(s, state, _)| (*s, *state)).collect();
        assert_eq!(
            order,
            [
                (Stage::Requested, StageState::Done),
                (Stage::Started, StageState::Done),
                (Stage::Diff, StageState::Done),
                (Stage::Checkout, StageState::Skipped),
                (Stage::Lanes, StageState::Done),
                (Stage::FactCheck, StageState::Skipped),
                (Stage::Publish, StageState::Done),
                (Stage::Done, StageState::Done),
            ],
            "{stages:?}"
        );
        let detail = |stage: Stage| {
            stages
                .iter()
                .find(|(s, _, _)| *s == stage)
                .map(|(_, _, d)| d.clone())
                .unwrap()
        };
        assert_eq!(detail(Stage::Requested), "test");
        assert!(
            detail(Stage::Diff).ends_with("0 not reviewed"),
            "{}",
            detail(Stage::Diff)
        );
        assert_eq!(detail(Stage::Lanes), "1 of 1 finished");
        assert_eq!(detail(Stage::FactCheck), "no drafts");
        assert_eq!(detail(Stage::Publish), "the summary");
        for (_, _, line) in &stages {
            assert!(henk_domain::text::is_in_style(line), "{line}");
        }
    }

    #[tokio::test]
    async fn a_completed_review_posts_its_summary_and_closes_the_check() {
        // Twice: the lane's end of turn is answered by the coverage nudge.
        let f = fixture(DIFF, ScriptedClient::new("scripted", [done(), done()])).await;
        let run = RunId::parse("r-done").unwrap();
        let report = run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(report.summary.as_deref(), Some("No issues found."));
        assert_eq!(posted_kinds(&f.writer), vec![Some(MarkerKind::Summary)]);
        {
            let finished = f.writer.finished.lock().unwrap();
            assert_eq!(finished.len(), 1, "the check is closed once");
            assert_eq!(finished[0].check_conclusion(), CheckConclusion::Success);
        }
        let record = f.app.store.run(&run).await.unwrap().unwrap();
        assert_eq!(record.status, RunStatus::Finished);
        let lanes = f.app.store.lanes(&run).await.unwrap();
        assert_eq!(lanes.len(), 1);
        assert_eq!(lanes[0].status, LaneStatus::Finished);
    }

    #[tokio::test]
    async fn a_lane_whose_model_declined_is_not_a_clean_review() {
        let refused = Ok(Completion {
            message: henk_llm::ChatMessage::assistant(""),
            stop: StopReason::Refused("content_filter".to_owned()),
            usage: Usage::default(),
        });
        let f = fixture(DIFF, ScriptedClient::new("scripted", [refused])).await;
        let run = RunId::parse("r-refused").unwrap();
        let _ = run_review(&f.app, request(&run), CancellationToken::new()).await;
        {
            let finished = f.writer.finished.lock().unwrap();
            assert_eq!(finished.len(), 1, "the check is closed");
            let summary = finished[0].summary();
            assert_ne!(
                summary, "No issues found.",
                "a refusal never reads as a clean review"
            );
            assert!(summary.contains("Lane lane-a did not finish."), "{summary}");
            assert_eq!(finished[0].check_conclusion(), CheckConclusion::Failure);
        }
        let lanes = f.app.store.lanes(&run).await.unwrap();
        assert_eq!(lanes[0].status, LaneStatus::DidNotFinish);
        assert_eq!(
            lanes[0].error.as_deref(),
            Some("the model declined (content_filter)")
        );
    }

    #[tokio::test]
    async fn a_failed_review_says_so_and_closes_the_check_as_failure() {
        // No patches: the diff is empty, which ends the run as Henk's failure.
        let f = fixture("", ScriptedClient::new("scripted", [])).await;
        let run = RunId::parse("r-fail").unwrap();
        let error = run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("diff is empty"), "{error:#}");
        assert_eq!(posted_kinds(&f.writer), vec![Some(MarkerKind::Failure)]);
        {
            let finished = f.writer.finished.lock().unwrap();
            assert_eq!(finished.len(), 1);
            assert!(finished[0].lanes.is_empty());
            assert_eq!(finished[0].check_conclusion(), CheckConclusion::Failure);
        }
        let record = f.app.store.run(&run).await.unwrap().unwrap();
        assert_eq!(record.status, RunStatus::Failed);
        assert!(record.error.unwrap().contains("diff is empty"));
        assert_no_stage_running(&f.app, &run).await;
        let stages = stage_list(&f.app, &run).await;
        assert!(
            stages.contains(&(
                henk_store::Stage::Diff,
                StageState::Failed,
                "did not complete".into()
            )),
            "{stages:?}"
        );
    }

    /// A review cancelled while its lane waits on the model.
    /// `shutdown` stops Henk as Ctrl-C does; otherwise only this review is
    /// cancelled, as superseding does.
    async fn cancelled_review(
        run: &RunId,
        shutdown: bool,
    ) -> (Fixture, anyhow::Result<ReviewReport>) {
        let model =
            ScriptedClient::new("scripted", [done(), done()]).with_delay(Duration::from_secs(30));
        let f = fixture(DIFF, model).await;
        let cancel = f.app.shutdown.child_token();
        let trigger = if shutdown {
            f.app.shutdown.clone()
        } else {
            cancel.clone()
        };
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            trigger.cancel();
        });
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            run_review(&f.app, request(run), cancel),
        )
        .await
        .expect("a cancelled review ends promptly");
        (f, result)
    }

    /// The contract #7's fix relies on: a cancelled review closes its check
    /// and leaves no run or lane `running`.
    #[tokio::test]
    async fn a_cancelled_review_closes_its_check_and_leaves_nothing_running() {
        let run = RunId::parse("r-cancel").unwrap();
        let (f, result) = cancelled_review(&run, false).await;
        assert!(result.is_err());
        assert_eq!(
            f.writer.finished.lock().unwrap().len(),
            1,
            "the check is closed"
        );
        let record = f.app.store.run(&run).await.unwrap().unwrap();
        assert_ne!(record.status, RunStatus::Running);
        for lane in f.app.store.lanes(&run).await.unwrap() {
            assert_ne!(lane.status, LaneStatus::Running, "{}", lane.name);
        }
        assert_no_stage_running(&f.app, &run).await;
    }

    /// #7: Ctrl-C or a shutdown closes the check as interrupted, posts
    /// nothing and leaves nothing running.
    #[tokio::test]
    async fn an_interrupted_review_closes_its_check_and_posts_nothing() {
        let run = RunId::parse("r-interrupted").unwrap();
        let (f, result) = cancelled_review(&run, true).await;
        assert!(result.is_err_and(|e| e.is::<Interrupted>()));
        assert!(
            f.writer.replies.lock().unwrap().is_empty(),
            "nothing posted"
        );
        {
            let finished = f.writer.finished.lock().unwrap();
            assert_eq!(finished.len(), 1, "the check is closed");
            assert_eq!(finished[0].check_conclusion(), CheckConclusion::Failure);
            assert_eq!(finished[0].headline(), "Review interrupted.");
        }
        let record = f.app.store.run(&run).await.unwrap().unwrap();
        assert_eq!(record.status, RunStatus::Failed);
        assert_eq!(record.error.as_deref(), Some("interrupted"));
        for lane in f.app.store.lanes(&run).await.unwrap() {
            assert_ne!(lane.status, LaneStatus::Running, "{}", lane.name);
        }
    }

    fn new_run(id: &RunId) -> henk_store::NewRun {
        henk_store::NewRun {
            id: id.clone(),
            kind: henk_domain::run::RunKind::Review,
            platform: Platform::GitHub,
            repo: "o/r".to_owned(),
            target: 7,
            commit: Some(SHA.to_owned()),
            requester: None,
            trigger: "cli".to_owned(),
            link: "http://henk/runs/x".to_owned(),
        }
    }

    /// #7: a run a dead process left is closed with its check; a run a
    /// live process is still working on is left alone.
    #[tokio::test]
    async fn the_reaper_closes_runs_whose_heartbeat_stopped_and_only_those() {
        let f = fixture(DIFF, ScriptedClient::new("scripted", [done()])).await;
        let (dead, live) = (
            RunId::parse("r-dead").unwrap(),
            RunId::parse("r-live").unwrap(),
        );
        f.app.store.create_run(&new_run(&dead)).await.unwrap();
        f.app.store.set_check(&dead, "4242").await.unwrap();
        f.app.store.start_lane(&dead, "lane-a", "m").await.unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let cutoff = time::OffsetDateTime::now_utc();
        std::thread::sleep(std::time::Duration::from_millis(5));
        f.app.store.create_run(&new_run(&live)).await.unwrap();
        f.app.store.start_lane(&live, "lane-a", "m").await.unwrap();

        assert_eq!(crate::liveness::reap_silent_since(&f.app, cutoff).await, 1);

        let record = f.app.store.run(&dead).await.unwrap().unwrap();
        assert_eq!(record.status, RunStatus::Failed);
        assert_eq!(
            record.error.as_deref(),
            Some("interrupted: the process ended")
        );
        assert_eq!(
            f.app.store.lanes(&dead).await.unwrap()[0].status,
            LaneStatus::DidNotFinish
        );
        assert_eq!(
            *f.writer.finished_checks.lock().unwrap(),
            [Some("4242".to_owned())]
        );
        assert_eq!(
            f.writer.finished.lock().unwrap()[0].headline(),
            "Review interrupted."
        );

        let untouched = f.app.store.run(&live).await.unwrap().unwrap();
        assert_eq!(untouched.status, RunStatus::Running);
        assert_eq!(
            f.app.store.lanes(&live).await.unwrap()[0].status,
            LaneStatus::Running
        );
    }

    /// #47: a process that died and came back within the staleness window
    /// finds its old run still fresh at start. The serving reaper closes it
    /// on a later pass, with its check.
    #[tokio::test]
    async fn a_quick_restart_is_reaped_while_serving() {
        let f = fixture(DIFF, ScriptedClient::new("scripted", [done()])).await;
        let crashed = RunId::parse("r-crashed").unwrap();
        f.app.store.create_run(&new_run(&crashed)).await.unwrap();
        f.app.store.set_check(&crashed, "4242").await.unwrap();
        f.app
            .store
            .start_lane(&crashed, "lane-a", "m")
            .await
            .unwrap();

        assert_eq!(
            crate::liveness::reap_orphans(&f.app).await,
            0,
            "at restart the heartbeat is still fresh"
        );

        let writer = Arc::clone(&f.writer);
        let app = Arc::new(f.app);
        let cancel = CancellationToken::new();
        let reaper = tokio::spawn(crate::liveness::reap_every(
            Arc::clone(&app),
            cancel.clone(),
            Duration::from_millis(10),
            Duration::ZERO,
        ));
        let mut status = RunStatus::Running;
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            status = app.store.run(&crashed).await.unwrap().unwrap().status;
            if status != RunStatus::Running {
                break;
            }
        }
        cancel.cancel();
        reaper.await.unwrap();

        assert_eq!(status, RunStatus::Failed);
        let record = app.store.run(&crashed).await.unwrap().unwrap();
        assert_eq!(
            record.error.as_deref(),
            Some("interrupted: the process ended")
        );
        assert_eq!(
            app.store.lanes(&crashed).await.unwrap()[0].status,
            LaneStatus::DidNotFinish
        );
        assert_eq!(
            *writer.finished_checks.lock().unwrap(),
            [Some("4242".to_owned())]
        );
        assert_eq!(
            writer.finished.lock().unwrap()[0].headline(),
            "Review interrupted."
        );
    }

    /// #47: a periodic reaper must not close a run this process is still
    /// working on, even when its heartbeat lags (a store that was
    /// unreachable for a while). Once the run is let go, it may.
    #[tokio::test]
    async fn the_reaper_leaves_a_run_this_process_is_working_on() {
        let f = fixture(DIFF, ScriptedClient::new("scripted", [done()])).await;
        let run = RunId::parse("r-working").unwrap();
        f.app.store.create_run(&new_run(&run)).await.unwrap();
        let alive = KeepAlive::start(Arc::clone(&f.app.store), &f.app.live_runs, run.clone());
        std::thread::sleep(std::time::Duration::from_millis(5));
        let cutoff = time::OffsetDateTime::now_utc();

        assert_eq!(crate::liveness::reap_silent_since(&f.app, cutoff).await, 0);
        assert_eq!(
            f.app.store.run(&run).await.unwrap().unwrap().status,
            RunStatus::Running
        );

        drop(alive);
        assert_eq!(crate::liveness::reap_silent_since(&f.app, cutoff).await, 1);
        assert_eq!(
            f.app.store.run(&run).await.unwrap().unwrap().status,
            RunStatus::Failed
        );
    }

    /// #8: a review superseded by a newer commit is not Henk's failure. It
    /// must not post the failure comment, and its check is neutral.
    #[tokio::test]
    async fn a_review_cancelled_from_the_dashboard_says_by_whom_and_ends_cancelled() {
        let run = RunId::parse("r-cancelled").unwrap();
        let model =
            ScriptedClient::new("scripted", [done(), done()]).with_delay(Duration::from_secs(30));
        let f = fixture(DIFF, model).await;
        let cancel = f.app.shutdown.child_token();
        let _cancellable = f.app.cancels.register(run.clone(), cancel.clone());
        let (cancels, cancelled) = (f.app.cancels.clone(), run.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert!(cancels.cancel(&cancelled, "github:1234".to_owned()));
        });
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            run_review(&f.app, request(&run), cancel),
        )
        .await
        .expect("a cancelled review ends promptly");

        assert!(result.is_err_and(|e| e.is::<CancelledBy>()));
        let kinds = posted_kinds(&f.writer);
        assert!(
            !kinds.contains(&Some(MarkerKind::Failure)),
            "no failure comment"
        );
        let notice = f
            .writer
            .replies
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.body.contains("Cancelled from the dashboard"))
            .map(|r| r.body.clone())
            .expect("one comment says it was cancelled");
        assert!(notice.contains("by GitHub account 1234."), "{notice}");
        assert_eq!(
            Marker::parse(&notice).and_then(|m| m.kind),
            Some(MarkerKind::Reply)
        );
        {
            let finished = f.writer.finished.lock().unwrap();
            assert_eq!(finished[0].check_conclusion(), CheckConclusion::Neutral);
            assert_eq!(finished[0].headline(), "Cancelled from the dashboard.");
        }
        let record = f.app.store.run(&run).await.unwrap().unwrap();
        assert_eq!(record.status, RunStatus::Cancelled);
        assert_eq!(
            record.error.as_deref(),
            Some("cancelled from the dashboard by github:1234")
        );
    }

    #[tokio::test]
    async fn a_cancelled_review_leaves_the_queue_so_the_next_request_starts_afresh() {
        let model =
            ScriptedClient::new("scripted", [done(), done()]).with_delay(Duration::from_secs(30));
        let app = Arc::new(fixture(DIFF, model).await.app);
        let coordinator = crate::coordinator::Coordinator::new(Arc::clone(&app));
        let commit = CommitSha::parse(SHA).unwrap();
        let placeholder = RunId::parse("r-placeholder").unwrap();
        let (_, run) = coordinator.submit_review(request(&placeholder), commit.clone());
        let mut status = None;
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            status = app.store.run(&run).await.unwrap().map(|r| r.status);
            if status == Some(RunStatus::Running) {
                break;
            }
        }
        assert_eq!(status, Some(RunStatus::Running), "the review started");

        assert!(coordinator.cancel(&run, "github:1234".to_owned()));
        let (decision, again) = coordinator.submit_review(request(&placeholder), commit);
        assert_eq!(decision, henk_domain::queue::Decision::Start, "not joined");
        assert_ne!(again, run);
        for _ in 0..500 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            status = app.store.run(&run).await.unwrap().map(|r| r.status);
            if status != Some(RunStatus::Running) {
                break;
            }
        }
        assert_eq!(status, Some(RunStatus::Cancelled));
        assert!(
            !coordinator.cancel(&run, "github:1234".to_owned()),
            "an ended run is not cancellable"
        );
        app.shutdown.cancel();
    }

    #[tokio::test]
    async fn a_review_of_a_newer_commit_supersedes_the_running_one_and_is_named_by_it() {
        let model =
            ScriptedClient::new("scripted", [done(), done()]).with_delay(Duration::from_secs(30));
        let app = Arc::new(fixture(DIFF, model).await.app);
        let coordinator = crate::coordinator::Coordinator::new(Arc::clone(&app));
        let placeholder = RunId::parse("r-placeholder").unwrap();
        let (_, old) =
            coordinator.submit_review(request(&placeholder), CommitSha::parse(SHA).unwrap());
        let mut status = None;
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            status = app.store.run(&old).await.unwrap().map(|r| r.status);
            if status == Some(RunStatus::Running) {
                break;
            }
        }
        assert_eq!(status, Some(RunStatus::Running), "the first review started");

        let newer = CommitSha::parse("fedcba9876543210fedcba9876543210fedcba98").unwrap();
        let (decision, new) = coordinator.submit_review(request(&placeholder), newer);
        assert_eq!(decision, henk_domain::queue::Decision::Supersede);
        let mut record = None;
        for _ in 0..500 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            record = app.store.run(&old).await.unwrap();
            if record
                .as_ref()
                .is_some_and(|r| r.status != RunStatus::Running)
            {
                break;
            }
        }
        let record = record.unwrap();
        assert_eq!(record.status, RunStatus::Superseded);
        assert_eq!(record.superseded_by, Some(new.clone()));
        assert_no_stage_running(&app, &old).await;
        let stages = stage_list(&app, &old).await;
        assert!(
            stages
                .iter()
                .any(|(s, state, _)| *s == henk_store::Stage::Queued && *state == StageState::Done),
            "the coordinator's submit time gives the wait: {stages:?}"
        );
        assert!(
            stages.contains(&(
                henk_store::Stage::Done,
                StageState::Skipped,
                format!("superseded by {new}")
            )),
            "{stages:?}"
        );
        app.shutdown.cancel();
    }

    /// Waits, up to four seconds, until `ready` holds.
    async fn until(what: &str, ready: impl Fn() -> bool) {
        for _ in 0..400 {
            if ready() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("never: {what}");
    }

    /// One slot, review A of #7 holding it with a slow model, and review B
    /// of #8 submitted after it, so B waits (#262).
    async fn one_slot_two_reviews(
        fail_queue: bool,
    ) -> (
        Arc<App>,
        Arc<FakeWriter>,
        crate::coordinator::Coordinator,
        RunId,
    ) {
        let model = ScriptedClient::new("scripted", (0..12).map(|_| done()))
            .with_delay(Duration::from_millis(150));
        let mut f = fixture(DIFF, model).await;
        f.app.settings.review.max_concurrent = 1;
        let writer = if fail_queue {
            let failing = Arc::new(FakeWriter {
                head: SHA.to_owned(),
                patches: henk_domain::diff::split_unified(DIFF),
                fail_queue: true,
                ..FakeWriter::default()
            });
            f.app.test_writer = Some(Arc::clone(&failing) as Arc<dyn PlatformWriter>);
            failing
        } else {
            Arc::clone(&f.writer)
        };
        let app = Arc::new(f.app);
        let coordinator = crate::coordinator::Coordinator::new(Arc::clone(&app));
        let placeholder = RunId::parse("r-placeholder").unwrap();
        let commit = CommitSha::parse(SHA).unwrap();
        let _ = coordinator.submit_review(request(&placeholder), commit.clone());
        until("review A holds the slot", || {
            coordinator.running_reviews() == 1
        })
        .await;
        let mut other = request(&placeholder);
        other.target.number = 8;
        let (_, waiting) = coordinator.submit_review(other, commit);
        (app, writer, coordinator, waiting)
    }

    fn finished(
        writer: &FakeWriter,
    ) -> Vec<(Option<String>, Option<henk_domain::review::Stopped>)> {
        let checks = writer.finished_checks.lock().unwrap().clone();
        let outcomes = writer.finished.lock().unwrap().clone();
        checks
            .into_iter()
            .zip(outcomes.iter().map(|o| o.stopped))
            .collect()
    }

    #[tokio::test]
    async fn a_waiting_review_shows_queued_then_the_same_check_runs_and_completes() {
        let (app, writer, _coordinator, _waiting) = one_slot_two_reviews(false).await;
        until("B's check is queued", || {
            writer.queued.lock().unwrap().len() == 1
        })
        .await;
        let (id, title, summary) = writer.queued.lock().unwrap()[0].clone();
        assert_eq!((id.as_str(), title.as_str()), ("queued-1", "Queued"));
        assert_eq!(
            summary,
            "Waiting for a review slot: all 1 are in use. Henk starts this review when one frees."
        );
        assert!(henk_domain::text::is_in_style(&summary));
        assert_eq!(
            *writer.started.lock().unwrap(),
            [None],
            "A had a free slot: no queued check, a new one as before"
        );
        until("B starts on its queued check", || {
            writer.started.lock().unwrap().len() == 2
        })
        .await;
        assert_eq!(
            writer.started.lock().unwrap()[1].as_deref(),
            Some("queued-1")
        );
        until("B ends", || {
            writer
                .finished_checks
                .lock()
                .unwrap()
                .contains(&Some("queued-1".to_owned()))
        })
        .await;
        app.shutdown.cancel();
    }

    #[tokio::test]
    async fn a_queued_check_closes_when_its_review_is_superseded_or_henk_stops() {
        let (app, writer, coordinator, _waiting) = one_slot_two_reviews(false).await;
        until("B's check is queued", || {
            writer.queued.lock().unwrap().len() == 1
        })
        .await;
        let mut newer = request(&RunId::parse("r-placeholder").unwrap());
        newer.target.number = 8;
        let newer_commit = CommitSha::parse("fedcba9876543210fedcba9876543210fedcba98").unwrap();
        let _ = coordinator.submit_review(newer, newer_commit);
        until("B's check closes", || !finished(&writer).is_empty()).await;
        assert_eq!(
            finished(&writer)[0],
            (
                Some("queued-1".to_owned()),
                Some(henk_domain::review::Stopped::Superseded)
            ),
            "superseded while queued: neutral, no comment"
        );
        until("the newer review is queued", || {
            writer.queued.lock().unwrap().len() == 2
        })
        .await;

        app.shutdown.cancel();
        until("the newer one's check closes", || {
            finished(&writer)
                .iter()
                .any(|(check, _)| check.as_deref() == Some("queued-2"))
        })
        .await;
        assert!(
            finished(&writer).contains(&(
                Some("queued-2".to_owned()),
                Some(henk_domain::review::Stopped::Interrupted)
            )),
            "stopped with Henk while queued: {:?}",
            finished(&writer)
        );
    }

    #[tokio::test]
    async fn a_queued_check_closes_as_cancelled_and_a_failed_queue_does_not_stop_the_review() {
        let (app, writer, coordinator, waiting) = one_slot_two_reviews(false).await;
        until("B's check is queued", || {
            writer.queued.lock().unwrap().len() == 1
        })
        .await;
        assert!(coordinator.cancel(&waiting, "github:1234".to_owned()));
        until("B's check closes", || !finished(&writer).is_empty()).await;
        assert_eq!(
            finished(&writer)[0],
            (
                Some("queued-1".to_owned()),
                Some(henk_domain::review::Stopped::Cancelled)
            )
        );
        app.shutdown.cancel();

        let (app, writer, _coordinator, _waiting) = one_slot_two_reviews(true).await;
        until("B starts without a queued check", || {
            writer.started.lock().unwrap().len() == 2
        })
        .await;
        assert_eq!(*writer.started.lock().unwrap(), [None, None]);
        app.shutdown.cancel();
    }

    /// A slow platform does not cost a waiting review its place: B asks to
    /// show its check queued first and gets the answer last, and still
    /// starts before C, submitted after it (#262).
    #[tokio::test]
    async fn a_slow_queued_check_keeps_the_order_reviews_wait_in() {
        let model = ScriptedClient::new("scripted", (0..12).map(|_| done()))
            .with_delay(Duration::from_millis(400));
        let mut f = fixture(DIFF, model).await;
        f.app.settings.review.max_concurrent = 1;
        let writer = Arc::new(FakeWriter {
            head: SHA.to_owned(),
            patches: henk_domain::diff::split_unified(DIFF),
            slow_queue: Some((8, Duration::from_millis(250))),
            ..FakeWriter::default()
        });
        f.app.test_writer = Some(Arc::clone(&writer) as Arc<dyn PlatformWriter>);
        let app = Arc::new(f.app);
        let coordinator = crate::coordinator::Coordinator::new(Arc::clone(&app));
        let placeholder = RunId::parse("r-placeholder").unwrap();
        let commit = CommitSha::parse(SHA).unwrap();
        let _ = coordinator.submit_review(request(&placeholder), commit.clone());
        until("review A holds the slot", || {
            coordinator.running_reviews() == 1
        })
        .await;
        let mut b = request(&placeholder);
        b.target.number = 8;
        let _ = coordinator.submit_review(b, commit.clone());
        tokio::time::sleep(Duration::from_millis(20)).await;
        let mut c = request(&placeholder);
        c.target.number = 9;
        let _ = coordinator.submit_review(c, commit);
        until("both checks are queued", || {
            writer.queued.lock().unwrap().len() == 2
        })
        .await;
        assert_eq!(
            coordinator
                .slots()
                .waiting
                .iter()
                .map(|w| (w.position, w.number))
                .collect::<Vec<_>>(),
            [(1, 8), (2, 9)]
        );
        until("B and C start", || {
            writer.started.lock().unwrap().len() == 3
        })
        .await;
        assert_eq!(
            *writer.started.lock().unwrap(),
            [
                None,
                Some("queued-2".to_owned()),
                Some("queued-1".to_owned())
            ],
            "C's check was queued first, B still starts first"
        );
        app.shutdown.cancel();
    }

    /// A review that waited with a queued check, then ran preflight against
    /// `writer`, with a dashboard so the queue link differs from the run's.
    async fn queued_then_preflight(writer: FakeWriter) -> (App, Arc<FakeWriter>, RunId) {
        let config = format!("{CONFIG}[dashboard]\nallowed_github_ids = [1]\n");
        let mut f = fixture_on(
            DIFF,
            ScriptedClient::new("scripted", Vec::new()),
            &config,
            SHA,
            Arc::new(crate::workspace::host::HostProvider),
            None,
        )
        .await;
        let writer = Arc::new(writer);
        f.app.test_writer = Some(Arc::clone(&writer) as Arc<dyn PlatformWriter>);
        let run = RunId::parse("r-queued").unwrap();
        let mut queued = request(&run);
        queued.commit = Some(CommitSha::parse(SHA).unwrap());
        queued.queued_check = Some(ReviewHandle("queued-1".to_owned()));
        let result = run_review(&f.app, queued, CancellationToken::new()).await;
        assert!(result.is_err(), "the review does not run");
        (f.app, writer, run)
    }

    #[tokio::test]
    async fn a_queued_check_that_could_not_be_started_is_still_completed_at_the_end() {
        let mut f = fixture(
            DIFF,
            ScriptedClient::new("scripted", (0..12).map(|_| done())),
        )
        .await;
        let writer = Arc::new(FakeWriter {
            head: SHA.to_owned(),
            patches: henk_domain::diff::split_unified(DIFF),
            fail_start: true,
            ..FakeWriter::default()
        });
        f.app.test_writer = Some(Arc::clone(&writer) as Arc<dyn PlatformWriter>);
        let run = RunId::parse("r-start-failed").unwrap();
        let mut queued = request(&run);
        queued.commit = Some(CommitSha::parse(SHA).unwrap());
        queued.queued_check = Some(ReviewHandle("queued-1".to_owned()));
        let report = run_review(&f.app, queued, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            *writer.started.lock().unwrap(),
            [Some("queued-1".to_owned())]
        );
        assert_eq!(
            *writer.finished_checks.lock().unwrap(),
            [Some("queued-1".to_owned())],
            "the queued check is completed, not left queued (#262)"
        );
        assert!(report.outcome.unwrap().completed());
    }

    /// An app with a dashboard whose store already holds a run `run`, so
    /// recording it again fails as a store error would.
    async fn store_refuses(run: &RunId) -> (App, Arc<FakeWriter>) {
        let config = format!("{CONFIG}[dashboard]\nallowed_github_ids = [1]\n");
        let f = fixture_on(
            DIFF,
            ScriptedClient::new("scripted", Vec::new()),
            &config,
            SHA,
            Arc::new(crate::workspace::host::HostProvider),
            None,
        )
        .await;
        f.app
            .store
            .create_run(&NewRun {
                id: run.clone(),
                kind: RunKind::Review,
                platform: Platform::GitHub,
                repo: "o/r".to_owned(),
                target: 7,
                commit: None,
                requester: None,
                trigger: "test".to_owned(),
                link: String::new(),
            })
            .await
            .unwrap();
        (f.app, f.writer)
    }

    fn queued_request(run: &RunId) -> ReviewRequest {
        let mut queued = request(run);
        queued.commit = Some(CommitSha::parse(SHA).unwrap());
        queued.queued_check = Some(ReviewHandle("queued-1".to_owned()));
        queued
    }

    #[tokio::test]
    async fn a_queued_check_closes_as_a_failure_when_the_run_cannot_be_recorded() {
        let run = RunId::parse("r-taken").unwrap();
        let (app, writer) = store_refuses(&run).await;
        let result = run_review(&app, queued_request(&run), CancellationToken::new()).await;
        assert!(result.is_err());
        assert_eq!(
            *writer.finished_checks.lock().unwrap(),
            [Some("queued-1".to_owned())],
            "the queued check does not stay queued (#262)"
        );
        let finished = writer.finished.lock().unwrap().clone();
        assert_eq!(finished[0].check_conclusion(), CheckConclusion::Failure);
        assert_eq!(finished[0].headline(), "Review did not complete.");
        assert_eq!(
            *writer.finished_links.lock().unwrap(),
            [app.settings.queue_link(&run)]
        );
    }

    #[tokio::test]
    async fn a_review_cancelled_while_queued_closes_its_check_when_the_run_cannot_be_recorded() {
        let run = RunId::parse("r-taken").unwrap();
        let (app, writer) = store_refuses(&run).await;
        let result =
            report_cancelled_while_queued(&app, &queued_request(&run), "github:1234").await;
        assert!(result.is_err());
        assert_eq!(
            *writer.finished_checks.lock().unwrap(),
            [Some("queued-1".to_owned())],
            "the queued check does not stay queued (#262)"
        );
        assert_eq!(
            writer.finished.lock().unwrap()[0].stopped,
            Some(henk_domain::review::Stopped::Cancelled)
        );
        assert_eq!(
            *writer.finished_links.lock().unwrap(),
            [app.settings.queue_link(&run)]
        );
    }

    #[tokio::test]
    async fn a_queued_review_of_a_closed_pull_request_ends_not_reviewed_with_the_queue_link() {
        let (app, writer, run) = queued_then_preflight(FakeWriter {
            head: SHA.to_owned(),
            state: Some(PullRequestState::Closed),
            ..FakeWriter::default()
        })
        .await;
        let finished = writer.finished.lock().unwrap().clone();
        assert_eq!(finished.len(), 1);
        assert_eq!(
            finished[0].stopped,
            Some(henk_domain::review::Stopped::NotReviewed)
        );
        assert_eq!(finished[0].check_conclusion(), CheckConclusion::Neutral);
        assert_eq!(
            finished[0].headline(),
            "Not reviewed: pull request #7 is not open."
        );
        assert_eq!(
            *writer.finished_checks.lock().unwrap(),
            [Some("queued-1".to_owned())]
        );
        let links = writer.finished_links.lock().unwrap().clone();
        assert_eq!(links, [app.settings.queue_link(&run)]);
        assert_ne!(
            links[0],
            app.settings.run_link(&run),
            "the run was never stored: no link to it"
        );
        assert!(app.store.run(&run).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_queued_review_whose_pull_request_read_fails_ends_as_a_failure_without_the_error() {
        let secret = "502 Bad Gateway from https://api.github.test/repos/o/r/pulls/7";
        let (app, writer, run) = queued_then_preflight(FakeWriter {
            head: SHA.to_owned(),
            fail_pull_request: Some(secret.to_owned()),
            ..FakeWriter::default()
        })
        .await;
        let finished = writer.finished.lock().unwrap().clone();
        assert_eq!(finished.len(), 1);
        assert_eq!(finished[0].stopped, None);
        assert_eq!(
            finished[0].check_conclusion(),
            CheckConclusion::Failure,
            "an incomplete review is Henk's failure (§3.3)"
        );
        assert_eq!(finished[0].headline(), "Review did not complete.");
        for text in [finished[0].headline(), finished[0].summary()] {
            assert!(!text.contains("502"), "{text}");
            assert!(!text.contains("api.github.test"), "{text}");
        }
        assert_eq!(
            *writer.finished_links.lock().unwrap(),
            [app.settings.queue_link(&run)]
        );
    }

    #[tokio::test]
    async fn a_review_cancelled_while_it_waits_for_a_slot_ends_cancelled_at_once() {
        let model =
            ScriptedClient::new("scripted", [done(), done()]).with_delay(Duration::from_secs(30));
        let mut f = fixture(DIFF, model).await;
        f.app.settings.review.max_concurrent = 1;
        let writer = Arc::clone(&f.writer);
        let app = Arc::new(f.app);
        let coordinator = crate::coordinator::Coordinator::new(Arc::clone(&app));
        let commit = CommitSha::parse(SHA).unwrap();
        let placeholder = RunId::parse("r-placeholder").unwrap();
        let (_, first) = coordinator.submit_review(request(&placeholder), commit.clone());
        let mut status = None;
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            status = app.store.run(&first).await.unwrap().map(|r| r.status);
            if status == Some(RunStatus::Running) {
                break;
            }
        }
        assert_eq!(
            status,
            Some(RunStatus::Running),
            "the first review holds the slot"
        );

        let mut other = request(&placeholder);
        other.target.number = 8;
        let (_, queued) = coordinator.submit_review(other, commit);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            app.store.run(&queued).await.unwrap().is_none(),
            "the second review waits for the slot"
        );
        let slots = coordinator.slots();
        assert_eq!((slots.limit, slots.in_use), (1, 1));
        assert_eq!(
            slots
                .waiting
                .iter()
                .map(|w| (w.repo.as_str(), w.number))
                .collect::<Vec<_>>(),
            [("o/r", 8)],
            "the waiting review, not the running one"
        );

        assert!(coordinator.cancel(&queued, "github:1234".to_owned()));
        let mut record = None;
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            record = app.store.run(&queued).await.unwrap();
            if record
                .as_ref()
                .is_some_and(|r| r.status != RunStatus::Running)
            {
                break;
            }
        }
        let record = record.expect("the cancelled review has a run the dashboard can show");
        assert_eq!(record.status, RunStatus::Cancelled);
        assert_eq!(record.target, 8);
        assert_eq!(
            record.error.as_deref(),
            Some("cancelled from the dashboard by github:1234")
        );
        let notices: Vec<String> = writer
            .replies
            .lock()
            .unwrap()
            .iter()
            .filter(|r| Marker::parse(&r.body).is_some_and(|m| m.run == queued))
            .map(|r| r.body.clone())
            .collect();
        let stages = stage_list(&app, &queued).await;
        let cancelled = "cancelled by github:1234 while waiting for a slot".to_owned();
        assert!(
            stages.contains(&(
                henk_store::Stage::Queued,
                StageState::Skipped,
                cancelled.clone()
            )),
            "it never got a slot, so the queue is not done: {stages:?}"
        );
        assert!(
            stages.contains(&(henk_store::Stage::Done, StageState::Skipped, cancelled)),
            "{stages:?}"
        );
        assert_no_stage_running(&app, &queued).await;
        assert_eq!(notices.len(), 1, "one comment: {notices:?}");
        assert!(
            notices[0].contains("Cancelled from the dashboard by GitHub account 1234."),
            "{}",
            notices[0]
        );
        assert_eq!(
            app.store.run(&first).await.unwrap().map(|r| r.status),
            Some(RunStatus::Running),
            "the first review still holds the slot: the cancel did not wait for it"
        );
        app.shutdown.cancel();
    }

    #[tokio::test]
    async fn waiting_reviews_queue_in_order_with_who_asked_and_why_they_wait() {
        let model = ScriptedClient::new("scripted", [done(), done(), done()])
            .with_delay(Duration::from_secs(30));
        let mut f = fixture(DIFF, model).await;
        f.app.settings.review.max_concurrent = 1;
        let app = Arc::new(f.app);
        let coordinator = crate::coordinator::Coordinator::new(Arc::clone(&app));
        let commit = CommitSha::parse(SHA).unwrap();
        let placeholder = RunId::parse("r-placeholder").unwrap();
        let (_, first) = coordinator.submit_review(request(&placeholder), commit.clone());
        for _ in 0..200 {
            if coordinator.running_reviews() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut queued = Vec::new();
        for (number, requester) in [(8, Some("github:1234")), (9, None)] {
            let mut other = request(&placeholder);
            other.target.number = number;
            other.trigger = format!("push {number}");
            other.requester = requester.map(str::to_owned);
            queued.push(coordinator.submit_review(other, commit.clone()).1);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            (
                coordinator.running_reviews(),
                coordinator.queued_reviews(),
                coordinator.tracked_reviews()
            ),
            (1, 2, 3),
            "a waiting review is not running"
        );
        let slots = coordinator.slots();
        let seen: Vec<_> = slots
            .waiting
            .iter()
            .map(|w| {
                (
                    w.position,
                    w.run.clone(),
                    w.number,
                    w.trigger.as_str(),
                    w.requester.as_deref(),
                    w.reason,
                    w.commit.as_str() == SHA,
                )
            })
            .collect();
        assert_eq!(
            seen,
            [
                (
                    1,
                    queued[0].clone(),
                    8,
                    "push 8",
                    Some("github:1234"),
                    crate::coordinator::Reason::NoSlot,
                    true
                ),
                (
                    2,
                    queued[1].clone(),
                    9,
                    "push 9",
                    None,
                    crate::coordinator::Reason::NoSlot,
                    true
                ),
            ]
        );
        assert!(slots.waiting.iter().all(|w| w.run != first));

        assert!(coordinator.cancel(&queued[0], "github:1234".to_owned()));
        let after: Vec<_> = coordinator
            .slots()
            .waiting
            .iter()
            .map(|w| (w.position, w.number))
            .collect();
        assert_eq!(after, [(1, 9)], "the next one moves up");
        app.shutdown.cancel();
    }

    #[tokio::test]
    async fn a_superseded_review_posts_no_failure() {
        let run = RunId::parse("r-superseded").unwrap();
        let (f, result) = cancelled_review(&run, false).await;
        assert!(result.is_err_and(|e| e.is::<Superseded>()));
        assert!(
            !posted_kinds(&f.writer).contains(&Some(MarkerKind::Failure)),
            "no failure comment"
        );
        {
            let finished = f.writer.finished.lock().unwrap();
            assert_eq!(finished[0].check_conclusion(), CheckConclusion::Neutral);
        }
        let record = f.app.store.run(&run).await.unwrap().unwrap();
        assert_eq!(record.status, RunStatus::Superseded);
        assert_eq!(
            record.error.as_deref(),
            Some("superseded by a review of a newer commit")
        );
        assert_eq!(
            record.superseded_by, None,
            "no coordinator named a newer run"
        );
    }

    /// The platform's git credential in these tests: it reaches git on
    /// Henk's side and must never reach a model (§8.4).
    const GIT_TOKEN: &str = "ghs_TestOnlyGitCredential4f9c2e";

    /// The pull request's head repository as a review sees it (#170): where
    /// to fetch from and the credential to fetch with. Nothing else is asked
    /// of it.
    struct Source(henk_platform::address::PullFacts);

    fn unused() -> henk_platform::PlatformError {
        henk_platform::PlatformError::Decode("not used by a review".to_owned())
    }

    #[async_trait::async_trait]
    impl henk_platform::address::AddressWriter for Source {
        async fn pull_facts(
            &self,
            _: &ReviewTarget,
        ) -> Result<henk_platform::address::PullFacts, henk_platform::PlatformError> {
            Ok(self.0.clone())
        }
        async fn open_threads(
            &self,
            _: &ReviewTarget,
        ) -> Result<Vec<henk_platform::address::OpenThread>, henk_platform::PlatformError> {
            Err(unused())
        }
        async fn git_credential(
            &self,
        ) -> Result<Option<henk_platform::address::GitCredential>, henk_platform::PlatformError>
        {
            Ok(Some(henk_platform::address::GitCredential::github(
                GIT_TOKEN.to_owned().into(),
            )))
        }
        async fn repo_head(
            &self,
            _: &henk_domain::allowlist::RepoRef,
        ) -> Result<henk_platform::address::RepoHead, henk_platform::PlatformError> {
            Err(unused())
        }
        async fn commit_identity(
            &self,
        ) -> Result<henk_platform::address::CommitIdentity, henk_platform::PlatformError> {
            Err(unused())
        }
        fn noreply_host(&self) -> Result<String, henk_platform::PlatformError> {
            Err(unused())
        }
        async fn user_login(&self, _: u64) -> Result<String, henk_platform::PlatformError> {
            Err(unused())
        }
        fn commit_url(&self, _: &ReviewTarget, _: &str) -> String {
            String::new()
        }
        async fn reply_in_thread(
            &self,
            _: &ReviewTarget,
            _: &henk_platform::address::OpenThread,
            _: &str,
        ) -> Result<henk_platform::PostedComment, henk_platform::PlatformError> {
            Err(unused())
        }
        async fn resolve_thread(
            &self,
            _: &ReviewTarget,
            _: &str,
        ) -> Result<(), henk_platform::PlatformError> {
            Err(unused())
        }
        async fn post_comment(
            &self,
            _: &ReviewTarget,
            _: &str,
        ) -> Result<henk_platform::PostedComment, henk_platform::PlatformError> {
            Err(unused())
        }
    }

    /// The fake backend, noting what `src/a.rs` held in each source it
    /// was asked to open.
    #[derive(Default)]
    struct Peek {
        inner: crate::workspace::fake::FakeProvider,
        seen: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl crate::workspace::WorkspaceProvider for Peek {
        async fn open(
            &self,
            source: &std::path::Path,
            profile: &henk_domain::workspace::Profile,
        ) -> Result<Arc<dyn crate::workspace::Workspace>, crate::workspace::WorkspaceError>
        {
            let content = std::fs::read_to_string(source.join("src/a.rs")).unwrap_or_default();
            self.seen.lock().unwrap().push(content);
            self.inner.open(source, profile).await
        }
    }

    /// Two lanes and the fact-checker, reviewing in a workspace with one
    /// setup step.
    fn reviewing_config(extra: &str) -> String {
        let key = russh::keys::PrivateKey::from(
            russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[1; 32]),
        );
        let host_key = key.public_key().to_openssh().unwrap();
        CONFIG.replace(
            "lanes = [{ name = \"lane-a\", model = \"m\" }]\n",
            &format!(
                "lanes = [{{ name = \"lane-a\", model = \"m\" }}, {{ name = \"lane-b\", model = \"m\" }}]\n[review.fact_check]\nmodel = \"m\"\n[workspace]\nbackend = \"ssh\"\nreview = true\nsetup = [[\"make\", \"deps\"]]\n[workspace.ssh]\nhost = \"sandbox.example\"\nhost_key = \"{host_key}\"\n{extra}"
            ),
        )
    }

    struct Reviewed {
        f: Fixture,
        peek: Arc<Peek>,
        /// Keeps the remote while the review fetches from it.
        _remote: crate::git::ScratchDir,
    }

    /// A review of the seeded commit of a local remote whose branch has
    /// moved on since, so only a fetch by sha finds what is reviewed.
    async fn reviewed(
        name: &str,
        config: &str,
        model: ScriptedClient,
        setup_code: i32,
    ) -> Reviewed {
        reviewed_slow(name, config, model, setup_code, Duration::ZERO).await
    }

    /// [`reviewed`] with a setup step that takes `delay`.
    async fn reviewed_slow(
        name: &str,
        config: &str,
        model: ScriptedClient,
        setup_code: i32,
        delay: Duration,
    ) -> Reviewed {
        let mut inner = crate::workspace::fake::FakeProvider::default();
        inner.script.insert(
            "make deps".to_owned(),
            crate::workspace::fake::Scripted {
                code: setup_code,
                output: "deps ready".to_owned(),
                writes: Vec::new(),
                delay,
            },
        );
        let peek = Arc::new(Peek {
            inner,
            seen: std::sync::Mutex::new(Vec::new()),
        });
        let (f, remote) = reviewed_on(
            name,
            config,
            model,
            Arc::clone(&peek) as Arc<dyn crate::workspace::WorkspaceProvider>,
        )
        .await;
        Reviewed {
            f,
            peek,
            _remote: remote,
        }
    }

    /// [`reviewed`] on any backend; the remote is returned to be kept.
    async fn reviewed_on(
        name: &str,
        config: &str,
        model: ScriptedClient,
        provider: Arc<dyn crate::workspace::WorkspaceProvider>,
    ) -> (Fixture, crate::git::ScratchDir) {
        let (remote, reviewed) = crate::git::tests::bare_remote(name).await;
        let url = remote.path().to_string_lossy().into_owned();
        let later = crate::git::Checkout::clone_at(
            crate::git::ScratchDir::new(&format!("{name}-later")).unwrap(),
            &url,
            "feature",
            &reviewed,
            None,
        )
        .await
        .unwrap();
        std::fs::write(later.path().join("src/a.rs"), "moved on\n").unwrap();
        later
            .commit(
                &henk_platform::address::CommitIdentity {
                    name: "Later".to_owned(),
                    email: "later@example.com".to_owned(),
                },
                "Later\n",
            )
            .await
            .unwrap();
        later.push("feature").await.unwrap();

        let facts = henk_platform::address::PullFacts {
            push: henk_domain::address::PushFacts {
                open: true,
                head_repo: Some("o/r".to_owned()),
                base_repo: "o/r".to_owned(),
                head_ref: "feature".to_owned(),
                default_branch: "main".to_owned(),
                head_protected: false,
            },
            head: reviewed.clone(),
            remote: url,
        };
        let f = fixture_on(
            DIFF,
            model,
            config,
            reviewed.as_str(),
            provider,
            Some(Arc::new(Source(facts))),
        )
        .await;
        (f, remote)
    }

    /// The ssh backend's runner, run locally, and a setup step that exists
    /// there.
    async fn review_on_ssh(
        name: &str,
        provider: Arc<dyn crate::workspace::WorkspaceProvider>,
    ) -> Fixture {
        let config = reviewing_config("").replace("[[\"make\", \"deps\"]]", "[[\"true\"]]");
        let (f, _remote) = reviewed_on(name, &config, many_done(), provider).await;
        let run = RunId::parse(format!("r-{name}")).unwrap();
        run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();
        let events = f.app.store.events(&run).await.unwrap();
        assert!(
            events
                .iter()
                .any(|e| e.message.starts_with("review workspaces: 3 of 3 ready")),
            "{events:?}"
        );
        f
    }

    #[tokio::test]
    async fn a_review_on_the_ssh_backend_leaves_nothing_on_the_host() {
        use crate::workspace::remote::tests::LocalRunner;
        let runner = LocalRunner::new("henk-review-ssh");
        let provider = crate::workspace::ssh::SshProvider::with_runner(
            Arc::clone(&runner) as Arc<dyn crate::workspace::remote::Runner>
        );
        review_on_ssh("ssh-local", Arc::new(provider)).await;
        assert_eq!(std::fs::read_dir(runner.base()).unwrap().count(), 0);
    }

    #[tokio::test]
    #[ignore = "needs a sandbox host (HENK_TEST_SSH_*)"]
    async fn live_a_review_on_a_real_sandbox_host_opens_and_closes_its_workspaces() {
        let provider =
            crate::workspace::ssh::SshProvider::new(crate::workspace::ssh::tests::live_target());
        review_on_ssh("ssh-live", Arc::new(provider)).await;
    }

    #[tokio::test]
    #[ignore = "needs a cluster (HENK_TEST_KUBE_*)"]
    async fn live_kube_a_review_opens_and_closes_a_pod_per_lane() {
        use crate::workspace::kubernetes::tests::{LIVE, live_provider, pods_left};
        let _one = LIVE.lock().await;
        let provider = live_provider().await;
        review_on_ssh("kube-live", Arc::new(provider.clone())).await;
        assert_eq!(pods_left(&provider).await, Vec::<String>::new());
    }

    /// A one-lane review on `provider` whose lane asks `bash` for the
    /// checked-out commit: it gets the reviewed sha, as the run user, in
    /// its own copy.
    async fn bash_on_ssh(name: &str, provider: Arc<dyn crate::workspace::WorkspaceProvider>) {
        let config = reviewing_config("")
            .replace(", { name = \"lane-b\", model = \"m\" }", "")
            .replace("[[\"make\", \"deps\"]]", "[[\"true\"]]");
        let model = ScriptedClient::new(
            "scripted",
            [
                call(
                    "bash",
                    serde_json::json!({"command": "git log -1 --format=%H"}),
                ),
                done(),
                done(),
                done(),
            ],
        );
        let (f, _remote) = reviewed_on(name, &config, model, provider).await;
        let run = RunId::parse(format!("r-{name}")).unwrap();
        run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();
        let (_, results, _) = seen(&f.model);
        let expected = format!("\n{}\n", f.writer.head);
        assert!(
            results
                .iter()
                .any(|r| r.starts_with("exit 0 in ") && r.ends_with(&expected)),
            "the lane's bash sees the reviewed commit: {results:?}"
        );
        let stored = f
            .app
            .store
            .transcript(&run, "lane-a")
            .await
            .unwrap()
            .expect("the lane's transcript is stored with the run");
        assert!(
            stored.body.contains(&expected.replace('\n', "\\n")),
            "what bash printed is in the transcript: {}",
            stored.body
        );
        assert!(!stored.body.contains(GIT_TOKEN));
    }

    #[tokio::test]
    async fn bash_on_the_ssh_backend_sees_the_reviewed_commit() {
        use crate::workspace::remote::tests::LocalRunner;
        let runner = LocalRunner::new("henk-review-ssh-bash");
        let provider = crate::workspace::ssh::SshProvider::with_runner(
            Arc::clone(&runner) as Arc<dyn crate::workspace::remote::Runner>
        );
        bash_on_ssh("ssh-local-bash", Arc::new(provider)).await;
        assert_eq!(std::fs::read_dir(runner.base()).unwrap().count(), 0);
    }

    #[tokio::test]
    #[ignore = "needs a sandbox host (HENK_TEST_SSH_*)"]
    async fn live_a_review_lane_runs_bash_on_a_real_sandbox_host() {
        let provider =
            crate::workspace::ssh::SshProvider::new(crate::workspace::ssh::tests::live_target());
        bash_on_ssh("ssh-live-bash", Arc::new(provider)).await;
    }

    #[tokio::test]
    #[ignore = "needs a cluster (HENK_TEST_KUBE_*)"]
    async fn live_kube_a_review_lane_runs_bash_in_its_pod() {
        use crate::workspace::kubernetes::tests::{LIVE, live_provider, pods_left};
        let _one = LIVE.lock().await;
        let provider = live_provider().await;
        bash_on_ssh("kube-live-bash", Arc::new(provider.clone())).await;
        assert_eq!(pods_left(&provider).await, Vec::<String>::new());
    }

    /// A lane that goes looking for the credential the commit was fetched
    /// with finds nothing, and its run keeps nothing of it: no transcript
    /// and no tool call holds the token (#191, §8.4).
    #[tokio::test]
    async fn no_transcript_or_tool_call_holds_the_git_credential() {
        use crate::workspace::remote::tests::LocalRunner;
        let runner = LocalRunner::new("henk-review-credential");
        let provider = crate::workspace::ssh::SshProvider::with_runner(
            Arc::clone(&runner) as Arc<dyn crate::workspace::remote::Runner>
        );
        let config = reviewing_config("")
            .replace(", { name = \"lane-b\", model = \"m\" }", "")
            .replace("[[\"make\", \"deps\"]]", "[[\"true\"]]");
        let looking =
            "env; git config --list --show-origin; cat .git/config; grep -rn ghs_ .git . || true";
        let model = ScriptedClient::new(
            "scripted",
            [
                call("bash", serde_json::json!({"command": looking})),
                call("search", serde_json::json!({"pattern": "ghs_"})),
                done(),
                done(),
                done(),
            ],
        );
        let (f, _remote) = reviewed_on("credential", &config, model, Arc::new(provider)).await;
        let run = RunId::parse("r-credential").unwrap();
        run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();

        let store = &f.app.store;
        let listed = store.transcripts(&run).await.unwrap();
        assert!(listed.iter().any(|t| t.session == "lane-a"), "{listed:?}");
        for summary in &listed {
            let stored = store
                .transcript(&run, &summary.session)
                .await
                .unwrap()
                .unwrap();
            assert!(
                !stored.body.contains(GIT_TOKEN),
                "{} holds the credential",
                summary.session
            );
        }
        let lane = store.transcript(&run, "lane-a").await.unwrap().unwrap();
        assert!(
            lane.body.contains("PATH=") && lane.body.contains("core.repositoryformatversion"),
            "the lane's bash did read its environment and git config: {}",
            lane.body
        );
        let calls = store.tool_calls(&run).await.unwrap();
        assert!(calls.len() >= 2, "{calls:?}");
        assert!(!format!("{calls:?}").contains(GIT_TOKEN));
        assert!(
            !format!("{:?}", store.events(&run).await.unwrap()).contains(GIT_TOKEN),
            "nor the timeline"
        );
        assert_eq!(std::fs::read_dir(runner.base()).unwrap().count(), 0);
    }

    /// Lane-a on model `aa` and lane-b on model `bb`, with `extra` added
    /// to the configuration, and a writer that takes posts. `aa` is the
    /// fixture's model, `bb` its second.
    async fn two_lane_review(
        extra: &str,
        aa: ScriptedClient,
        bb: ScriptedClient,
    ) -> (Fixture, Arc<ScriptedClient>) {
        let config = CONFIG
            .replace(
                "lanes = [{ name = \"lane-a\", model = \"m\" }]\n",
                &format!(
                    "lanes = [{{ name = \"lane-a\", model = \"m\" }}, {{ name = \"lane-b\", model = \"m2\" }}]\n{extra}"
                ),
            )
            .replace(
                "[review]\n",
                "[models.m2]\nprovider = \"open_ai\"\nbase_url = \"https://x.test/v1\"\napi_key_env = \"UNUSED\"\nmodel = \"scripted\"\n[review]\n",
            );
        let mut f = fixture_on(
            DIFF,
            aa,
            &config,
            SHA,
            Arc::new(crate::workspace::host::HostProvider),
            None,
        )
        .await;
        let bb = Arc::new(bb);
        f.app
            .models
            .insert("m2".to_owned(), Arc::clone(&bb) as Arc<dyn ModelClient>);
        let writer = Arc::new(FakeWriter {
            head: SHA.to_owned(),
            patches: henk_domain::diff::split_unified(DIFF),
            accept_posts: true,
            ..FakeWriter::default()
        });
        f.app.test_writer = Some(Arc::clone(&writer) as Arc<dyn PlatformWriter>);
        f.writer = writer;
        (f, bb)
    }

    fn draft(line: u32, body: &str) -> Result<Completion, henk_llm::LlmError> {
        call(
            "post_finding",
            serde_json::json!({"path": "src/a.rs", "line": line, "body": body}),
        )
    }

    fn verdict(
        id: &str,
        verdict: &str,
        same_as: Option<&str>,
    ) -> Result<Completion, henk_llm::LlmError> {
        let mut args =
            serde_json::json!({"id": id, "verdict": verdict, "reason": "src/a.rs:2 shows it."});
        if let Some(of) = same_as {
            args["same_as"] = serde_json::json!(of);
        }
        call("give_verdict", args)
    }

    async fn decisions(f: &Fixture, run: &RunId) -> Vec<(String, String, String, String)> {
        f.app
            .store
            .drafts(run)
            .await
            .unwrap()
            .into_iter()
            .map(|d| {
                let decision = d.decision.unwrap();
                (
                    d.lane,
                    decision.verdict.as_str().to_owned(),
                    decision.checker,
                    decision.same_as,
                )
            })
            .collect()
    }

    /// #189: two lanes on different models report one problem. Nothing is
    /// posted while they work; afterwards each draft is checked by the
    /// other lane's model, one session each, and the repeat is merged.
    #[tokio::test]
    async fn a_review_is_announced_as_it_happens_for_the_live_view() {
        let aa = ScriptedClient::new(
            "aa",
            [
                draft(2, "x changes from 1 to 2 and no test covers it."),
                done(),
                done(),
                verdict("d2", "same_as", Some("d1")),
                done(),
            ],
        );
        let bb = ScriptedClient::new(
            "bb",
            [
                draft(3, "The new value of x is untested."),
                done(),
                done(),
                verdict("d1", "confirmed", None),
                done(),
            ],
        )
        .with_delay(Duration::from_millis(100));
        let (f, _bb) = two_lane_review(
            "[review.fact_check]\nmodel = \"m\"\nbackup_model = \"m2\"\n",
            aa,
            bb,
        )
        .await;
        let run = RunId::parse("r-live").unwrap();
        let mut changes = f.app.feed.subscribe();
        run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();

        let mut seen = Vec::new();
        while let Ok(change) = changes.try_recv() {
            assert_eq!(change.run, run);
            seen.push(change);
        }
        let first = seen.first().unwrap();
        assert!(
            matches!(&first.kind, crate::live::ChangeKind::Run(r) if r.status == henk_store::RunStatus::Running),
            "the run's start comes first"
        );
        let last = seen.last().unwrap();
        assert!(
            matches!(&last.kind, crate::live::ChangeKind::Run(r) if r.status == henk_store::RunStatus::Finished),
            "its end comes last"
        );
        let drafts: Vec<(String, Option<String>)> = seen
            .iter()
            .filter_map(|c| match &c.kind {
                crate::live::ChangeKind::Draft(d) => Some((
                    d.draft.clone(),
                    d.decision.as_ref().map(|x| x.verdict.as_str().to_owned()),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            drafts,
            [
                ("d1".to_owned(), None),
                ("d2".to_owned(), None),
                ("d1".to_owned(), Some("confirmed".to_owned())),
                ("d2".to_owned(), Some("same_as".to_owned())),
            ],
            "queued while the lanes ran, decided after"
        );
        let started = seen
            .iter()
            .position(|c| matches!(&c.kind, crate::live::ChangeKind::Lanes(l) if l.iter().any(|l| l.name == "lane-a")))
            .unwrap();
        let first_draft = seen
            .iter()
            .position(|c| matches!(c.kind, crate::live::ChangeKind::Draft(_)))
            .unwrap();
        assert!(started < first_draft, "the lanes start before they draft");
        assert!(
            seen.iter()
                .any(|c| matches!(c.kind, crate::live::ChangeKind::ToolCall(_))),
            "tool calls are announced"
        );
        assert!(
            seen.windows(2).all(|w| w[0].seq < w[1].seq),
            "in feed order"
        );
    }

    #[tokio::test]
    async fn drafts_are_checked_by_another_model_after_the_lanes_and_a_repeat_is_merged() {
        let aa = ScriptedClient::new(
            "aa",
            [
                draft(2, "x changes from 1 to 2 and no test covers it."),
                done(),
                done(),
                verdict("d2", "same_as", Some("d1")),
                done(),
            ],
        );
        // Later than lane-a, so its draft is d2.
        let bb = ScriptedClient::new(
            "bb",
            [
                draft(3, "The new value of x is untested."),
                done(),
                done(),
                verdict("d1", "confirmed", None),
                done(),
            ],
        )
        .with_delay(Duration::from_millis(100));
        let (f, bb) = two_lane_review(
            "[review.fact_check]\nmodel = \"m\"\nbackup_model = \"m2\"\n",
            aa,
            bb,
        )
        .await;
        let run = RunId::parse("r-drafts").unwrap();
        run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();

        let posts = f.writer.posts.lock().unwrap().clone();
        assert_eq!(posts.len(), 1, "one problem, one comment: {posts:?}");
        assert_eq!(posts[0].1, 2);
        let marker = Marker::parse(&posts[0].2).unwrap();
        assert_eq!(marker.model.as_str(), "aa", "lane-a wrote it");
        assert_eq!(
            marker.checked_by.unwrap().as_str(),
            "bb",
            "lane-b's model checked it"
        );
        assert_eq!(
            decisions(&f, &run).await,
            [
                (
                    "lane-a".into(),
                    "confirmed".into(),
                    "bb".into(),
                    String::new()
                ),
                ("lane-b".into(), "same_as".into(), "aa".into(), "d1".into()),
            ]
        );
        let actions: Vec<(String, String)> = f
            .app
            .store
            .findings(&run)
            .await
            .unwrap()
            .into_iter()
            .map(|r| (r.lane, r.action))
            .collect();
        assert_eq!(
            actions,
            [
                ("lane-a".to_owned(), "posted".to_owned()),
                ("lane-b".to_owned(), "merged".to_owned())
            ]
        );
        let sessions: Vec<String> = f
            .app
            .store
            .lanes(&run)
            .await
            .unwrap()
            .into_iter()
            .map(|l| l.name)
            .filter(|n| n.starts_with("check-"))
            .collect();
        assert_eq!(
            sessions,
            ["check-1", "check-2"],
            "one session per checking model"
        );
        let check = bb.requests().last().unwrap().messages[0].text();
        assert!(
            check.contains("## d1: a new finding on src/a.rs:2"),
            "{check}"
        );
        assert!(
            !check.contains("## d2"),
            "bb never checks its own lane's draft first"
        );

        // The lanes had ended before anything was confirmed or posted.
        let events: Vec<String> = f
            .app
            .store
            .events(&run)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.message)
            .collect();
        let at = |needle: &str| {
            events
                .iter()
                .position(|e| e.contains(needle))
                .unwrap_or_else(|| panic!("{needle} in {events:?}"))
        };
        assert!(at("lane-b:") < at("confirmed by bb"), "{events:?}");
        assert!(at("lane-a:") < at("confirmed by bb"), "{events:?}");
    }

    /// A rejected draft is not posted, and a review without a fact-check
    /// posts its drafts after the lanes as they are.
    #[tokio::test]
    async fn a_rejected_draft_is_not_posted_and_without_a_checker_drafts_go_out() {
        let aa = ScriptedClient::new("aa", [draft(2, "x is wrong."), done(), done()]);
        let bb = ScriptedClient::new(
            "bb",
            [done(), done(), verdict("d1", "rejected", None), done()],
        );
        let (f, _) = two_lane_review(
            "[review.fact_check]\nmodel = \"m\"\nbackup_model = \"m2\"\n",
            aa,
            bb,
        )
        .await;
        let run = RunId::parse("r-rejected").unwrap();
        run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();
        assert!(f.writer.posts.lock().unwrap().is_empty());
        let stages = stage_list(&f.app, &run).await;
        assert!(
            stages.contains(&(
                henk_store::Stage::FactCheck,
                StageState::Done,
                "1 draft: 0 confirmed, 1 rejected".into()
            )),
            "{stages:?}"
        );
        assert_eq!(
            decisions(&f, &run).await,
            [(
                "lane-a".into(),
                "rejected".into(),
                "bb".into(),
                String::new()
            )]
        );

        let aa = ScriptedClient::new("aa", [draft(2, "x is wrong."), done(), done()]);
        let bb = ScriptedClient::new("bb", [done(), done()]);
        let (f, _) = two_lane_review("", aa, bb).await;
        let run = RunId::parse("r-unchecked").unwrap();
        run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();
        let posts = f.writer.posts.lock().unwrap().clone();
        assert_eq!(posts.len(), 1);
        assert_eq!(Marker::parse(&posts[0].2).unwrap().checked_by, None);
        assert_eq!(
            decisions(&f, &run).await,
            [(
                "lane-a".into(),
                "not_checked".into(),
                String::new(),
                String::new()
            )]
        );
        let actions: Vec<String> = f
            .app
            .store
            .findings(&run)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.action)
            .collect();
        assert_eq!(
            actions,
            ["posted"],
            "not marked unverified: nothing was configured to check"
        );
    }

    /// Three drafts on one file, one per line of its diff: one check
    /// session, with the diff once. Checked one by one, each check got the
    /// diff again (#189).
    #[tokio::test]
    async fn drafts_on_one_file_are_checked_in_one_session_with_the_diff_once() {
        let mut lane = Vec::new();
        for line in 1..=3 {
            lane.push(draft(line, &format!("Problem on line {line}.")));
        }
        lane.extend([done(), done()]);
        let aa = ScriptedClient::new("aa", lane);
        let mut check: Vec<_> = Vec::new();
        let (f, bb) = {
            check.extend([done(), done()]);
            for n in 1..=3 {
                check.push(verdict(&format!("d{n}"), "confirmed", None));
            }
            check.push(done());
            two_lane_review(
                "[review.fact_check]\nmodel = \"m2\"\n",
                aa,
                ScriptedClient::new("bb", check),
            )
            .await
        };
        let run = RunId::parse("r-six").unwrap();
        run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();
        let checks: Vec<String> = bb
            .requests()
            .iter()
            .filter(|r| r.tools.iter().any(|t| t.name.as_str() == "give_verdict"))
            .map(|r| r.messages[0].text())
            .collect();
        assert!(!checks.is_empty());
        let openings: std::collections::BTreeSet<&String> = checks.iter().collect();
        assert_eq!(openings.len(), 1, "one check session");
        let opening = openings.into_iter().next().unwrap();
        assert_eq!(
            opening.matches("The diff of src/a.rs:").count(),
            1,
            "{opening}"
        );
        assert_eq!(opening.matches("## d").count(), 3, "{opening}");
    }

    /// A review cancelled while its lanes work posts none of their drafts.
    #[tokio::test]
    async fn a_cancelled_review_posts_no_drafts() {
        let aa = ScriptedClient::new("aa", [draft(2, "x is wrong."), done(), done()])
            .with_delay(Duration::from_millis(400));
        let bb = ScriptedClient::new("bb", [done(), done()]);
        let (f, _) = two_lane_review("[review.fact_check]\nmodel = \"m2\"\n", aa, bb).await;
        let run = RunId::parse("r-cancel-drafts").unwrap();
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(600)).await;
            trigger.cancel();
        });
        let _ = run_review(&f.app, request(&run), cancel).await;
        assert!(f.writer.posts.lock().unwrap().is_empty());
        assert_eq!(
            decisions(&f, &run).await,
            [(
                "lane-a".into(),
                "cancelled".into(),
                String::new(),
                String::new()
            )]
        );
    }

    fn many_done() -> ScriptedClient {
        ScriptedClient::new("scripted", (0..12).map(|_| done()))
    }

    #[tokio::test]
    async fn every_lane_and_the_fact_checker_review_in_their_own_workspace_at_the_commit() {
        let r = reviewed("henk-review-ws", &reviewing_config(""), many_done(), 0).await;
        let run = RunId::parse("r-ws").unwrap();
        run_review(&r.f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();

        let fake = &r.peek.inner;
        assert_eq!(fake.opened(), 3, "lane-a, lane-b and the fact-checker");
        assert_eq!(fake.live(), 0, "every workspace is closed");
        assert_eq!(fake.unclosed(), 0, "closed, not just dropped");
        assert_eq!(
            *r.peek.seen.lock().unwrap(),
            vec!["fn main() {\n    let x = 1;\n}\n"; 3],
            "the reviewed commit, not the branch head"
        );
        assert_eq!(
            *fake.ran.lock().unwrap(),
            vec!["make deps"; 3],
            "each one set up"
        );
        let events = r.f.app.store.events(&run).await.unwrap();
        assert!(
            events.iter().any(|e| e.level == "info"
                && e.message
                    .starts_with("review workspaces: 3 of 3 ready on ssh at ")),
            "{events:?}"
        );
        for lane in ["lane-a", "lane-b", "fact-check"] {
            assert!(
                events.iter().any(|e| e
                    .message
                    .starts_with(&format!("{lane}: exec make deps exit 0"))),
                "setup is on the timeline, by lane: {events:?}"
            );
        }
    }

    fn call(name: &str, arguments: serde_json::Value) -> Result<Completion, henk_llm::LlmError> {
        Ok(Completion {
            message: henk_llm::ChatMessage {
                role: henk_llm::Role::Assistant,
                blocks: vec![henk_llm::Block::ToolCall(henk_llm::ToolCall {
                    id: format!("call-{name}"),
                    name: name.to_owned(),
                    arguments: henk_llm::ToolArguments::Parsed(arguments),
                })],
            },
            stop: StopReason::ToolUse,
            usage: Usage::default(),
        })
    }

    /// The tool names of the first request, and every tool result the
    /// model was sent.
    fn seen(model: &ScriptedClient) -> (Vec<String>, Vec<String>, String) {
        let requests = model.requests();
        let first = requests.first().unwrap();
        let names = first.tools.iter().map(|t| t.name.to_string()).collect();
        let results = requests
            .iter()
            .flat_map(|r| r.messages.iter())
            .flat_map(|m| m.blocks.iter())
            .filter_map(|b| match b {
                henk_llm::Block::ToolResult(r) => Some(r.content.clone()),
                _ => None,
            })
            .collect();
        (names, results, first.system.clone().unwrap_or_default())
    }

    #[tokio::test]
    async fn a_lane_with_a_workspace_searches_and_reads_the_reviewed_commit() {
        let config = reviewing_config("").replace(", { name = \"lane-b\", model = \"m\" }", "");
        let model = ScriptedClient::new(
            "scripted",
            [
                call(
                    "search",
                    serde_json::json!({"pattern": "let x = \\d", "glob": "*.rs"}),
                ),
                call(
                    "read_file",
                    serde_json::json!({"path": "src/a.rs", "start_line": 2, "end_line": 2}),
                ),
                done(),
                done(),
                done(),
            ],
        );
        let r = reviewed("henk-review-tools", &config, model, 0).await;
        let run = RunId::parse("r-tools").unwrap();
        run_review(&r.f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();
        let (names, results, system) = seen(&r.f.model);
        for tool in ["list_files", "read_file", "search", "get_file_diff"] {
            assert!(names.iter().any(|n| n == tool), "{tool}: {names:?}");
        }
        assert!(system.contains("# Your copy of the tree"), "{system}");
        assert!(
            results.iter().any(|r| r == "src/a.rs:2: let x = 1;\n"),
            "the reviewed commit, not the moved branch: {results:?}"
        );
        assert!(
            results
                .iter()
                .any(|r| r == "    2|     let x = 1;\n[3 lines; continue with start_line = 3]\n"),
            "{results:?}"
        );
        // Every call of the lane is on the run, under the lane's name (#190).
        let calls = r.f.app.store.tool_calls(&run).await.unwrap();
        let lane_a: Vec<(&str, &str, &str)> = calls
            .iter()
            .filter(|c| c.session == "lane-a")
            .map(|c| (c.tool.as_str(), c.origin.as_str(), c.outcome.as_str()))
            .collect();
        assert_eq!(
            lane_a,
            [
                ("search", "workspace", "ok"),
                ("read_file", "workspace", "ok")
            ],
            "{calls:?}"
        );
        assert_eq!(r.peek.inner.live(), 0);
    }

    #[tokio::test]
    async fn a_lane_runs_a_command_and_the_timeline_says_which_lane() {
        let config = reviewing_config("").replace(", { name = \"lane-b\", model = \"m\" }", "");
        let model = ScriptedClient::new(
            "scripted",
            [
                call(
                    "bash",
                    serde_json::json!({"command": "cargo test -q parse"}),
                ),
                done(),
                done(),
                done(),
            ],
        );
        let mut inner = crate::workspace::fake::FakeProvider::default();
        for (command, output) in [
            ("make deps", "deps ready"),
            ("bash -c cargo test -q parse", "1 passed"),
        ] {
            inner.script.insert(
                command.to_owned(),
                crate::workspace::fake::Scripted {
                    output: output.to_owned(),
                    ..crate::workspace::fake::Scripted::default()
                },
            );
        }
        let peek = Arc::new(Peek {
            inner,
            seen: std::sync::Mutex::new(Vec::new()),
        });
        let (f, _remote) = reviewed_on(
            "henk-review-bash",
            &config,
            model,
            Arc::clone(&peek) as Arc<dyn crate::workspace::WorkspaceProvider>,
        )
        .await;
        let run = RunId::parse("r-bash").unwrap();
        run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();
        let (names, results, system) = seen(&f.model);
        assert!(names.iter().any(|n| n == "bash"), "{names:?}");
        assert!(
            system.contains("`bash` runs a command in your copy"),
            "{system}"
        );
        assert!(
            results
                .iter()
                .any(|r| r.starts_with("exit 0 in ") && r.ends_with("\n1 passed\n")),
            "{results:?}"
        );
        let events = f.app.store.events(&run).await.unwrap();
        assert!(
            events.iter().any(|e| e
                .message
                .starts_with("lane-a: exec bash -c cargo test -q parse exit 0")),
            "henk runs show lists the lane's command: {events:?}"
        );
        assert_eq!(peek.inner.live(), 0);
    }

    #[tokio::test]
    async fn a_lane_without_a_workspace_keeps_its_tools() {
        let f = fixture(DIFF, ScriptedClient::new("scripted", [done(), done()])).await;
        let run = RunId::parse("r-notools").unwrap();
        run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();
        let (names, _, system) = seen(&f.model);
        assert!(
            !names
                .iter()
                .any(|n| n == "list_files" || n == "search" || n == "bash"),
            "{names:?}"
        );
        assert!(!system.contains("# Your copy of the tree"));
    }

    #[tokio::test]
    async fn without_review_in_the_profile_a_review_opens_no_workspace() {
        let config = reviewing_config("").replace("review = true\n", "");
        let r = reviewed("henk-review-nows", &config, many_done(), 0).await;
        let run = RunId::parse("r-nows").unwrap();
        run_review(&r.f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(r.peek.inner.opened(), 0);
        let events = r.f.app.store.events(&run).await.unwrap();
        assert!(
            !events
                .iter()
                .any(|e| e.message.starts_with("review workspaces")),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn a_failed_setup_costs_the_lanes_their_workspaces_not_the_review() {
        let r = reviewed(
            "henk-review-badsetup",
            &reviewing_config(""),
            many_done(),
            2,
        )
        .await;
        let run = RunId::parse("r-badsetup").unwrap();
        let report = run_review(&r.f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();
        assert!(report.outcome.unwrap().completed());
        assert_eq!(r.peek.inner.opened(), 3);
        assert_eq!(
            r.peek.inner.live(),
            0,
            "a workspace whose setup failed is closed"
        );
        assert_eq!(r.peek.inner.unclosed(), 0);
        let events = r.f.app.store.events(&run).await.unwrap();
        let warning = events
            .iter()
            .find(|e| e.level == "warn" && e.message.starts_with("review workspaces: 0 of 3"))
            .unwrap_or_else(|| panic!("{events:?}"));
        for lane in ["lane-a", "lane-b", "fact-check"] {
            assert!(
                warning.message.contains(&format!(
                    "no workspace for {lane}: setup step `make deps` exited with 2"
                )),
                "{}",
                warning.message
            );
        }
    }

    #[tokio::test]
    async fn a_review_without_its_source_goes_on_without_workspaces() {
        let config = reviewing_config("");
        let model = many_done();
        let peek = Arc::new(Peek::default());
        let f = fixture_on(
            DIFF,
            model,
            &config,
            SHA,
            Arc::clone(&peek) as Arc<dyn crate::workspace::WorkspaceProvider>,
            None,
        )
        .await;
        let run = RunId::parse("r-nosource").unwrap();
        let report = run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();
        assert!(report.outcome.unwrap().completed());
        assert_eq!(peek.inner.opened(), 0);
        let events = f.app.store.events(&run).await.unwrap();
        assert!(
            events.iter().any(|e| e.level == "warn"
                && e.message.starts_with(
                    "review workspaces: none, the reviewed commit could not be checked out"
                )),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn a_cancelled_review_closes_every_workspace() {
        let run = RunId::parse("r-ws-cancel").unwrap();
        let model = ScriptedClient::new("scripted", (0..12).map(|_| done()))
            .with_delay(Duration::from_secs(30));
        let r = reviewed("henk-review-ws-cancel", &reviewing_config(""), model, 0).await;
        let cancel = r.f.app.shutdown.child_token();
        let _cancellable = r.f.app.cancels.register(run.clone(), cancel.clone());
        let (cancels, cancelled) = (r.f.app.cancels.clone(), run.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            assert!(cancels.cancel(&cancelled, "github:1234".to_owned()));
        });
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            run_review(&r.f.app, request(&run), cancel),
        )
        .await
        .expect("a cancelled review ends promptly");
        assert!(result.is_err_and(|e| e.is::<CancelledBy>()));
        assert_eq!(r.peek.inner.opened(), 3);
        assert_eq!(r.peek.inner.live(), 0);
        assert_eq!(r.peek.inner.unclosed(), 0, "closed, not just dropped");
    }

    #[tokio::test]
    async fn a_review_cancelled_during_setup_stops_at_once_and_keeps_no_workspace() {
        let run = RunId::parse("r-ws-cancel-setup").unwrap();
        let r = reviewed_slow(
            "henk-review-ws-cancel-setup",
            &reviewing_config(""),
            many_done(),
            0,
            Duration::from_secs(45),
        )
        .await;
        let cancel = r.f.app.shutdown.child_token();
        let _cancellable = r.f.app.cancels.register(run.clone(), cancel.clone());
        let (cancels, cancelled) = (r.f.app.cancels.clone(), run.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert!(cancels.cancel(&cancelled, "github:1234".to_owned()));
        });
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            run_review(&r.f.app, request(&run), cancel),
        )
        .await
        .expect("the setup is not waited for");
        assert!(result.is_err_and(|e| e.is::<CancelledBy>()));
        assert_eq!(r.peek.inner.opened(), 3);
        assert_eq!(r.peek.inner.live(), 0, "dropped mid-setup, so destroyed");
    }

    #[tokio::test]
    async fn a_lane_out_of_time_still_closes_its_workspace() {
        let run = RunId::parse("r-ws-timeout").unwrap();
        let model = ScriptedClient::new("scripted", (0..12).map(|_| done()))
            .with_delay(Duration::from_secs(30));
        let config = reviewing_config("").replace(
            "[review.fact_check]",
            "lane_timeout_secs = 1\n[review.fact_check]",
        );
        let r = reviewed("henk-review-ws-timeout", &config, model, 0).await;
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            run_review(&r.f.app, request(&run), CancellationToken::new()),
        )
        .await
        .expect("the lanes stop at their time limit");
        assert!(result.is_ok());
        assert_eq!(r.peek.inner.opened(), 3);
        assert_eq!(r.peek.inner.live(), 0);
        assert_eq!(r.peek.inner.unclosed(), 0, "closed, not just dropped");
    }

    /// The pull request of a review loop (#284), on a local remote: its
    /// head is whatever `feature` holds now, so a push moves it.
    struct LoopHub {
        remote: std::path::PathBuf,
        /// The repository the pull request's branch is in: `o/r` unless it
        /// comes from a fork.
        head_repo: &'static str,
    }

    #[async_trait::async_trait]
    impl henk_platform::address::AddressWriter for LoopHub {
        async fn pull_facts(
            &self,
            _: &ReviewTarget,
        ) -> Result<henk_platform::address::PullFacts, henk_platform::PlatformError> {
            let (head, _) = crate::git::tests::remote_feature(&self.remote).await;
            Ok(henk_platform::address::PullFacts {
                push: henk_domain::address::PushFacts {
                    open: true,
                    head_repo: Some(self.head_repo.to_owned()),
                    base_repo: "o/r".to_owned(),
                    head_ref: "feature".to_owned(),
                    default_branch: "main".to_owned(),
                    head_protected: false,
                },
                head: CommitSha::parse(&head).unwrap(),
                remote: self.remote.to_string_lossy().into_owned(),
            })
        }
        async fn open_threads(
            &self,
            _: &ReviewTarget,
        ) -> Result<Vec<henk_platform::address::OpenThread>, henk_platform::PlatformError> {
            Err(unused())
        }
        async fn git_credential(
            &self,
        ) -> Result<Option<henk_platform::address::GitCredential>, henk_platform::PlatformError>
        {
            Ok(None)
        }
        async fn repo_head(
            &self,
            _: &henk_domain::allowlist::RepoRef,
        ) -> Result<henk_platform::address::RepoHead, henk_platform::PlatformError> {
            Err(unused())
        }
        async fn commit_identity(
            &self,
        ) -> Result<henk_platform::address::CommitIdentity, henk_platform::PlatformError> {
            Ok(henk_platform::address::CommitIdentity {
                name: "meneer-henk[bot]".to_owned(),
                email: "1+meneer-henk[bot]@users.noreply.github.com".to_owned(),
            })
        }
        fn noreply_host(&self) -> Result<String, henk_platform::PlatformError> {
            Err(unused())
        }
        async fn user_login(&self, _: u64) -> Result<String, henk_platform::PlatformError> {
            Err(unused())
        }
        fn commit_url(&self, _: &ReviewTarget, _: &str) -> String {
            String::new()
        }
        async fn reply_in_thread(
            &self,
            _: &ReviewTarget,
            _: &henk_platform::address::OpenThread,
            _: &str,
        ) -> Result<henk_platform::PostedComment, henk_platform::PlatformError> {
            Err(unused())
        }
        async fn resolve_thread(
            &self,
            _: &ReviewTarget,
            _: &str,
        ) -> Result<(), henk_platform::PlatformError> {
            Err(unused())
        }
        async fn post_comment(
            &self,
            _: &ReviewTarget,
            _: &str,
        ) -> Result<henk_platform::PostedComment, henk_platform::PlatformError> {
            Err(unused())
        }
    }

    /// A backend that cannot open a workspace.
    struct NoWorkspaces;

    #[async_trait::async_trait]
    impl crate::workspace::WorkspaceProvider for NoWorkspaces {
        async fn open(
            &self,
            _: &std::path::Path,
            _: &henk_domain::workspace::Profile,
        ) -> Result<Arc<dyn crate::workspace::Workspace>, crate::workspace::WorkspaceError>
        {
            Err(crate::workspace::WorkspaceError::Refused(
                "the sandbox host is down".to_owned(),
            ))
        }
    }

    /// Opens host workspaces in which a search also writes a file, as a
    /// reviewer that changes the workspace would.
    struct Scribbling;

    #[async_trait::async_trait]
    impl crate::workspace::WorkspaceProvider for Scribbling {
        async fn open(
            &self,
            dir: &std::path::Path,
            profile: &henk_domain::workspace::Profile,
        ) -> Result<Arc<dyn crate::workspace::Workspace>, crate::workspace::WorkspaceError>
        {
            let inner = crate::workspace::host::HostProvider
                .open(dir, profile)
                .await?;
            Ok(Arc::new(Scribbler(inner)))
        }
    }

    struct Scribbler(Arc<dyn crate::workspace::Workspace>);

    #[async_trait::async_trait]
    impl crate::workspace::Workspace for Scribbler {
        async fn exec(
            &self,
            argv: &[String],
            cwd: &henk_domain::address::WorkspacePath,
            timeout: Duration,
        ) -> Result<crate::workspace::ExecResult, crate::workspace::WorkspaceError> {
            self.0.exec(argv, cwd, timeout).await
        }

        async fn read(
            &self,
            path: &henk_domain::address::WorkspacePath,
            max_bytes: u64,
        ) -> Result<Vec<u8>, crate::workspace::WorkspaceError> {
            self.0.read(path, max_bytes).await
        }

        async fn write(
            &self,
            path: &henk_domain::address::WorkspacePath,
            content: &[u8],
        ) -> Result<(), crate::workspace::WorkspaceError> {
            self.0.write(path, content).await
        }

        async fn list(
            &self,
            dir: &henk_domain::address::WorkspacePath,
            only: Option<&henk_domain::ignore::PathFilter>,
            cap: usize,
        ) -> Result<Vec<String>, crate::workspace::WorkspaceError> {
            self.0.list(dir, only, cap).await
        }

        async fn search(
            &self,
            dir: &henk_domain::address::WorkspacePath,
            pattern: &crate::workspace::Pattern,
            only: Option<&henk_domain::ignore::PathFilter>,
            context: usize,
            max_file_bytes: u64,
            cap: usize,
        ) -> Result<Vec<crate::workspace::Hit>, crate::workspace::WorkspaceError> {
            let scribble = henk_domain::address::WorkspacePath::parse("scribble.txt").unwrap();
            self.0.write(&scribble, b"the reviewer was here").await?;
            self.0
                .search(dir, pattern, only, context, max_file_bytes, cap)
                .await
        }

        async fn export(
            &self,
        ) -> Result<Vec<crate::workspace::Exported>, crate::workspace::WorkspaceError> {
            self.0.export().await
        }

        async fn baseline(&self) -> Result<(), crate::workspace::WorkspaceError> {
            self.0.baseline().await
        }

        async fn close(&self) {
            self.0.close().await;
        }
    }

    fn say(text: &str) -> Result<Completion, henk_llm::LlmError> {
        Ok(Completion {
            message: henk_llm::ChatMessage::assistant(text),
            stop: StopReason::EndTurn,
            usage: Usage::default(),
        })
    }

    fn edit(old: &str, new: &str) -> Result<Completion, henk_llm::LlmError> {
        call(
            "edit_file",
            serde_json::json!({"path": "src/a.rs", "old": old, "new": new}),
        )
    }

    /// [`CONFIG`] with a review loop of reviewer `r` and fixer `f` (#284).
    fn loop_config() -> String {
        loop_config_with("")
    }

    /// [`loop_config`] with more `[review.loop]` settings.
    fn loop_config_with(settings: &str) -> String {
        let models = "[models.r]\nprovider = \"open_ai\"\nbase_url = \"https://x.test/v1\"\napi_key_env = \"UNUSED\"\nmodel = \"r\"\n[models.f]\nprovider = \"open_ai\"\nbase_url = \"https://x.test/v1\"\napi_key_env = \"UNUSED\"\nmodel = \"f\"\n";
        CONFIG.replace("[review]\n", &format!("{models}[review]\n"))
            + "[review.loop]\nreviewer = \"r\"\nfixer = \"f\"\n"
            + settings
            + "[address]\nmodel = \"f\"\nrequester_id = 3\n"
    }

    /// The fixture of a looping test: the app, the reviewer, the fixer and
    /// the remote, which is returned to be kept.
    type Looping = (
        Fixture,
        Arc<ScriptedClient>,
        Arc<ScriptedClient>,
        crate::git::ScratchDir,
    );

    /// A reviewer `r` and a fixer `f`, each scripted, on a fresh local
    /// remote served by [`LoopHub`]; the remote is returned to be kept.
    async fn looping(
        name: &str,
        reviewer: ScriptedClient,
        fixer: ScriptedClient,
        provider: Arc<dyn crate::workspace::WorkspaceProvider>,
    ) -> Looping {
        looping_with(name, "", reviewer, fixer, provider).await
    }

    /// [`looping`] with more `[review.loop]` settings.
    async fn looping_with(
        name: &str,
        settings: &str,
        reviewer: ScriptedClient,
        fixer: ScriptedClient,
        provider: Arc<dyn crate::workspace::WorkspaceProvider>,
    ) -> Looping {
        let lanes = ScriptedClient::new("m", []);
        looping_on(name, "o/r", settings, lanes, reviewer, fixer, provider).await
    }

    /// [`looping`] on a pull request whose branch is in `head_repo`, with
    /// `lanes` answering the lane review.
    async fn looping_from(
        name: &str,
        head_repo: &'static str,
        lanes: ScriptedClient,
        reviewer: ScriptedClient,
        fixer: ScriptedClient,
        provider: Arc<dyn crate::workspace::WorkspaceProvider>,
    ) -> Looping {
        looping_on(name, head_repo, "", lanes, reviewer, fixer, provider).await
    }

    /// [`looping_from`] with more `[review.loop]` settings.
    async fn looping_on(
        name: &str,
        head_repo: &'static str,
        settings: &str,
        lanes: ScriptedClient,
        reviewer: ScriptedClient,
        fixer: ScriptedClient,
        provider: Arc<dyn crate::workspace::WorkspaceProvider>,
    ) -> Looping {
        let (remote, head) = crate::git::tests::bare_remote(name).await;
        let config = loop_config_with(settings);
        let hub = Arc::new(LoopHub {
            remote: remote.path().to_path_buf(),
            head_repo,
        });
        let mut f = fixture_on(DIFF, lanes, &config, head.as_str(), provider, Some(hub)).await;
        let (reviewer, fixer) = (Arc::new(reviewer), Arc::new(fixer));
        f.app.models.insert(
            "r".to_owned(),
            Arc::clone(&reviewer) as Arc<dyn ModelClient>,
        );
        f.app
            .models
            .insert("f".to_owned(), Arc::clone(&fixer) as Arc<dyn ModelClient>);
        (f, reviewer, fixer, remote)
    }

    /// The text of every message of a request, in order.
    fn texts(request: &henk_llm::CompletionRequest) -> Vec<String> {
        request
            .messages
            .iter()
            .map(henk_llm::ChatMessage::text)
            .collect()
    }

    fn report(
        line: u32,
        claim: &str,
        reopens: Option<&str>,
    ) -> Result<Completion, henk_llm::LlmError> {
        let mut args = serde_json::json!({
            "path": "src/a.rs",
            "line": line,
            "claim": claim,
            "why": "the caller divides by it",
            "fix": "make it 3",
        });
        if let Some(reopens) = reopens {
            args["reopens"] = serde_json::json!(reopens);
        }
        call("report_finding", args)
    }

    fn finish_round() -> Result<Completion, henk_llm::LlmError> {
        call("finish_round", serde_json::json!({"covered": ["src/a.rs"]}))
    }

    fn judge(finding: &str, verdict: &str, reason: &str) -> Result<Completion, henk_llm::LlmError> {
        call(
            "give_verdict",
            serde_json::json!({"finding": finding, "verdict": verdict, "reason": reason}),
        )
    }

    /// The loop's findings on the run: (draft, verdict, checker, reason,
    /// commit, contests).
    async fn loop_findings(
        app: &App,
        run: &RunId,
    ) -> Vec<(String, String, String, String, String, String)> {
        app.store
            .drafts(run)
            .await
            .unwrap()
            .into_iter()
            .map(|d| {
                assert_eq!(d.kind, henk_store::LOOP_FINDING_KIND);
                let decision = d.decision.unwrap();
                (
                    d.draft,
                    decision.verdict.as_str().to_owned(),
                    decision.checker,
                    decision.reason,
                    decision.comment_id,
                    d.target,
                )
            })
            .collect()
    }

    /// Nothing at all on the pull request: no comment, line comment,
    /// rewrite or resolved thread (#285).
    fn assert_nothing_posted(writer: &FakeWriter) {
        assert!(writer.replies.lock().unwrap().is_empty(), "comments");
        assert!(writer.posts.lock().unwrap().is_empty(), "line comments");
        assert!(writer.updates.lock().unwrap().is_empty(), "rewrites");
        assert!(
            writer.resolved.lock().unwrap().is_empty(),
            "resolved threads"
        );
    }

    #[tokio::test]
    async fn the_fixer_gives_each_finding_one_verdict_and_nothing_is_posted() {
        let (f, reviewer, fixer, remote) = looping(
            "review-loop",
            ScriptedClient::new(
                "r",
                [
                    // Round 1: two findings.
                    report(2, "x must be 3", None),
                    report(1, "main must return a value", None),
                    finish_round(),
                    say("Done for now."),
                    // Round 2: the rejection of f2, contested.
                    report(
                        1,
                        "main must return a value: the exit code is read",
                        Some("f2"),
                    ),
                    finish_round(),
                    say("Done for now."),
                    // Round 3: f2 again is refused; nothing new.
                    report(1, "main must return a value", Some("f2")),
                    finish_round(),
                    say("Nothing more."),
                ],
            ),
            ScriptedClient::new(
                "f",
                [
                    edit("let x = 1;", "let x = 3;"),
                    judge("f1", "fixed", "x is 3 now."),
                    judge("f2", "rejected", "main returns (), see src/a.rs:1."),
                    say("Both settled."),
                    judge("f3", "rejected", "nothing reads an exit code here."),
                    say("Settled."),
                ],
            ),
            Arc::new(crate::workspace::host::HostProvider),
        )
        .await;
        let run = RunId::parse("r-loop").unwrap();
        let report = run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();

        // One commit, for f1; the rejections changed nothing.
        let (head, log) = crate::git::tests::remote_feature(remote.path()).await;
        assert!(log.contains("review loop round 1"), "{log}");
        assert!(f.app.own_pushes.contains(&head));
        // The new head gets the review's check too, not only the commit the
        // review started at: nothing else reviews it.
        let reported = f.writer.finished_commits.lock().unwrap().clone();
        assert_eq!(reported.len(), 2, "{reported:?}");
        assert!(reported.contains(&head), "{reported:?}");
        let findings = loop_findings(&f.app, &run).await;
        let fixed_why = "x is 3 now.".to_owned();
        assert_eq!(
            findings,
            [
                (
                    "f1".into(),
                    "fixed".into(),
                    "fixer".into(),
                    fixed_why,
                    head.clone(),
                    String::new()
                ),
                (
                    "f2".into(),
                    "rejected".into(),
                    "fixer".into(),
                    "main returns (), see src/a.rs:1.".into(),
                    String::new(),
                    String::new()
                ),
                (
                    "f3".into(),
                    "rejected".into(),
                    "fixer".into(),
                    "nothing reads an exit code here.".into(),
                    String::new(),
                    "f2".into()
                ),
            ],
            "one verdict each, with its reason"
        );
        assert_nothing_posted(&f.writer);

        // Each side heard the other in its own conversation.
        let asked = reviewer.requests();
        assert_eq!(asked.len(), 10);
        let round_two = texts(&asked[4]);
        let verdicts = round_two.last().unwrap();
        assert!(
            verdicts.contains("f1 at src/a.rs:2 (x must be 3): fixed in"),
            "{verdicts}"
        );
        assert!(
            verdicts
                .contains("f2 at src/a.rs:1 (main must return a value): rejected: main returns ()"),
            "{verdicts}"
        );
        assert!(
            verdicts.contains("+    let x = 3;"),
            "the pushed patch: {verdicts}"
        );
        // The tool result of round 3's second contest of f2.
        let refused = serde_json::to_string(asked[8].messages.last().unwrap()).unwrap();
        assert!(
            refused.contains("f2 was rejected twice and is closed"),
            "{refused}"
        );
        let fixing = fixer.requests();
        assert_eq!(fixing.len(), 6);
        let second = texts(&fixing[4]);
        assert!(second.iter().any(|t| t == "Both settled."), "{second:?}");
        assert!(
            second.last().unwrap().contains("f3 at src/a.rs:1"),
            "{second:?}"
        );

        let record = f.app.store.run(&run).await.unwrap().unwrap();
        assert_eq!(record.status, RunStatus::Finished);
        assert_eq!(
            (record.loop_stop.as_deref(), record.loop_rounds),
            (Some("converged"), Some(3))
        );
        let summary = report.summary.unwrap();
        assert!(
            summary.contains(
                "ran 3 rounds, settled 3 findings, pushed 1 commit and stopped: converged"
            ),
            "{summary}"
        );
        assert_eq!(f.writer.finished.lock().unwrap()[0].open_findings, 0);
        let lanes = f.app.store.lanes(&run).await.unwrap();
        let names: Vec<_> = lanes.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["fixer", "reviewer"]);
        for session in ["reviewer", "fixer"] {
            assert!(
                f.app
                    .store
                    .transcript(&run, session)
                    .await
                    .unwrap()
                    .is_some(),
                "{session}"
            );
        }
        let runs = f
            .app
            .store
            .count_runs(&henk_store::RunFilter::default())
            .await
            .unwrap();
        assert_eq!(runs, 1, "the loop is one run");
        assert_no_stage_running(&f.app, &run).await;
    }

    #[tokio::test]
    async fn a_finding_the_fixer_gives_no_verdict_ends_unsettled() {
        let (f, _, fixer, remote) = looping(
            "review-loop-silent",
            ScriptedClient::new(
                "r",
                [
                    report(2, "x must be 3", None),
                    finish_round(),
                    say("Done for now."),
                    finish_round(),
                    say("Nothing more."),
                ],
            ),
            ScriptedClient::new("f", [say("Looked."), say("Still looking."), say("Done.")]),
            Arc::new(crate::workspace::host::HostProvider),
        )
        .await;
        let (before, _) = crate::git::tests::remote_feature(remote.path()).await;
        let run = RunId::parse("r-loop-silent").unwrap();
        run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            fixer.requests().len(),
            3,
            "nudged twice, then settled for it"
        );
        let findings = loop_findings(&f.app, &run).await;
        assert_eq!(findings.len(), 1);
        assert_eq!(
            (findings[0].1.as_str(), findings[0].3.as_str()),
            ("unsettled", "the fixer gave no verdict")
        );
        let (after, _) = crate::git::tests::remote_feature(remote.path()).await;
        assert_eq!(before, after, "nothing pushed");
        assert_eq!(f.writer.finished.lock().unwrap()[0].open_findings, 1);
        assert_nothing_posted(&f.writer);
    }

    #[tokio::test]
    async fn a_reviewer_that_reports_a_fixed_finding_again_stops_the_loop() {
        let claim = "x must be 3, the caller divides by it";
        let (f, _, fixer, remote) = looping(
            "review-loop-repeat",
            ScriptedClient::new(
                "r",
                [
                    report(2, claim, None),
                    finish_round(),
                    say("Done for now."),
                    report(2, "X must be 3: the caller divides by it", None),
                    finish_round(),
                    say("Done for now."),
                ],
            ),
            ScriptedClient::new(
                "f",
                [
                    edit("let x = 1;", "let x = 3;"),
                    judge("f1", "fixed", "x is 3 now."),
                    say("Settled."),
                ],
            ),
            Arc::new(crate::workspace::host::HostProvider),
        )
        .await;
        let run = RunId::parse("r-loop-repeat").unwrap();
        let report = run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();
        let record = f.app.store.run(&run).await.unwrap().unwrap();
        assert_eq!(
            (record.loop_stop.as_deref(), record.loop_rounds),
            (Some("repeating_finding"), Some(2)),
            "at the repeat, not at max_rounds (50)"
        );
        assert_eq!(fixer.requests().len(), 3, "the fixer never got the repeat");
        let (head, _) = crate::git::tests::remote_feature(remote.path()).await;
        let findings = loop_findings(&f.app, &run).await;
        assert_eq!(findings[0].1, "fixed");
        assert_eq!(findings[1].1, "unsettled");
        assert_eq!(
            findings[1].3,
            format!("repeats f1, which was fixed in {}", &head[..12])
        );
        let summary = report.summary.unwrap();
        assert!(
            summary.contains("stopped: the reviewer reported f1 again as f2 after it was fixed"),
            "{summary}"
        );
    }

    #[tokio::test]
    async fn a_reviewer_that_changes_the_workspace_and_repeats_a_fixed_finding_fails() {
        let claim = "x must be 3, the caller divides by it";
        let (f, _, fixer, _remote) = looping(
            "review-loop-scribble",
            ScriptedClient::new(
                "r",
                [
                    report(2, claim, None),
                    finish_round(),
                    say("Done for now."),
                    call(
                        "search",
                        serde_json::json!({"pattern": "let x", "glob": "*.rs"}),
                    ),
                    report(2, claim, None),
                    finish_round(),
                    say("Done for now."),
                ],
            ),
            ScriptedClient::new(
                "f",
                [
                    edit("let x = 1;", "let x = 3;"),
                    judge("f1", "fixed", "x is 3 now."),
                    say("Settled."),
                ],
            ),
            Arc::new(Scribbling),
        )
        .await;
        let run = RunId::parse("r-loop-scribble").unwrap();
        let report = run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();
        let record = f.app.store.run(&run).await.unwrap().unwrap();
        assert_eq!(
            record.loop_stop.as_deref(),
            Some("workspace_changed"),
            "{:?}",
            report.summary
        );
        assert_eq!(fixer.requests().len(), 3, "the fixer never got round 2");
        assert!(!report.outcome.unwrap().completed());
    }

    #[tokio::test]
    async fn a_round_the_time_ran_out_before_is_not_counted() {
        let (f, reviewer, fixer, _remote) = looping_with(
            "review-loop-no-time",
            "run_timeout_secs = 0\n",
            ScriptedClient::new("r", []),
            ScriptedClient::new("f", []),
            Arc::new(crate::workspace::host::HostProvider),
        )
        .await;
        let run = RunId::parse("r-loop-no-time").unwrap();
        let report = run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();
        let record = f.app.store.run(&run).await.unwrap().unwrap();
        assert_eq!(
            (record.loop_stop.as_deref(), record.loop_rounds),
            (Some("timeout"), Some(0))
        );
        assert!(reviewer.requests().is_empty() && fixer.requests().is_empty());
        let summary = report.summary.unwrap();
        assert!(summary.contains("ran 0 rounds"), "{summary}");
        assert!(
            summary.contains("before the reviewer's turn in round 1"),
            "{summary}"
        );
    }

    #[tokio::test]
    async fn the_loop_stops_at_its_time_limit() {
        let (f, reviewer, fixer, _remote) = looping_with(
            "review-loop-slow",
            "run_timeout_secs = 1\n",
            ScriptedClient::new("r", [say("Thinking.")]).with_delay(Duration::from_secs(3)),
            ScriptedClient::new("f", []),
            Arc::new(crate::workspace::host::HostProvider),
        )
        .await;
        let run = RunId::parse("r-loop-slow").unwrap();
        let _ = run_review(&f.app, request(&run), CancellationToken::new()).await;
        let record = f.app.store.run(&run).await.unwrap().unwrap();
        assert_eq!(
            (record.loop_stop.as_deref(), record.loop_rounds),
            (Some("timeout"), Some(1))
        );
        assert_eq!(reviewer.requests().len(), 1);
        assert!(fixer.requests().is_empty());
        assert_nothing_posted(&f.writer);
    }

    #[tokio::test]
    async fn a_reviewer_that_never_finishes_its_round_is_not_convergence() {
        let (f, reviewer, fixer, remote) = looping(
            "review-loop-unfinished",
            ScriptedClient::new("r", (0..3).map(|_| say("Done."))),
            ScriptedClient::new("f", [say("Nothing to do.")]),
            Arc::new(crate::workspace::host::HostProvider),
        )
        .await;
        let (before, _) = crate::git::tests::remote_feature(remote.path()).await;
        let run = RunId::parse("r-loop-unfinished").unwrap();
        let report = run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(reviewer.requests().len(), 3, "nudged twice, then stopped");
        assert!(fixer.requests().is_empty());
        let summary = report.summary.unwrap();
        assert!(!report.outcome.unwrap().completed(), "{summary}");
        assert!(!summary.contains("converged"), "{summary}");
        assert!(
            summary.contains("the reviewer did not finish round 1"),
            "{summary}"
        );
        assert!(henk_domain::text::is_in_style(&summary), "{summary}");
        let (after, _) = crate::git::tests::remote_feature(remote.path()).await;
        assert_eq!(before, after, "nothing pushed");
        assert_nothing_posted(&f.writer);
        assert_no_stage_running(&f.app, &run).await;
    }

    #[tokio::test]
    async fn without_a_workspace_the_loop_ends_at_once_and_pushes_nothing() {
        let (f, reviewer, fixer, remote) = looping(
            "review-loop-none",
            ScriptedClient::new("r", [say("src/a.rs:2: wrong.")]),
            ScriptedClient::new("f", [edit("let x = 1;", "let x = 3;")]),
            Arc::new(NoWorkspaces),
        )
        .await;
        let (before, _) = crate::git::tests::remote_feature(remote.path()).await;
        let run = RunId::parse("r-loop-none").unwrap();
        let _ = run_review(&f.app, request(&run), CancellationToken::new()).await;
        let record = f.app.store.run(&run).await.unwrap().unwrap();
        assert_eq!(record.status, RunStatus::Failed);
        let error = record.error.unwrap_or_default();
        assert!(error.contains("needs a workspace"), "{error}");
        assert!(error.contains("the sandbox host is down"), "{error}");
        assert!(reviewer.requests().is_empty() && fixer.requests().is_empty());
        let (after, _) = crate::git::tests::remote_feature(remote.path()).await;
        assert_eq!(before, after, "nothing pushed");
        assert_no_stage_running(&f.app, &run).await;
    }

    #[tokio::test]
    async fn a_loop_stopped_after_it_pushed_still_reports_the_check_on_the_new_head() {
        let (f, _reviewer, _fixer, remote) = looping(
            "review-loop-stopped",
            ScriptedClient::new(
                "r",
                [
                    report(2, "x must be 3", None),
                    finish_round(),
                    say("Done for now."),
                    finish_round(),
                    say("Nothing more."),
                ],
            )
            .with_delay(Duration::from_millis(300)),
            ScriptedClient::new(
                "f",
                [
                    edit("let x = 1;", "let x = 3;"),
                    judge("f1", "fixed", "x is 3 now."),
                    say("Fixed."),
                ],
            ),
            Arc::new(crate::workspace::host::HostProvider),
        )
        .await;
        let (before, _) = crate::git::tests::remote_feature(remote.path()).await;
        // Henk stops once round 1 pushed, while the reviewer looks again.
        let cancel = CancellationToken::new();
        let watcher = {
            let (cancel, shutdown) = (cancel.clone(), f.app.shutdown.clone());
            let remote = remote.path().to_path_buf();
            let before = before.clone();
            tokio::spawn(async move {
                for _ in 0..400 {
                    if crate::git::tests::remote_feature(&remote).await.0 != before {
                        shutdown.cancel();
                        cancel.cancel();
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
        };
        let run = RunId::parse("r-loop-stopped").unwrap();
        let result = run_review(&f.app, request(&run), cancel).await;
        watcher.await.unwrap();
        let error = result.unwrap_err();
        assert!(error.is::<Interrupted>(), "{error:#}");

        let (head, _) = crate::git::tests::remote_feature(remote.path()).await;
        assert_ne!(head, before, "round 1 pushed");
        assert!(
            f.app.own_pushes.contains(&head),
            "so nothing else reviews it"
        );
        let reported = f.writer.finished_commits.lock().unwrap().clone();
        assert_eq!(reported.len(), 2, "{reported:?}");
        assert!(reported.contains(&before), "{reported:?}");
        assert!(
            reported.contains(&head),
            "the new head gets the check: {reported:?}"
        );
        assert_no_stage_running(&f.app, &run).await;
    }

    #[tokio::test]
    async fn a_pull_request_from_a_fork_gets_the_lane_review_instead_of_the_loop() {
        let (f, reviewer, fixer, remote) = looping_from(
            "review-loop-fork",
            "someone/r",
            ScriptedClient::new("m", (0..4).map(|_| done())),
            ScriptedClient::new("r", [say("src/a.rs:2: wrong.")]),
            ScriptedClient::new("f", [edit("let x = 1;", "let x = 3;")]),
            Arc::new(crate::workspace::host::HostProvider),
        )
        .await;
        let (before, _) = crate::git::tests::remote_feature(remote.path()).await;
        let run = RunId::parse("r-loop-fork").unwrap();
        let report = run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();

        let record = f.app.store.run(&run).await.unwrap().unwrap();
        assert_eq!(record.status, RunStatus::Finished, "{:?}", record.error);
        assert!(report.outcome.unwrap().completed());
        let summary = report.summary.unwrap();
        assert!(
            summary.contains("The review loop did not run: its branch is in another repository"),
            "{summary}"
        );
        assert!(summary.contains("The lanes reviewed instead."), "{summary}");
        assert!(henk_domain::text::is_in_style(&summary), "{summary}");
        assert!(!f.model.requests().is_empty(), "the lane reviewed");
        let lanes = f.app.store.lanes(&run).await.unwrap();
        let names: Vec<_> = lanes.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["lane-a"]);
        assert!(reviewer.requests().is_empty() && fixer.requests().is_empty());
        let (after, _) = crate::git::tests::remote_feature(remote.path()).await;
        assert_eq!(before, after, "nothing pushed");
        assert_eq!(
            *f.writer.finished_commits.lock().unwrap(),
            [before],
            "nothing pushed, one check"
        );
        assert_no_stage_running(&f.app, &run).await;
    }

    /// The lanes that review in the loop's place post, so a cancel from
    /// the dashboard says so on the pull request, as without the loop.
    #[tokio::test]
    async fn the_lane_review_in_the_loop_s_place_posts_its_cancel_notice() {
        let (f, _, _, _remote) = looping_from(
            "review-loop-fork-cancel",
            "someone/r",
            ScriptedClient::new("m", (0..4).map(|_| done())).with_delay(Duration::from_secs(30)),
            ScriptedClient::new("r", []),
            ScriptedClient::new("f", []),
            Arc::new(crate::workspace::host::HostProvider),
        )
        .await;
        let run = RunId::parse("r-loop-fork-cancel").unwrap();
        let cancel = f.app.shutdown.child_token();
        let _cancellable = f.app.cancels.register(run.clone(), cancel.clone());
        let (cancels, cancelled) = (f.app.cancels.clone(), run.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert!(cancels.cancel(&cancelled, "github:1234".to_owned()));
        });
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            run_review(&f.app, request(&run), cancel),
        )
        .await
        .expect("a cancelled review ends promptly");

        assert!(result.is_err_and(|e| e.is::<CancelledBy>()));
        assert!(
            f.writer
                .replies
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.body.contains("Cancelled from the dashboard")),
            "one comment says it was cancelled"
        );
    }

    #[tokio::test]
    async fn a_branch_that_moved_after_queueing_gets_the_lane_review_instead_of_the_loop() {
        let (f, reviewer, fixer, remote) = looping_from(
            "review-loop-moved",
            "o/r",
            ScriptedClient::new("m", (0..4).map(|_| done())),
            ScriptedClient::new("r", [say("src/a.rs:2: wrong.")]),
            ScriptedClient::new("f", [edit("let x = 1;", "let x = 3;")]),
            Arc::new(crate::workspace::host::HostProvider),
        )
        .await;
        let (before, _) = crate::git::tests::remote_feature(remote.path()).await;
        let run = RunId::parse("r-loop-moved").unwrap();
        // Queued at an older commit than the branch's head now.
        let mut queued = request(&run);
        queued.commit = Some(CommitSha::parse(SHA).unwrap());
        let report = run_review(&f.app, queued, CancellationToken::new())
            .await
            .unwrap();

        let summary = report.summary.unwrap();
        assert!(
            summary.contains("The review loop did not run: the branch moved to"),
            "{summary}"
        );
        assert!(report.outcome.unwrap().completed(), "{summary}");
        assert!(reviewer.requests().is_empty() && fixer.requests().is_empty());
        let (after, _) = crate::git::tests::remote_feature(remote.path()).await;
        assert_eq!(before, after, "nothing pushed");
        assert_no_stage_running(&f.app, &run).await;
    }

    #[tokio::test]
    async fn a_review_with_only_the_loop_shows_its_check_queued() {
        let lanes = "lanes = [{ name = \"lane-a\", model = \"m\" }]";
        let only_loop = loop_config().replace(lanes, "lanes = []");
        assert_ne!(only_loop, loop_config());
        let f = fixture_on(
            DIFF,
            ScriptedClient::new("m", []),
            &only_loop,
            SHA,
            Arc::new(crate::workspace::host::HostProvider),
            None,
        )
        .await;
        assert!(f.app.settings.lanes.is_empty());
        let run = RunId::parse("r-loop-queued").unwrap();
        let mut queued = request(&run);
        queued.commit = Some(CommitSha::parse(SHA).unwrap());
        let handle = crate::coordinator::queue_check(&f.app, &queued).await;
        assert_eq!(handle, Some(ReviewHandle("queued-1".to_owned())));
        assert_eq!(f.writer.queued.lock().unwrap().len(), 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shared_copy_note_is_in_style() {
        assert!(henk_domain::text::is_in_style(SHARED_COPY));
    }

    #[test]
    fn the_lane_turn_warning_is_in_style_and_says_what_to_do() {
        assert!(henk_domain::text::is_in_style(LANE_TURN_WARNING));
        assert!(LANE_TURN_WARNING.contains("post_finding"));
        assert!(LANE_TURN_WARNING.starts_with(&format!("{LANE_TURN_WARNING_AT} turns left")));
    }
}
