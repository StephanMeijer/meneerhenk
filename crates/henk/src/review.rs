//! The review orchestrator (§3).

use std::sync::{Arc, Mutex};

use anyhow::{Context as _, anyhow};
use henk_domain::allowlist::Platform;
use henk_domain::diff::ReviewDiff;
use henk_domain::identity::RequesterId;
use henk_domain::ignore::PathFilter;
use henk_domain::marker::{Marker, MarkerKind, ModelId};
use henk_domain::review::{CommitSha, ReviewOutcome};
use henk_domain::run::{RunId, RunKind};
use henk_platform::{PlatformWriter, PullRequestState, ReviewHandle, ReviewTarget};
use henk_store::{NewRun, RunStatus, Stage, StageState};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, instrument, warn};

use crate::app::App;
use crate::ids::new_run_id;
use crate::liveness::KeepAlive;
use crate::stages;

/// The review was cancelled because a newer commit arrived.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("superseded by a review of a newer commit")]
pub struct Superseded;

/// A person cancelled the review from the dashboard (#69), or an MCP client
/// did (#266); [`crate::cancel::cancelled_reason`] says which.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{}", crate::cancel::cancelled_reason(.0))]
pub struct CancelledBy(pub String);

/// The review was cancelled because Henk was told to stop.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("interrupted")]
pub struct Interrupted;

/// The pull request is not one Henk reviews: closed, a draft, not on the
/// allowlist, or reviews are off. Not Henk's failure, unlike a read that
/// failed (#262). The reason is Henk's own words and safe to show.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct NotReviewable(pub String);

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
    /// Who asked, when Henk knows them by a stable id (#399).
    requester: Option<&'a RequesterId>,
    /// The last commit a review loop pushed (#284), the pull request's new
    /// head, which every ending of the review reports its check on too.
    pushed_head: &'a Mutex<Option<CommitSha>>,
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
    /// Who asked, as the run records it: a stable id such as
    /// `github:<id>`, a webhook sender's login, or nobody.
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

impl ReviewRequest {
    /// Who asked, when Henk knows them by a stable id (#399).
    #[must_use]
    pub fn requester_id(&self) -> Option<RequesterId> {
        self.requester.as_deref().and_then(RequesterId::parse)
    }
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
    let writer = app.writer(&request.target.repo)?;
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

    let (pushed_head, requester) = (Mutex::new(None), request.requester_id());
    let review = ReviewRun {
        app,
        writer: &writer,
        target: &request.target,
        commit: &commit,
        run: &run,
        link: &link,
        requester: requester.as_ref(),
        pushed_head: &pushed_head,
    };
    let result = loop_body(review, &info.base_ref, cancel).await;

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
            sessions: Vec::new(),
            open_findings: 0,
            nothing_to_review: false,
            stopped: None,
            not_reviewed: None,
            findings_left: false,
            report: None,
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
    if app.settings.review.r#loop.is_none() {
        return Err(
            NotReviewable("reviews are off: no [review.loop] is configured".to_owned()).into(),
        );
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

/// A person cancelled the review from the dashboard (#69): a neutral check
/// that says who, nothing posted, and the run ends `cancelled`. Not Henk's
/// failure, so nothing says it is.
async fn report_cancelled(
    review: ReviewRun<'_>,
    handle: Option<&henk_platform::ReviewHandle>,
    by: &str,
) -> anyhow::Result<()> {
    let ReviewRun {
        app, commit, run, ..
    } = review;
    info!(run = %run, by, "review {}", crate::cancel::cancelled_reason(by));
    let outcome = ReviewOutcome::cancelled(commit.clone(), crate::cancel::cancelled_via(by));
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
/// who; nothing is posted. Its queued check, if it has one, closes
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
        match app.writer(&request.target.repo) {
            Ok(writer) => {
                close_queued_as_cancelled(&writer, request, &app.settings.queue_link(&run), by)
                    .await;
            }
            Err(writer_error) => error!(%writer_error, "could not close the queued check"),
        }
        return Err(error.into());
    }
    info!(run = %run, by, "review {} before it started", crate::cancel::cancelled_reason(by));
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
    match app.writer(&request.target.repo) {
        Ok(writer) => close_queued_as_cancelled(&writer, request, &link, by).await,
        Err(writer_error) => error!(%writer_error, "could not close the queued check"),
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
    by: &str,
) {
    if let (Some(handle), Some(commit)) = (&request.queued_check, &request.commit) {
        let outcome = ReviewOutcome::cancelled(commit.clone(), crate::cancel::cancelled_via(by));
        if let Err(finish_error) = writer
            .finish_review(&request.target, commit, Some(handle), &outcome, link)
            .await
        {
            error!(%finish_error, "could not close the queued check");
        }
    }
}

/// Henk was told to stop while the review waited for a slot. It never
/// started; its run is still recorded and ends `failed` as interrupted,
/// as a running review's does, so the next start resumes it (#160). Its
/// queued check, if it has one, closes as interrupted, also when the run
/// could not be recorded. Nothing is posted.
///
/// # Errors
///
/// Returns an error when the run could not be recorded.
pub async fn report_interrupted_while_queued(
    app: &App,
    request: &ReviewRequest,
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
        close_queued_as_interrupted(app, request, &app.settings.queue_link(&run)).await;
        return Err(error.into());
    }
    warn!(run = %run, "review interrupted before it started");
    stages::request(
        &*app.store,
        &run,
        request.submitted_at,
        &request.trigger,
        request.requester.as_deref(),
    )
    .await;
    let interrupted = "interrupted while waiting for a slot";
    stages::mark_span(
        &*app.store,
        &run,
        Stage::Queued,
        StageState::Failed,
        interrupted,
        request
            .submitted_at
            .unwrap_or_else(time::OffsetDateTime::now_utc),
        None,
    )
    .await;
    stages::end(
        &*app.store,
        &run,
        StageState::Failed,
        interrupted,
        interrupted,
    )
    .await;
    close_queued_as_interrupted(app, request, &link).await;
    app.store
        .finish_run(
            &run,
            RunStatus::Failed,
            None,
            Some(&Interrupted.to_string()),
        )
        .await?;
    Ok(())
}

/// A queued check of a review Henk stopped before it started closes as
/// interrupted, as a running one's does (#262).
async fn close_queued_as_interrupted(app: &App, request: &ReviewRequest, link: &str) {
    let (Some(handle), Some(commit)) = (&request.queued_check, &request.commit) else {
        return;
    };
    let outcome = ReviewOutcome::interrupted(commit.clone());
    match app.writer(&request.target.repo) {
        Ok(writer) => {
            if let Err(finish_error) = writer
                .finish_review(&request.target, commit, Some(handle), &outcome, link)
                .await
            {
                error!(%finish_error, "could not close the queued check");
            }
        }
        Err(writer_error) => error!(%writer_error, "could not close the queued check"),
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
        app, commit, run, ..
    } = review;
    let message = format!("{error:#}");
    error!(error = %message, "review failed");
    let failed = ReviewOutcome {
        commit: commit.clone(),
        sessions: Vec::new(),
        open_findings: 0,
        nothing_to_review: false,
        stopped: None,
        not_reviewed: None,
        findings_left: false,
        report: None,
    };
    if let Err(finish_error) = finish_checks(review, handle, &failed).await {
        error!(%finish_error, "could not finish the check after failure");
    }
    stages::failed(&*app.store, run).await;
    app.store
        .finish_run(run, RunStatus::Failed, None, Some(&message))
        .await?;
    Ok(())
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

/// A review as a reviewer↔executor loop (#284, #285): the reviewer's findings
/// go to the executor, never to the pull request. Nothing is posted; what the
/// loop did is on the run, and its report, with the findings that still
/// stand, is the end of the check's summary. A pull request the loop may
/// not push to (a fork, the default or a protected branch) or whose branch
/// moved gets the reviewer alone (#307). On GitLab, whose commit status
/// carries only the count, the reviewer alone's summary is a summary note:
/// its findings are not fixed, so they must be readable on the merge
/// request.
async fn loop_body(
    review: ReviewRun<'_>,
    base_ref: &str,
    cancel: CancellationToken,
) -> anyhow::Result<(ReviewOutcome, String)> {
    let ReviewRun {
        app,
        writer,
        target,
        commit,
        run,
        link,
        ..
    } = review;
    let store = &*app.store;
    stages::mark(store, run, Stage::Diff, StageState::Running, "").await;
    let diff = fetch_diff(review, base_ref).await?;
    let nothing_to_review = diff.is_empty();
    let (results, open_findings, report, findings_left, review_only) = if nothing_to_review {
        for stage in [Stage::Checkout, Stage::Rounds] {
            stages::mark(store, run, stage, StageState::Skipped, "nothing to review").await;
        }
        (Vec::new(), 0, None, false, false)
    } else {
        let on = crate::review_loop::LoopTarget {
            app,
            target,
            commit,
            run,
            requester: review.requester,
            pushed_head: review.pushed_head,
        };
        let report = match crate::review_loop::run_loop(on, Arc::clone(&diff), &cancel).await {
            Ok(report) => report,
            // Stopped for its token, it ends the way the token says:
            // cancelled, superseded or interrupted, not failed.
            Err(error) => {
                stop_if_ended(review, &cancel)?;
                return Err(error);
            }
        };
        (
            report.results,
            report.open,
            Some(report.report),
            report.findings_left,
            report.review_only,
        )
    };
    stop_if_ended(review, &cancel)?;
    let outcome = ReviewOutcome {
        commit: commit.clone(),
        sessions: results,
        open_findings,
        nothing_to_review,
        stopped: None,
        not_reviewed: None,
        findings_left,
        report,
    };
    let summary_text = outcome.summary();
    if review_only && writer.platform() == Platform::GitLab {
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
        let published = "the summary note: a commit status carries only the count";
        stages::mark(store, run, Stage::Publish, StageState::Done, published).await;
        return Ok((outcome, summary_text));
    }
    let skipped = "a review loop posts nothing on the pull request";
    stages::mark(store, run, Stage::Publish, StageState::Skipped, skipped).await;
    Ok((outcome, summary_text))
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

/// The diff, once, for the review loop: handed out per file. Without it
/// there is no review: the error ends the run as Henk's own failure, which
/// the summary says.
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
    use henk_mcp::McpSession;
    use henk_mcp::testing::{FakeServer, echo_behaviour};
    use henk_store::SessionStatus;

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

    /// A review loop whose reviewer and executor are both model `m` (#308).
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
[endpoints.m]
provider = "openai"
base_url = "https://x.test/v1"
api_key_env = "UNUSED"
[models.m]
endpoint = "m"
model = "scripted"
[review]
[review.loop]
[roles.reviewer]
model = "m"
[roles.executor]
model = "m"
requester_id = 3
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
        /// The model every session talks to, with what it was asked.
        model: Arc<ScriptedClient>,
        /// The reviewed head.
        head: CommitSha,
        /// Keeps the remote a review loop works on, if there is one.
        remote: Option<crate::git::ScratchDir>,
    }

    /// An app whose writer, read session and model are fakes, reviewing as
    /// a review loop with `model` as reviewer and executor, on a fresh local
    /// remote `name` whose head is the reviewed commit. `diff` is the diff
    /// the writer serves.
    async fn fixture(name: &str, diff: &str, model: ScriptedClient) -> Fixture {
        let (remote, head) = crate::git::tests::bare_remote(name).await;
        let hub = Arc::new(LoopHub {
            remote: remote.path().to_path_buf(),
            head_repo: "o/r",
            credential: None,
        });
        let mut f = fixture_on(
            diff,
            model,
            CONFIG,
            head.as_str(),
            Arc::new(crate::workspace::host::HostProvider),
            Some(hub),
        )
        .await;
        f.remote = Some(remote);
        f
    }

    /// The reviewer's round with nothing to report.
    fn converge() -> [Result<Completion, henk_llm::LlmError>; 2] {
        [finish_round(), say("Nothing.")]
    }

    /// [`converge`], `reviews` times, for reviews one after another.
    fn converging(reviews: usize) -> Vec<Result<Completion, henk_llm::LlmError>> {
        (0..reviews).flat_map(|_| converge()).collect()
    }

    /// [`fixture`] with its own configuration, reviewed head, workspace
    /// backend and address writer (the review's way to the source, #170).
    async fn fixture_on(
        diff: &str,
        model: ScriptedClient,
        config: &str,
        head: &str,
        provider: Arc<dyn crate::workspace::WorkspaceProvider>,
        source: Option<Arc<dyn henk_platform::push::PushWriter>>,
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
                test_push_writer: source,
                test_issue_writer: None,
            },
            writer,
            model,
            head: CommitSha::parse(head).unwrap(),
            remote: None,
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

    /// [`request`], asked for by `requester` as a run records it.
    fn request_by(run: &RunId, requester: &str) -> ReviewRequest {
        ReviewRequest {
            requester: Some(requester.to_owned()),
            ..request(run)
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
        let f = fixture(
            "review-stages",
            DIFF,
            ScriptedClient::new("scripted", converge()),
        )
        .await;
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
                (Stage::Checkout, StageState::Done),
                (Stage::Rounds, StageState::Done),
                (Stage::Publish, StageState::Skipped),
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
        assert_eq!(
            detail(Stage::Checkout),
            format!("ready at {}", f.head.short())
        );
        assert!(
            detail(Stage::Rounds).starts_with("The review loop ran 1 round"),
            "{}",
            detail(Stage::Rounds)
        );
        assert_eq!(
            detail(Stage::Publish),
            "a review loop posts nothing on the pull request"
        );
        for (_, _, line) in &stages {
            assert!(henk_domain::text::is_in_style(line), "{line}");
        }
    }

    /// A review loop posts nothing on the pull request: its check is where
    /// the review is read (#285, #307).
    #[tokio::test]
    async fn a_completed_review_closes_its_check_and_posts_nothing() {
        let f = fixture(
            "review-done",
            DIFF,
            ScriptedClient::new("scripted", converge()),
        )
        .await;
        let run = RunId::parse("r-done").unwrap();
        let report = run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();
        let summary = report.summary.unwrap();
        assert!(
            summary.starts_with("No issues found.\n\nThe review loop ran 1 round"),
            "{summary}"
        );
        assert_nothing_posted(&f.writer);
        {
            let finished = f.writer.finished.lock().unwrap();
            assert_eq!(finished.len(), 1, "the check is closed once");
            assert_eq!(finished[0].check_conclusion(), CheckConclusion::Success);
        }
        let record = f.app.store.run(&run).await.unwrap().unwrap();
        assert_eq!(record.status, RunStatus::Finished);
        let stored = f.app.store.sessions(&run).await.unwrap();
        let sessions: Vec<_> = stored.iter().map(|s| (s.name.as_str(), s.status)).collect();
        assert_eq!(
            sessions,
            [("reviewer", SessionStatus::Finished)],
            "no executor: nothing to fix"
        );
    }

    /// A failed review closes its check as a failure and records why; a
    /// review loop posts no failure comment (#285).
    #[tokio::test]
    async fn a_failed_review_closes_the_check_as_failure_and_posts_nothing() {
        // No patches: the diff is empty, which ends the run as Henk's failure.
        let f = fixture("review-fail", "", ScriptedClient::new("scripted", [])).await;
        let run = RunId::parse("r-fail").unwrap();
        let error = run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("diff is empty"), "{error:#}");
        assert_nothing_posted(&f.writer);
        {
            let finished = f.writer.finished.lock().unwrap();
            assert_eq!(finished.len(), 1);
            assert!(finished[0].sessions.is_empty());
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

    /// A review cancelled while its reviewer waits on the model.
    /// `shutdown` stops Henk as Ctrl-C does; otherwise only this review is
    /// cancelled, as superseding does.
    async fn cancelled_review(
        run: &RunId,
        shutdown: bool,
    ) -> (Fixture, anyhow::Result<ReviewReport>) {
        let model = ScriptedClient::new("scripted", converge()).with_delay(Duration::from_secs(30));
        let f = fixture(&format!("henk-{run}"), DIFF, model).await;
        let cancel = f.app.shutdown.child_token();
        let trigger = if shutdown {
            f.app.shutdown.clone()
        } else {
            cancel.clone()
        };
        let result = ended_once(
            Box::pin(run_review(&f.app, request(run), cancel)),
            "the reviewer's first request",
            || !f.model.requests().is_empty(),
            || trigger.cancel(),
        )
        .await;
        (f, result)
    }

    /// The contract #7's fix relies on: a cancelled review closes its check
    /// and leaves no run or session `running`.
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
        for session in f.app.store.sessions(&run).await.unwrap() {
            assert_ne!(session.status, SessionStatus::Running, "{}", session.name);
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
        for session in f.app.store.sessions(&run).await.unwrap() {
            assert_ne!(session.status, SessionStatus::Running, "{}", session.name);
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
        let f = fixture(
            &format!("henk-reaper-{}", line!()),
            DIFF,
            ScriptedClient::new("scripted", [done()]),
        )
        .await;
        let (dead, live) = (
            RunId::parse("r-dead").unwrap(),
            RunId::parse("r-live").unwrap(),
        );
        f.app.store.create_run(&new_run(&dead)).await.unwrap();
        f.app.store.set_check(&dead, "4242").await.unwrap();
        f.app
            .store
            .start_session(&dead, "reviewer", henk_store::SessionRole::Reviewer, "m")
            .await
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let cutoff = time::OffsetDateTime::now_utc();
        std::thread::sleep(std::time::Duration::from_millis(5));
        f.app.store.create_run(&new_run(&live)).await.unwrap();
        f.app
            .store
            .start_session(&live, "reviewer", henk_store::SessionRole::Reviewer, "m")
            .await
            .unwrap();

        assert_eq!(crate::liveness::reap_silent_since(&f.app, cutoff).await, 1);

        let record = f.app.store.run(&dead).await.unwrap().unwrap();
        assert_eq!(record.status, RunStatus::Failed);
        assert_eq!(
            record.error.as_deref(),
            Some("interrupted: the process ended")
        );
        assert_eq!(
            f.app.store.sessions(&dead).await.unwrap()[0].status,
            SessionStatus::DidNotFinish
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
            f.app.store.sessions(&live).await.unwrap()[0].status,
            SessionStatus::Running
        );
    }

    /// #47: a process that died and came back within the staleness window
    /// finds its old run still fresh at start. The serving reaper closes it
    /// on a later pass, with its check.
    #[tokio::test]
    async fn a_quick_restart_is_reaped_while_serving() {
        let f = fixture(
            &format!("henk-reaper-{}", line!()),
            DIFF,
            ScriptedClient::new("scripted", [done()]),
        )
        .await;
        let crashed = RunId::parse("r-crashed").unwrap();
        f.app.store.create_run(&new_run(&crashed)).await.unwrap();
        f.app.store.set_check(&crashed, "4242").await.unwrap();
        f.app
            .store
            .start_session(&crashed, "reviewer", henk_store::SessionRole::Reviewer, "m")
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
            None,
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
            app.store.sessions(&crashed).await.unwrap()[0].status,
            SessionStatus::DidNotFinish
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
        let f = fixture(
            &format!("henk-reaper-{}", line!()),
            DIFF,
            ScriptedClient::new("scripted", [done()]),
        )
        .await;
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
        review_cancelled_by(
            "github:1234",
            "Cancelled from the dashboard.",
            "cancelled from the dashboard by github:1234",
        )
        .await;
    }

    /// #293: a cancel over MCP says so in the check and the run, not "from
    /// the dashboard".
    #[tokio::test]
    async fn a_review_cancelled_over_mcp_says_so_in_the_check_and_the_run() {
        review_cancelled_by(
            "mcp:claude",
            "Cancelled over MCP.",
            "cancelled over MCP by mcp:claude",
        )
        .await;
    }

    /// A review that `by` cancels while it runs: the check's headline and
    /// the run's stored reason say who. A review loop posts nothing, no
    /// notice and no failure comment (#285).
    async fn review_cancelled_by(by: &str, headline: &str, reason: &str) {
        let run = RunId::parse("r-cancelled").unwrap();
        let model = ScriptedClient::new("scripted", converge()).with_delay(Duration::from_secs(30));
        let f = fixture(
            &format!("henk-cancelled-{by}").replace(':', "-"),
            DIFF,
            model,
        )
        .await;
        let cancel = f.app.shutdown.child_token();
        let _cancellable = f.app.cancels.register(run.clone(), cancel.clone());
        let result = ended_once(
            Box::pin(run_review(&f.app, request(&run), cancel)),
            "the reviewer's first request",
            || !f.model.requests().is_empty(),
            || assert!(f.app.cancels.cancel(&run, by.to_owned())),
        )
        .await;

        assert!(result.is_err_and(|e| e.is::<CancelledBy>()));
        assert_nothing_posted(&f.writer);
        {
            let finished = f.writer.finished.lock().unwrap();
            assert_eq!(finished[0].check_conclusion(), CheckConclusion::Neutral);
            assert_eq!(finished[0].headline(), headline);
        }
        let record = f.app.store.run(&run).await.unwrap().unwrap();
        assert_eq!(record.status, RunStatus::Cancelled);
        assert_eq!(record.error.as_deref(), Some(reason));
    }

    /// #57: the same pull request with its path in another case joins the
    /// running review instead of starting a second one beside it.
    #[tokio::test]
    async fn a_request_differing_only_in_case_joins_the_running_review() {
        let model = ScriptedClient::new("scripted", converge()).with_delay(Duration::from_secs(30));
        let Fixture {
            app,
            head,
            remote: _remote,
            ..
        } = fixture("henk-coord-case", DIFF, model).await;
        let app = Arc::new(app);
        let coordinator = crate::coordinator::Coordinator::new(Arc::clone(&app));
        let commit = head;
        let placeholder = RunId::parse("r-placeholder").unwrap();
        let in_case = |path: &str| ReviewRequest {
            target: ReviewTarget {
                repo: RepoRef::parse(Platform::GitHub, path).unwrap(),
                number: 7,
            },
            ..request(&placeholder)
        };
        let (_, run) = coordinator.submit_review(in_case("O/R"), commit.clone());
        let mut status = None;
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            status = app.store.run(&run).await.unwrap().map(|r| r.status);
            if status == Some(RunStatus::Running) {
                break;
            }
        }
        assert_eq!(status, Some(RunStatus::Running), "the review started");

        let (decision, joined) = coordinator.submit_review(in_case("o/r"), commit);
        assert_eq!(decision, henk_domain::queue::Decision::Join);
        assert_eq!(joined, run, "one review, not two");
        assert!(coordinator.cancel(&run, "github:1234".to_owned()));
    }

    #[tokio::test]
    async fn a_cancelled_review_leaves_the_queue_so_the_next_request_starts_afresh() {
        let model = ScriptedClient::new("scripted", converge()).with_delay(Duration::from_secs(30));
        let Fixture {
            app,
            head,
            remote: _remote,
            ..
        } = fixture("henk-coord-cancel", DIFF, model).await;
        let app = Arc::new(app);
        let coordinator = crate::coordinator::Coordinator::new(Arc::clone(&app));
        let commit = head;
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
        let model = ScriptedClient::new("scripted", converge()).with_delay(Duration::from_secs(30));
        let Fixture {
            app, head, remote, ..
        } = fixture("henk-coord-supersede", DIFF, model).await;
        let app = Arc::new(app);
        let coordinator = crate::coordinator::Coordinator::new(Arc::clone(&app));
        let placeholder = RunId::parse("r-placeholder").unwrap();
        let (_, old) = coordinator.submit_review(request(&placeholder), head);
        let mut status = None;
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            status = app.store.run(&old).await.unwrap().map(|r| r.status);
            if status == Some(RunStatus::Running) {
                break;
            }
        }
        assert_eq!(status, Some(RunStatus::Running), "the first review started");

        // A real newer commit, so the review of it can check it out.
        let remote = remote.as_ref().unwrap().path();
        move_branch(remote, "henk-coord-supersede").await;
        let (newer, _) = crate::git::tests::remote_feature(remote).await;
        let newer = CommitSha::parse(&newer).unwrap();
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
        Option<crate::git::ScratchDir>,
    ) {
        let model =
            ScriptedClient::new("scripted", converging(6)).with_delay(Duration::from_millis(150));
        let mut f = fixture(
            if fail_queue {
                "henk-one-slot-failing"
            } else {
                "henk-one-slot"
            },
            DIFF,
            model,
        )
        .await;
        f.app.settings.review.max_concurrent = 1;
        let writer = if fail_queue {
            let failing = Arc::new(FakeWriter {
                head: f.head.as_str().to_owned(),
                patches: henk_domain::diff::split_unified(DIFF),
                fail_queue: true,
                ..FakeWriter::default()
            });
            f.app.test_writer = Some(Arc::clone(&failing) as Arc<dyn PlatformWriter>);
            failing
        } else {
            Arc::clone(&f.writer)
        };
        let (app, commit, remote) = (Arc::new(f.app), f.head, f.remote);
        let coordinator = crate::coordinator::Coordinator::new(Arc::clone(&app));
        let placeholder = RunId::parse("r-placeholder").unwrap();
        let _ = coordinator.submit_review(request(&placeholder), commit.clone());
        until("review A holds the slot", || {
            coordinator.running_reviews() == 1
        })
        .await;
        let mut other = request(&placeholder);
        other.target.number = 8;
        let (_, waiting) = coordinator.submit_review(other, commit);
        (app, writer, coordinator, waiting, remote)
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
        let (app, writer, _coordinator, _waiting, _remote) = one_slot_two_reviews(false).await;
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
        let (app, writer, coordinator, _waiting, _remote) = one_slot_two_reviews(false).await;
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

    /// #160: a review Henk stops while it waits for a slot never started,
    /// yet its run is recorded as interrupted, as a running review's is,
    /// so the next start resumes it. Its queued check closes as interrupted.
    #[tokio::test]
    async fn a_review_stopped_while_queued_is_recorded_interrupted_for_the_next_start() {
        let (app, writer, _coordinator, waiting, _remote) = one_slot_two_reviews(false).await;
        until("B's check is queued", || {
            writer.queued.lock().unwrap().len() == 1
        })
        .await;

        app.shutdown.cancel();
        let mut record = None;
        for _ in 0..300 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            record = app
                .store
                .run(&waiting)
                .await
                .unwrap()
                .filter(|r| r.status != RunStatus::Running);
            if record.is_some() {
                break;
            }
        }
        let record = record.expect("the queued review is recorded once Henk stops");
        assert_eq!(record.status, RunStatus::Failed);
        assert_eq!(record.error.as_deref(), Some("interrupted"));
        assert_eq!(record.target, 8);
        assert_eq!(record.commit.as_deref(), Some(writer.head.as_str()));
        assert_no_stage_running(&app, &waiting).await;
        assert!(
            finished(&writer).contains(&(
                Some("queued-1".to_owned()),
                Some(henk_domain::review::Stopped::Interrupted)
            )),
            "{:?}",
            finished(&writer)
        );

        let since = time::OffsetDateTime::now_utc() - time::Duration::hours(1);
        let resumable = crate::resume::interrupted_reviews(&app, since).await;
        assert!(
            resumable.iter().any(|r| r.id == waiting),
            "the next start resumes it: {resumable:?}"
        );
    }

    #[tokio::test]
    async fn a_queued_check_closes_as_cancelled_and_a_failed_queue_does_not_stop_the_review() {
        let (app, writer, coordinator, waiting, _remote) = one_slot_two_reviews(false).await;
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

        let (app, writer, _coordinator, _waiting, _remote) = one_slot_two_reviews(true).await;
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
        let model =
            ScriptedClient::new("scripted", converging(6)).with_delay(Duration::from_millis(400));
        let mut f = fixture("henk-slow-queue", DIFF, model).await;
        f.app.settings.review.max_concurrent = 1;
        let writer = Arc::new(FakeWriter {
            head: f.head.as_str().to_owned(),
            patches: henk_domain::diff::split_unified(DIFF),
            slow_queue: Some((8, Duration::from_millis(250))),
            ..FakeWriter::default()
        });
        f.app.test_writer = Some(Arc::clone(&writer) as Arc<dyn PlatformWriter>);
        let (app, commit, _remote) = (Arc::new(f.app), f.head, f.remote);
        let coordinator = crate::coordinator::Coordinator::new(Arc::clone(&app));
        let placeholder = RunId::parse("r-placeholder").unwrap();
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
            "henk-start-failed",
            DIFF,
            ScriptedClient::new("scripted", converge()),
        )
        .await;
        let writer = Arc::new(FakeWriter {
            head: f.head.as_str().to_owned(),
            patches: henk_domain::diff::split_unified(DIFF),
            fail_start: true,
            ..FakeWriter::default()
        });
        f.app.test_writer = Some(Arc::clone(&writer) as Arc<dyn PlatformWriter>);
        let run = RunId::parse("r-start-failed").unwrap();
        let mut queued = request(&run);
        queued.commit = Some(f.head.clone());
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
        let model = ScriptedClient::new("scripted", converge()).with_delay(Duration::from_secs(30));
        let mut f = fixture("henk-slot-cancel", DIFF, model).await;
        f.app.settings.review.max_concurrent = 1;
        let writer = Arc::clone(&f.writer);
        let (app, commit, _remote) = (Arc::new(f.app), f.head, f.remote);
        let coordinator = crate::coordinator::Coordinator::new(Arc::clone(&app));
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
        assert!(
            notices.is_empty(),
            "a review loop posts no notice: {notices:?}"
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
        let model = ScriptedClient::new("scripted", converge()).with_delay(Duration::from_secs(30));
        let mut f = fixture("henk-waiting", DIFF, model).await;
        f.app.settings.review.max_concurrent = 1;
        let (app, commit, _remote) = (Arc::new(f.app), f.head, f.remote);
        let coordinator = crate::coordinator::Coordinator::new(Arc::clone(&app));
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
                    w.commit == commit,
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
        assert_nothing_posted(&f.writer);
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

    fn unused() -> henk_platform::PlatformError {
        henk_platform::PlatformError::Decode("not used by a review".to_owned())
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

    /// Drives `review` until `ready` holds, then cancels `run` and gives
    /// the review ten seconds to end, so a test cancels in the state it
    /// means to, not after a guess at how long getting there takes. The
    /// wait runs on the test's own task, so a review that ends first or
    /// never reaches the state fails the test naming `what`; the bound is
    /// generous for loaded runners and costs nothing when the state comes.
    async fn cancelled_once<T>(
        review: impl std::future::Future<Output = T>,
        what: &str,
        ready: impl Fn() -> bool,
        cancels: &crate::cancel::Cancels,
        run: &RunId,
    ) -> T {
        ended_once(review, what, ready, || {
            assert!(cancels.cancel(run, "github:1234".to_owned()));
        })
        .await
    }

    /// [`cancelled_once`] with any way to stop the review: `stop` runs once
    /// `ready` holds, such as a shutdown or a superseding cancel.
    async fn ended_once<T>(
        review: impl std::future::Future<Output = T>,
        what: &str,
        ready: impl Fn() -> bool,
        stop: impl FnOnce(),
    ) -> T {
        let mut review = std::pin::pin!(review);
        let reached = async {
            let deadline = tokio::time::Instant::now() + Duration::from_mins(1);
            while !ready() {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the review never reached {what}"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::select! {
            _ = &mut review => panic!("the review ended before {what}"),
            () = reached => {}
        }
        stop();
        tokio::time::timeout(Duration::from_secs(10), review)
            .await
            .expect("a cancelled review ends promptly")
    }

    /// `[review.loop]` settings for a loop on the ssh backend, with
    /// `setup` as its one setup step.
    fn ssh_loop_settings(setup: &str) -> String {
        let key = russh::keys::PrivateKey::from(
            russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[1; 32]),
        );
        let host_key = key.public_key().to_openssh().unwrap();
        format!(
            "[workspace]\nbackend = \"ssh\"\nreview = true\nsetup = [[\"{setup}\"]]\n[workspace.ssh]\nhost = \"sandbox.example\"\nhost_key = \"{host_key}\"\n"
        )
    }

    /// A review loop on `provider` whose reviewer finds nothing; its
    /// workspace is checked out and set up before the reviewer starts.
    async fn review_on_ssh(
        name: &str,
        provider: Arc<dyn crate::workspace::WorkspaceProvider>,
    ) -> Fixture {
        let (f, reviewer, _, _remote) = looping_with(
            name,
            &ssh_loop_settings("true"),
            ScriptedClient::new("r", converge()),
            ScriptedClient::new("f", []),
            provider,
        )
        .await;
        let run = RunId::parse(format!("r-{name}")).unwrap();
        run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();
        assert!(!reviewer.requests().is_empty());
        let stages = stage_list(&f.app, &run).await;
        let ready = format!("ready at {}", f.head.short());
        assert!(
            stages.contains(&(henk_store::Stage::Checkout, StageState::Done, ready)),
            "{stages:?}"
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
    async fn live_a_review_on_a_real_sandbox_host_opens_and_closes_its_workspace() {
        let provider =
            crate::workspace::ssh::SshProvider::new(crate::workspace::ssh::tests::live_target());
        review_on_ssh("ssh-live", Arc::new(provider)).await;
    }

    #[tokio::test]
    #[ignore = "needs a cluster (HENK_TEST_KUBE_*)"]
    async fn live_kube_a_review_opens_and_closes_its_pod() {
        use crate::workspace::kubernetes::tests::{LIVE, live_provider, pods_left};
        let _one = LIVE.lock().await;
        let provider = live_provider().await;
        review_on_ssh("kube-live", Arc::new(provider.clone())).await;
        assert_eq!(pods_left(&provider).await, Vec::<String>::new());
    }

    /// A review loop on `provider` whose reviewer asks `bash` for the
    /// checked-out commit: it gets the reviewed sha, as the run user, in
    /// its own copy.
    async fn bash_on_ssh(name: &str, provider: Arc<dyn crate::workspace::WorkspaceProvider>) {
        let reviewer = ScriptedClient::new(
            "r",
            [
                call(
                    "bash",
                    serde_json::json!({"command": "git log -1 --format=%H"}),
                ),
                finish_round(),
                say("Nothing."),
            ],
        );
        let (f, reviewer, _, _remote) = looping_with(
            name,
            &ssh_loop_settings("true"),
            reviewer,
            ScriptedClient::new("f", []),
            provider,
        )
        .await;
        let run = RunId::parse(format!("r-{name}")).unwrap();
        run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();
        let (_, results, _) = seen(&reviewer);
        let expected = format!("\n{}\n", f.writer.head);
        assert!(
            results
                .iter()
                .any(|r| r.starts_with("exit 0 in ") && r.ends_with(&expected)),
            "the reviewer's bash sees the reviewed commit: {results:?}"
        );
        let stored = f
            .app
            .store
            .transcript(&run, "reviewer")
            .await
            .unwrap()
            .expect("the reviewer's transcript is stored with the run");
        assert!(
            stored.body.contains(&expected.replace('\n', "\\n")),
            "what bash printed is in the transcript: {}",
            stored.body
        );
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

    /// The executor is told of `bash` only when it has it (#334): on a
    /// backend that runs model commands, and with `bash` in
    /// `[roles.executor]` tools.
    #[tokio::test]
    async fn the_executor_hears_of_bash_only_when_its_tools_grant_it() {
        use crate::workspace::remote::tests::LocalRunner;
        let without: Vec<String> = henk_domain::role::Role::Executor
            .default_tools()
            .into_iter()
            .filter(|tool| tool != "bash")
            .collect();
        for (name, tools, granted) in [
            ("ssh-executor-bash", None, true),
            ("ssh-executor-nobash", Some(without), false),
        ] {
            let runner = LocalRunner::new(&format!("henk-review-{name}"));
            let provider = crate::workspace::ssh::SshProvider::with_runner(
                Arc::clone(&runner) as Arc<dyn crate::workspace::remote::Runner>
            );
            let (mut f, _, executor, _remote) = looping_with(
                name,
                &ssh_loop_settings("true"),
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
                Arc::new(provider),
            )
            .await;
            f.app.settings.roles.executor.as_mut().unwrap().tools = tools;
            let run = RunId::parse(format!("r-{name}")).unwrap();
            run_review(&f.app, request(&run), CancellationToken::new())
                .await
                .unwrap();
            let requests = executor.requests();
            let first = requests.first().unwrap();
            let names: Vec<String> = first.tools.iter().map(|t| t.name.to_string()).collect();
            let system = first.system.clone().unwrap_or_default();
            assert_eq!(names.iter().any(|n| n == "bash"), granted, "{names:?}");
            assert_eq!(
                system.contains("commands of your own in the checkout with `bash`"),
                granted,
                "{system}"
            );
            assert!(names.iter().any(|n| n == "give_verdict"), "{names:?}");
        }
    }

    #[tokio::test]
    #[ignore = "needs a sandbox host (HENK_TEST_SSH_*)"]
    async fn live_a_reviewer_runs_bash_on_a_real_sandbox_host() {
        let provider =
            crate::workspace::ssh::SshProvider::new(crate::workspace::ssh::tests::live_target());
        bash_on_ssh("ssh-live-bash", Arc::new(provider)).await;
    }

    #[tokio::test]
    #[ignore = "needs a cluster (HENK_TEST_KUBE_*)"]
    async fn live_kube_a_reviewer_runs_bash_in_its_pod() {
        use crate::workspace::kubernetes::tests::{LIVE, live_provider, pods_left};
        let _one = LIVE.lock().await;
        let provider = live_provider().await;
        bash_on_ssh("kube-live-bash", Arc::new(provider.clone())).await;
        assert_eq!(pods_left(&provider).await, Vec::<String>::new());
    }

    /// A reviewer that goes looking for the credential the pull request
    /// was checked out with finds nothing, and its run keeps nothing of it:
    /// no transcript and no tool call holds the token (#191, §8.4).
    #[tokio::test]
    async fn no_transcript_or_tool_call_holds_the_git_credential() {
        use crate::workspace::remote::tests::LocalRunner;
        let runner = LocalRunner::new("henk-review-credential");
        let provider = crate::workspace::ssh::SshProvider::with_runner(
            Arc::clone(&runner) as Arc<dyn crate::workspace::remote::Runner>
        );
        let looking =
            "env; git config --list --show-origin; cat .git/config; grep -rn ghs_ .git . || true";
        let reviewer = ScriptedClient::new(
            "r",
            [
                call("bash", serde_json::json!({"command": looking})),
                call("search", serde_json::json!({"pattern": "ghs_"})),
                finish_round(),
                say("Nothing."),
            ],
        );
        let (mut f, _, _, remote) = looping_with(
            "credential",
            &ssh_loop_settings("true"),
            reviewer,
            ScriptedClient::new("f", []),
            Arc::new(provider),
        )
        .await;
        f.app.test_push_writer = Some(Arc::new(LoopHub {
            remote: remote.path().to_path_buf(),
            head_repo: "o/r",
            credential: Some(henk_platform::push::GitCredential::github(
                GIT_TOKEN.to_owned().into(),
            )),
        }));
        let run = RunId::parse("r-credential").unwrap();
        run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();

        let store = &f.app.store;
        let listed = store.transcripts(&run).await.unwrap();
        assert!(listed.iter().any(|t| t.session == "reviewer"), "{listed:?}");
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
        let transcript = store.transcript(&run, "reviewer").await.unwrap().unwrap();
        assert!(
            transcript.body.contains("PATH=")
                && transcript.body.contains("core.repositoryformatversion"),
            "the reviewer's bash did read its environment and git config: {}",
            transcript.body
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

    /// The pull request of a review loop (#284), on a local remote: its
    /// head is whatever `feature` holds now, so a push moves it.
    struct LoopHub {
        remote: std::path::PathBuf,
        /// The repository the pull request's branch is in: `o/r` unless it
        /// comes from a fork.
        head_repo: &'static str,
        /// The credential git is given, if any.
        credential: Option<henk_platform::push::GitCredential>,
    }

    #[async_trait::async_trait]
    impl henk_platform::push::PushWriter for LoopHub {
        async fn pull_facts(
            &self,
            _: &ReviewTarget,
        ) -> Result<henk_platform::push::PullFacts, henk_platform::PlatformError> {
            let (head, _) = crate::git::tests::remote_feature(&self.remote).await;
            Ok(henk_platform::push::PullFacts {
                push: henk_domain::push::PushFacts {
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
        async fn git_credential(
            &self,
        ) -> Result<Option<henk_platform::push::GitCredential>, henk_platform::PlatformError>
        {
            Ok(self.credential.clone())
        }
        async fn repo_head(
            &self,
            _: &henk_domain::allowlist::RepoRef,
        ) -> Result<henk_platform::push::RepoHead, henk_platform::PlatformError> {
            Err(unused())
        }
        async fn commit_identity(
            &self,
        ) -> Result<henk_platform::push::CommitIdentity, henk_platform::PlatformError> {
            Ok(henk_platform::push::CommitIdentity {
                name: "meneer-henk[bot]".to_owned(),
                email: "1+meneer-henk[bot]@users.noreply.github.com".to_owned(),
            })
        }
        fn noreply_host(&self) -> Result<String, henk_platform::PlatformError> {
            Err(unused())
        }
        async fn open_pull_request(
            &self,
            _: &henk_domain::allowlist::RepoRef,
            _: &henk_platform::push::NewPull,
        ) -> Result<henk_platform::push::OpenedPull, henk_platform::PlatformError> {
            Err(unused())
        }
        /// Account 77 is `alice`, the requester of [`PEOPLE`]; no other
        /// login can be read.
        async fn user_login(&self, id: u64) -> Result<String, henk_platform::PlatformError> {
            match id {
                77 => Ok("alice".to_owned()),
                _ => Err(henk_platform::PlatformError::Decode(format!(
                    "no user {id}"
                ))),
            }
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
            cwd: &henk_domain::path::WorkspacePath,
            timeout: Duration,
        ) -> Result<crate::workspace::ExecResult, crate::workspace::WorkspaceError> {
            self.0.exec(argv, cwd, timeout).await
        }

        async fn read(
            &self,
            path: &henk_domain::path::WorkspacePath,
            max_bytes: u64,
        ) -> Result<Vec<u8>, crate::workspace::WorkspaceError> {
            self.0.read(path, max_bytes).await
        }

        async fn write(
            &self,
            path: &henk_domain::path::WorkspacePath,
            content: &[u8],
        ) -> Result<(), crate::workspace::WorkspaceError> {
            self.0.write(path, content).await
        }

        async fn list(
            &self,
            dir: &henk_domain::path::WorkspacePath,
            only: Option<&henk_domain::ignore::PathFilter>,
            cap: usize,
        ) -> Result<Vec<String>, crate::workspace::WorkspaceError> {
            self.0.list(dir, only, cap).await
        }

        async fn search(
            &self,
            dir: &henk_domain::path::WorkspacePath,
            pattern: &crate::workspace::Pattern,
            only: Option<&henk_domain::ignore::PathFilter>,
            context: usize,
            max_file_bytes: u64,
            cap: usize,
        ) -> Result<Vec<crate::workspace::Hit>, crate::workspace::WorkspaceError> {
            let scribble = henk_domain::path::WorkspacePath::parse("scribble.txt").unwrap();
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

    /// A review loop of reviewer `r` and executor `f` (#284), each its own
    /// scripted model, with `settings` in `[review.loop]`.
    fn loop_config_with(settings: &str) -> String {
        let models = "[endpoints.r]\nprovider = \"openai\"\nbase_url = \"https://x.test/v1\"\napi_key_env = \"UNUSED\"\n[models.r]\nendpoint = \"r\"\nmodel = \"r\"\n[endpoints.f]\nprovider = \"openai\"\nbase_url = \"https://x.test/v1\"\napi_key_env = \"UNUSED\"\n[models.f]\nendpoint = \"f\"\nmodel = \"f\"\n";
        CONFIG
            .replace("[review]\n", &format!("{models}[review]\n"))
            .replace("[review.loop]\n", &format!("[review.loop]\n{settings}"))
            .replace(
                "[roles.reviewer]\nmodel = \"m\"\n",
                "[roles.reviewer]\nmodel = \"r\"\n",
            )
            .replace(
                "[roles.executor]\nmodel = \"m\"\n",
                "[roles.executor]\nmodel = \"f\"\n",
            )
    }

    /// The fixture of a looping test: the app, the reviewer, the executor and
    /// the remote, which is returned to be kept.
    type Looping = (
        Fixture,
        Arc<ScriptedClient>,
        Arc<ScriptedClient>,
        crate::git::ScratchDir,
    );

    /// A reviewer `r` and an executor `f`, each scripted, on a fresh local
    /// remote served by [`LoopHub`]; the remote is returned to be kept.
    async fn looping(
        name: &str,
        reviewer: ScriptedClient,
        executor: ScriptedClient,
        provider: Arc<dyn crate::workspace::WorkspaceProvider>,
    ) -> Looping {
        looping_with(name, "", reviewer, executor, provider).await
    }

    /// [`looping`] with more `[review.loop]` settings.
    async fn looping_with(
        name: &str,
        settings: &str,
        reviewer: ScriptedClient,
        executor: ScriptedClient,
        provider: Arc<dyn crate::workspace::WorkspaceProvider>,
    ) -> Looping {
        let lanes = ScriptedClient::new("m", []);
        looping_on(name, "o/r", settings, lanes, reviewer, executor, provider).await
    }

    /// [`looping`] on a pull request whose branch is in `head_repo`, with
    /// `lanes` answering the lane review.
    async fn looping_from(
        name: &str,
        head_repo: &'static str,
        lanes: ScriptedClient,
        reviewer: ScriptedClient,
        executor: ScriptedClient,
        provider: Arc<dyn crate::workspace::WorkspaceProvider>,
    ) -> Looping {
        looping_on(name, head_repo, "", lanes, reviewer, executor, provider).await
    }

    /// [`looping_from`] with more `[review.loop]` settings.
    async fn looping_on(
        name: &str,
        head_repo: &'static str,
        settings: &str,
        lanes: ScriptedClient,
        reviewer: ScriptedClient,
        executor: ScriptedClient,
        provider: Arc<dyn crate::workspace::WorkspaceProvider>,
    ) -> Looping {
        let (remote, head) = crate::git::tests::bare_remote(name).await;
        let config = loop_config_with(settings);
        let hub = Arc::new(LoopHub {
            remote: remote.path().to_path_buf(),
            head_repo,
            credential: None,
        });
        let mut f = fixture_on(DIFF, lanes, &config, head.as_str(), provider, Some(hub)).await;
        let (reviewer, executor) = (Arc::new(reviewer), Arc::new(executor));
        f.app.models.insert(
            "r".to_owned(),
            Arc::clone(&reviewer) as Arc<dyn ModelClient>,
        );
        f.app.models.insert(
            "f".to_owned(),
            Arc::clone(&executor) as Arc<dyn ModelClient>,
        );
        (f, reviewer, executor, remote)
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

    /// Nothing at all on the pull request (#285): the writer has no way
    /// left to post a finding, so a comment is all there could be.
    fn assert_nothing_posted(writer: &FakeWriter) {
        assert!(writer.replies.lock().unwrap().is_empty(), "comments");
    }

    #[tokio::test]
    async fn the_executor_gives_each_finding_one_verdict_and_nothing_is_posted() {
        let (f, reviewer, executor, remote) = looping(
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
        assert!(f.app.own_pushes.has(&head));
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
                    "executor".into(),
                    fixed_why,
                    head.clone(),
                    String::new()
                ),
                (
                    "f2".into(),
                    "rejected".into(),
                    "executor".into(),
                    "main returns (), see src/a.rs:1.".into(),
                    String::new(),
                    String::new()
                ),
                (
                    "f3".into(),
                    "rejected".into(),
                    "executor".into(),
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
        let fixing = executor.requests();
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
        let sessions = f.app.store.sessions(&run).await.unwrap();
        let names: Vec<_> = sessions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["executor", "reviewer"]);
        for session in ["reviewer", "executor"] {
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
    async fn a_finding_the_executor_gives_no_verdict_ends_unsettled() {
        let (f, _, executor, remote) = looping(
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
            executor.requests().len(),
            3,
            "nudged twice, then settled for it"
        );
        let findings = loop_findings(&f.app, &run).await;
        assert_eq!(findings.len(), 1);
        assert_eq!(
            (findings[0].1.as_str(), findings[0].3.as_str()),
            ("unsettled", "the executor gave no verdict")
        );
        let (after, _) = crate::git::tests::remote_feature(remote.path()).await;
        assert_eq!(before, after, "nothing pushed");
        assert_eq!(f.writer.finished.lock().unwrap()[0].open_findings, 1);
        assert_nothing_posted(&f.writer);
    }

    #[tokio::test]
    async fn a_reviewer_that_reports_a_fixed_finding_again_stops_the_loop() {
        let claim = "x must be 3, the caller divides by it";
        let (f, _, executor, remote) = looping(
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
        assert_eq!(
            executor.requests().len(),
            3,
            "the executor never got the repeat"
        );
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
        let (f, _, executor, _remote) = looping(
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
        assert_eq!(
            executor.requests().len(),
            3,
            "the executor never got round 2"
        );
        assert!(!report.outcome.unwrap().completed());
    }

    #[tokio::test]
    async fn a_round_the_time_ran_out_before_is_not_counted() {
        let (f, reviewer, executor, _remote) = looping_with(
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
        assert!(reviewer.requests().is_empty() && executor.requests().is_empty());
        let summary = report.summary.unwrap();
        assert!(summary.contains("ran 0 rounds"), "{summary}");
        assert!(
            summary.contains("before the reviewer's turn in round 1"),
            "{summary}"
        );
    }

    #[tokio::test]
    async fn the_loop_stops_at_its_time_limit() {
        let (f, reviewer, executor, _remote) = looping_with(
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
        assert!(executor.requests().is_empty());
        assert_nothing_posted(&f.writer);
    }

    #[tokio::test]
    async fn a_reviewer_that_never_finishes_its_round_is_not_convergence() {
        let (f, reviewer, executor, remote) = looping(
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
        assert!(executor.requests().is_empty());
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
        let (f, reviewer, executor, remote) = looping(
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
        assert!(reviewer.requests().is_empty() && executor.requests().is_empty());
        let (after, _) = crate::git::tests::remote_feature(remote.path()).await;
        assert_eq!(before, after, "nothing pushed");
        assert_no_stage_running(&f.app, &run).await;
    }

    #[tokio::test]
    async fn a_loop_stopped_after_it_pushed_still_reports_the_check_on_the_new_head() {
        let (f, _reviewer, _executor, remote) = looping(
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
        assert!(f.app.own_pushes.has(&head), "so nothing else reviews it");
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
    async fn a_loop_out_of_rounds_with_findings_open_is_no_pass() {
        let (mut f, _reviewer, _executor, _remote) = looping(
            "review-loop-rounds",
            ScriptedClient::new(
                "r",
                [
                    report(2, "x must be 3", None),
                    finish_round(),
                    say("Done for now."),
                ],
            ),
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
        f.app.settings.review.r#loop.as_mut().unwrap().max_rounds = 1;
        let run = RunId::parse("r-loop-rounds").unwrap();
        let report = run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();

        let outcome = report.outcome.unwrap();
        assert!(outcome.completed());
        assert_eq!(outcome.open_findings, 0, "the loop posts no findings");
        assert_eq!(outcome.check_conclusion(), CheckConclusion::Neutral);
        let summary = report.summary.unwrap();
        assert!(
            summary.starts_with("The review loop left findings open."),
            "{summary}"
        );
        assert!(summary.contains("it reached max_rounds (1)"), "{summary}");
        let finished = f.writer.finished.lock().unwrap().clone();
        assert_eq!(finished.len(), 2, "the queued commit and the new head");
        assert!(
            finished
                .iter()
                .all(|o| o.check_conclusion() != CheckConclusion::Success),
            "{finished:?}"
        );
    }

    #[tokio::test]
    async fn a_compacted_executor_keeps_its_own_history_not_the_reviewer_s_task() {
        let (mut f, reviewer, executor, _remote) = looping(
            "review-loop-compact",
            ScriptedClient::new(
                "r",
                [
                    report(2, "x must be 3", None),
                    finish_round(),
                    say("Done."),
                    report(1, "main must return a value", None),
                    finish_round(),
                    say("Done."),
                    report(1, "main needs a doc comment", None),
                    finish_round(),
                    say("Done."),
                ],
            ),
            ScriptedClient::new(
                "f",
                [
                    judge("f1", "rejected", "x is never divided by."),
                    say("Settled."),
                    judge("f2", "rejected", "main returns ()."),
                    say("Settled."),
                    judge("f3", "wont_fix", "a test fixture needs none."),
                    say("Settled."),
                ],
            ),
            Arc::new(crate::workspace::host::HostProvider),
        )
        .await;
        // Every conversation is over budget from round 2 on.
        f.app.settings.review.max_conversation_chars = 10;
        f.app.settings.review.r#loop.as_mut().unwrap().max_rounds = 3;
        let run = RunId::parse("r-loop-compact").unwrap();
        run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();

        let earlier = "What happened in the earlier rounds";
        // The reviewer's round 3 starts from its own task and the summary.
        let asked = reviewer.requests();
        assert_eq!(asked.len(), 9);
        let first = texts(&asked[6]).into_iter().next().unwrap();
        assert!(first.contains(earlier), "{first}");
        assert!(first.contains("get_file_diff"), "{first}");
        assert!(first.contains("f2 at src/a.rs:1"), "{first}");
        // The executor's round 3: the summary, never the reviewer's task or a
        // tool it does not have.
        let fixing = executor.requests();
        assert_eq!(fixing.len(), 6);
        let first = texts(&fixing[4]).into_iter().next().unwrap();
        assert!(first.contains(earlier), "{first}");
        assert!(first.contains("f2 at src/a.rs:1"), "{first}");
        assert!(!first.contains("Review pull request"), "{first}");
        assert!(!first.contains("get_file_diff"), "{first}");
        // #330: a compaction is on the run, with the round, never quiet.
        let events = f.app.store.events(&run).await.unwrap();
        let dropped: Vec<&str> = events
            .iter()
            .filter(|e| e.level == "warn" && e.message.contains("from its conversation"))
            .map(|e| e.message.as_str())
            .collect();
        assert_eq!(
            dropped,
            [
                "reviewer: dropped round 1 from its conversation before round 3 to stay under max_conversation_chars; the prompt cache is written again from there",
                "executor: dropped round 1 from its conversation before round 3 to stay under max_conversation_chars; the prompt cache is written again from there",
            ],
            "{events:?}"
        );
    }

    /// #330: in a three-round loop, each resumed round of the reviewer and
    /// of the executor starts with the round before byte for byte, both
    /// keep their prompt cache for the hour since each waits for the
    /// other, and behind a cache every turn after the first reads its
    /// history from it; the run keeps what each session read.
    #[tokio::test]
    async fn a_pairing_resumes_on_its_unchanged_history_and_reads_it_from_the_cache() {
        let (mut f, _, _, _remote) = looping(
            "review-loop-warm",
            ScriptedClient::new("unused", []),
            ScriptedClient::new("unused", []),
            Arc::new(crate::workspace::host::HostProvider),
        )
        .await;
        let reviewer = Arc::new(henk_llm::testing::CachingClient::new(ScriptedClient::new(
            "r",
            [
                report(2, "x must be 3", None),
                finish_round(),
                say("Done."),
                report(1, "main must return a value", None),
                finish_round(),
                say("Done."),
                finish_round(),
                say("Nothing more."),
            ],
        )));
        let executor = Arc::new(henk_llm::testing::CachingClient::new(ScriptedClient::new(
            "f",
            [
                judge("f1", "rejected", "x is never divided by."),
                say("Settled."),
                judge("f2", "rejected", "main returns ()."),
                say("Settled."),
            ],
        )));
        f.app.models.insert(
            "r".to_owned(),
            Arc::clone(&reviewer) as Arc<dyn ModelClient>,
        );
        f.app.models.insert(
            "f".to_owned(),
            Arc::clone(&executor) as Arc<dyn ModelClient>,
        );
        let run = RunId::parse("r-loop-warm").unwrap();
        run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();

        let reviews = reviewer.requests();
        assert_eq!(reviews.len(), 8);
        henk_llm::testing::assert_resumed_with_only_the_update(&reviews[2], &reviews[3]);
        henk_llm::testing::assert_resumed_with_only_the_update(&reviews[5], &reviews[6]);
        let fixes = executor.requests();
        assert_eq!(fixes.len(), 4);
        henk_llm::testing::assert_resumed_with_only_the_update(&fixes[1], &fixes[2]);
        for request in reviews.iter().chain(&fixes) {
            assert_eq!(request.cache_ttl, henk_llm::CacheTtl::Long);
        }

        for (who, usage) in [
            ("reviewer", reviewer.cache_usage()),
            ("executor", executor.cache_usage()),
        ] {
            assert_eq!(usage[0].0, 0, "{who}: the first turn writes the cache");
            for (turn, (read, _)) in usage.iter().enumerate().skip(1) {
                assert!(*read > 0, "{who}: turn {turn} read nothing: {usage:?}");
            }
        }
        let sessions = f.app.store.sessions(&run).await.unwrap();
        for session in &sessions {
            assert!(
                session.cache_read_tokens > 0 && session.cache_read_tokens < session.input_tokens,
                "{session:?}"
            );
            assert_eq!(
                session.input_tokens,
                session.cache_read_tokens + session.cache_write_tokens,
                "tokens in stay the whole prompt: {session:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_loop_cancelled_just_before_its_push_ends_cancelled_not_failed() {
        let (mut f, _reviewer, _executor, remote) = looping(
            "review-loop-cancel-push",
            ScriptedClient::new(
                "r",
                [
                    report(2, "x must be 3", None),
                    finish_round(),
                    say("Done for now."),
                ],
            ),
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
        // The project's check says when it runs, and waits until the review
        // was cancelled: the next step is the push.
        let signals = crate::git::ScratchDir::new("review-loop-cancel-push-signals").unwrap();
        let (running, go) = (signals.path().join("running"), signals.path().join("go"));
        let script = format!(
            ": > '{}'; while [ ! -e '{}' ]; do :; done",
            running.display(),
            go.display()
        );
        f.app
            .settings
            .roles
            .executor
            .as_mut()
            .unwrap()
            .check_commands = vec![vec!["/bin/sh".to_owned(), "-c".to_owned(), script]];
        let (before, _) = crate::git::tests::remote_feature(remote.path()).await;
        let run = RunId::parse("r-loop-cancel-push").unwrap();
        let cancel = f.app.shutdown.child_token();
        let _cancellable = f.app.cancels.register(run.clone(), cancel.clone());
        let (cancels, cancelled) = (f.app.cancels.clone(), run.clone());
        let watcher = tokio::spawn(async move {
            for _ in 0..1000 {
                if running.exists() {
                    assert!(cancels.cancel(&cancelled, "github:1234".to_owned()));
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            std::fs::write(go, "").unwrap();
        });
        let result = tokio::time::timeout(
            Duration::from_secs(20),
            run_review(&f.app, request(&run), cancel),
        )
        .await
        .expect("a cancelled loop ends promptly");
        watcher.await.unwrap();

        let error = result.unwrap_err();
        assert!(error.is::<CancelledBy>(), "{error:#}");
        let record = f.app.store.run(&run).await.unwrap().unwrap();
        assert_eq!(record.status, RunStatus::Cancelled);
        assert!(
            !posted_kinds(&f.writer).contains(&Some(MarkerKind::Failure)),
            "no failure notice"
        );
        let (after, _) = crate::git::tests::remote_feature(remote.path()).await;
        assert_eq!(before, after, "nothing pushed");
        assert_no_stage_running(&f.app, &run).await;
        // A cancel is a skip, never a failed Rounds stage.
        let stages = stage_list(&f.app, &run).await;
        let rounds = stages
            .iter()
            .find(|(stage, _, _)| *stage == henk_store::Stage::Rounds)
            .unwrap();
        assert_eq!(rounds.1, StageState::Skipped, "{stages:?}");
        assert!(
            stages
                .iter()
                .all(|(_, state, _)| *state != StageState::Failed),
            "{stages:?}"
        );
    }

    /// Pushes a commit to the remote's `feature` branch, so the branch has
    /// moved on from what a review was queued at.
    async fn move_branch(remote: &std::path::Path, name: &str) {
        let url = remote.to_string_lossy().into_owned();
        let (head, _) = crate::git::tests::remote_feature(remote).await;
        let later = crate::git::Checkout::clone_at(
            crate::git::ScratchDir::new(&format!("{name}-later")).unwrap(),
            &url,
            "feature",
            &CommitSha::parse(&head).unwrap(),
            None,
        )
        .await
        .unwrap();
        std::fs::write(later.path().join("src/a.rs"), "moved on\n").unwrap();
        later
            .commit(
                &henk_platform::push::CommitIdentity {
                    name: "Later".to_owned(),
                    email: "later@example.com".to_owned(),
                },
                "Later\n",
            )
            .await
            .unwrap();
        later.push("feature").await.unwrap();
    }

    /// A pull request the loop may not push to is reviewed by the reviewer
    /// alone (#307): one round, no executor, nothing pushed or posted, and the
    /// finding stands in the check, with why it was not fixed.
    #[tokio::test]
    async fn a_pull_request_from_a_fork_is_reviewed_by_the_reviewer_alone() {
        let (f, reviewer, executor, remote) = looping_from(
            "review-loop-fork",
            "someone/r",
            ScriptedClient::new("m", (0..4).map(|_| done())),
            ScriptedClient::new(
                "r",
                [report(2, "x must be 3", None), finish_round(), say("Done.")],
            ),
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
        assert_eq!(
            (record.loop_stop.as_deref(), record.loop_rounds),
            (Some("review_only"), Some(1))
        );
        let outcome = report.outcome.unwrap();
        assert!(outcome.completed());
        assert_eq!(outcome.check_conclusion(), CheckConclusion::Neutral);
        let summary = report.summary.unwrap();
        assert!(summary.starts_with("1 issue found."), "{summary}");
        assert!(
            summary.contains("reviewed only, since its branch is in another repository"),
            "{summary}"
        );
        assert!(
            summary.contains("f1 at src/a.rs:2 (x must be 3): unsettled: not fixed: its branch is in another repository"),
            "{summary}"
        );
        assert!(henk_domain::text::is_in_style(&summary), "{summary}");
        assert_eq!(outcome.summary(), summary, "the check says the same");

        assert!(!reviewer.requests().is_empty(), "the reviewer reviewed");
        assert!(executor.requests().is_empty(), "no executor");
        assert!(f.model.requests().is_empty(), "no session");
        let sessions = f.app.store.sessions(&run).await.unwrap();
        let names: Vec<_> = sessions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["reviewer"]);
        let findings = loop_findings(&f.app, &run).await;
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].1, "unsettled");
        assert!(
            findings[0]
                .3
                .starts_with("not fixed: its branch is in another repository"),
            "{findings:?}"
        );

        let (after, _) = crate::git::tests::remote_feature(remote.path()).await;
        assert_eq!(before, after, "nothing pushed");
        assert_eq!(
            *f.writer.finished_commits.lock().unwrap(),
            [before],
            "nothing pushed, one check"
        );
        assert_nothing_posted(&f.writer);
        assert_no_stage_running(&f.app, &run).await;
        let stages = stage_list(&f.app, &run).await;
        let push = stages
            .iter()
            .find(|(stage, _, _)| *stage == henk_store::Stage::Push)
            .unwrap();
        assert_eq!(push.1, StageState::Skipped, "{stages:?}");
    }

    /// On GitLab the commit status carries only the count, so the reviewer
    /// alone's summary, with the findings that stand, is the summary note:
    /// the merge request shows them (#307).
    #[tokio::test]
    async fn a_merge_request_from_a_fork_on_gitlab_gets_the_findings_in_a_summary_note() {
        let (mut f, _, executor, remote) = looping_from(
            "review-loop-fork-gitlab",
            "someone/r",
            ScriptedClient::new("m", []),
            ScriptedClient::new(
                "r",
                [report(2, "x must be 3", None), finish_round(), say("Done.")],
            ),
            ScriptedClient::new("f", []),
            Arc::new(crate::workspace::host::HostProvider),
        )
        .await;
        let gitlab = Arc::new(FakeWriter {
            head: f.writer.head.clone(),
            patches: f.writer.patches.clone(),
            platform: Some(Platform::GitLab),
            ..FakeWriter::default()
        });
        f.app.test_writer = Some(Arc::clone(&gitlab) as Arc<dyn PlatformWriter>);
        f.writer = gitlab;
        let (before, _) = crate::git::tests::remote_feature(remote.path()).await;
        let run = RunId::parse("r-loop-fork-gitlab").unwrap();
        let report = run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();

        let summary = report.summary.unwrap();
        let replies = f.writer.replies.lock().unwrap().clone();
        assert_eq!(replies.len(), 1, "one summary note: {replies:?}");
        let note = &replies[0].body;
        assert_eq!(Marker::parse(note).unwrap().kind, Some(MarkerKind::Summary));
        assert!(note.contains(&summary), "{note}");
        assert!(note.contains("x must be 3"), "{note}");
        assert!(
            note.contains("not fixed: its branch is in another repository"),
            "{note}"
        );
        assert!(note.contains("Run: "), "{note}");
        assert!(executor.requests().is_empty(), "no executor");
        let (after, _) = crate::git::tests::remote_feature(remote.path()).await;
        assert_eq!(before, after, "nothing pushed");
        let stages = stage_list(&f.app, &run).await;
        let publish = stages
            .iter()
            .find(|(stage, _, _)| *stage == henk_store::Stage::Publish)
            .unwrap();
        assert_eq!(publish.1, StageState::Done, "{stages:?}");
        assert_no_stage_running(&f.app, &run).await;
    }

    /// A reviewer alone that finds nothing passes the check.
    #[tokio::test]
    async fn a_pull_request_from_a_fork_with_nothing_to_find_passes() {
        let (f, reviewer, executor, _remote) = looping_from(
            "review-loop-fork-clean",
            "someone/r",
            ScriptedClient::new("m", []),
            ScriptedClient::new("r", [finish_round(), say("Nothing.")]),
            ScriptedClient::new("f", []),
            Arc::new(crate::workspace::host::HostProvider),
        )
        .await;
        let run = RunId::parse("r-loop-fork-clean").unwrap();
        let report = run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();

        let outcome = report.outcome.unwrap();
        assert_eq!(outcome.check_conclusion(), CheckConclusion::Success);
        let summary = report.summary.unwrap();
        assert!(summary.starts_with("No issues found."), "{summary}");
        assert!(summary.contains("reviewed only, since"), "{summary}");
        assert!(!summary.contains("Open findings"), "{summary}");
        assert!(executor.requests().is_empty());
        // Alone, the reviewer waits for nobody: each turn refreshes the
        // short cache (#330).
        assert!(
            reviewer
                .requests()
                .iter()
                .all(|r| r.cache_ttl == henk_llm::CacheTtl::Short)
        );
        let record = f.app.store.run(&run).await.unwrap().unwrap();
        assert_eq!(record.loop_stop.as_deref(), Some("review_only"));
    }

    /// The reviewer alone posts nothing, so a cancel posts no notice
    /// either: the check says it.
    #[tokio::test]
    async fn a_cancelled_review_by_the_reviewer_alone_posts_nothing() {
        let (f, reviewer, _, _remote) = looping_from(
            "review-loop-fork-cancel",
            "someone/r",
            ScriptedClient::new("m", []),
            ScriptedClient::new("r", [finish_round(), say("Nothing.")])
                .with_delay(Duration::from_secs(30)),
            ScriptedClient::new("f", []),
            Arc::new(crate::workspace::host::HostProvider),
        )
        .await;
        let run = RunId::parse("r-loop-fork-cancel").unwrap();
        let cancel = f.app.shutdown.child_token();
        let _cancellable = f.app.cancels.register(run.clone(), cancel.clone());
        let result = cancelled_once(
            Box::pin(run_review(&f.app, request(&run), cancel)),
            "the reviewer's first request",
            || !reviewer.requests().is_empty(),
            &f.app.cancels,
            &run,
        )
        .await;

        assert!(result.is_err_and(|e| e.is::<CancelledBy>()));
        assert_nothing_posted(&f.writer);
        assert_no_stage_running(&f.app, &run).await;
    }

    /// A branch that moved after the review was queued is not pushed to,
    /// and the reviewer alone reviews what was queued, not the new head.
    #[tokio::test]
    async fn a_branch_that_moved_after_queueing_is_reviewed_alone_at_the_queued_commit() {
        let peek = Arc::new(Peek::default());
        let (f, reviewer, executor, remote) = looping_from(
            "review-loop-moved",
            "o/r",
            ScriptedClient::new("m", []),
            ScriptedClient::new("r", [finish_round(), say("Nothing.")]),
            ScriptedClient::new("f", [edit("let x = 1;", "let x = 3;")]),
            Arc::clone(&peek) as Arc<dyn crate::workspace::WorkspaceProvider>,
        )
        .await;
        let (queued_at, _) = crate::git::tests::remote_feature(remote.path()).await;
        move_branch(remote.path(), "review-loop-moved").await;
        let (moved, _) = crate::git::tests::remote_feature(remote.path()).await;
        assert_ne!(queued_at, moved);
        let run = RunId::parse("r-loop-moved").unwrap();
        let mut queued = request(&run);
        queued.commit = Some(CommitSha::parse(&queued_at).unwrap());
        let report = run_review(&f.app, queued, CancellationToken::new())
            .await
            .unwrap();

        let summary = report.summary.unwrap();
        assert!(
            summary.contains("reviewed only, since the branch moved to"),
            "{summary}"
        );
        assert!(report.outcome.unwrap().completed(), "{summary}");
        assert!(!reviewer.requests().is_empty() && executor.requests().is_empty());
        let seen = peek.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_ne!(seen[0], "moved on\n", "the queued commit, not the new head");
        let (after, _) = crate::git::tests::remote_feature(remote.path()).await;
        assert_eq!(moved, after, "nothing pushed");
        assert_no_stage_running(&f.app, &run).await;
    }

    /// A normal loop's check carries its report: rounds, pushes and why it
    /// stopped (#307).
    #[tokio::test]
    async fn the_loop_s_report_is_in_its_check() {
        let (f, _, _, _remote) = looping(
            "review-loop-report",
            ScriptedClient::new("r", [finish_round(), say("Nothing.")]),
            ScriptedClient::new("f", []),
            Arc::new(crate::workspace::host::HostProvider),
        )
        .await;
        let run = RunId::parse("r-loop-report").unwrap();
        let report = run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();
        let summary = report.outcome.unwrap().summary();
        assert!(
            summary.starts_with("No issues found.\n\nThe review loop ran 1 round"),
            "{summary}"
        );
        assert!(summary.ends_with("stopped: converged, the reviewer found nothing more."));
    }

    /// A fake backend that answers `script`, seen through [`Peek`].
    fn peeking(script: &[(&str, &str, Duration)]) -> Arc<Peek> {
        let mut inner = crate::workspace::fake::FakeProvider::default();
        for (command, output, delay) in script {
            inner.script.insert(
                (*command).to_owned(),
                crate::workspace::fake::Scripted {
                    output: (*output).to_owned(),
                    delay: *delay,
                    ..crate::workspace::fake::Scripted::default()
                },
            );
        }
        Arc::new(Peek {
            inner,
            seen: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// The reviewer reads and searches its workspace, at the reviewed
    /// commit, and every call it makes there is on the run under its name
    /// (#190).
    #[tokio::test]
    async fn the_reviewer_searches_and_reads_its_workspace_and_its_calls_are_on_the_run() {
        let reviewer = ScriptedClient::new(
            "r",
            [
                call(
                    "search",
                    serde_json::json!({"pattern": "let x = \\d", "glob": "*.rs"}),
                ),
                call(
                    "read_file",
                    serde_json::json!({"path": "src/a.rs", "start_line": 2, "end_line": 2}),
                ),
                finish_round(),
                say("Nothing."),
            ],
        );
        let peek = peeking(&[]);
        let (f, reviewer, _, _remote) = looping(
            "henk-loop-tools",
            reviewer,
            ScriptedClient::new("f", []),
            Arc::clone(&peek) as Arc<dyn crate::workspace::WorkspaceProvider>,
        )
        .await;
        let run = RunId::parse("r-loop-tools").unwrap();
        run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();
        let (names, results, _) = seen(&reviewer);
        for tool in ["list_files", "read_file", "search", "get_file_diff"] {
            assert!(names.iter().any(|n| n == tool), "{tool}: {names:?}");
        }
        assert!(
            results.iter().any(|r| r == "src/a.rs:2: let x = 1;\n"),
            "{results:?}"
        );
        assert!(
            results
                .iter()
                .any(|r| r == "    2|     let x = 1;\n[3 lines; continue with start_line = 3]\n"),
            "{results:?}"
        );
        let calls = f.app.store.tool_calls(&run).await.unwrap();
        let in_workspace: Vec<(&str, &str, &str)> = calls
            .iter()
            .filter(|c| c.origin == "workspace")
            .map(|c| (c.session.as_str(), c.tool.as_str(), c.outcome.as_str()))
            .collect();
        assert_eq!(
            in_workspace,
            [
                ("reviewer", "search", "ok"),
                ("reviewer", "read_file", "ok")
            ],
            "{calls:?}"
        );
        assert_eq!((peek.inner.opened(), peek.inner.live()), (1, 0));
    }

    /// A command the reviewer runs is on the run's timeline, as
    /// `henk runs show` lists it.
    #[tokio::test]
    async fn the_reviewer_runs_a_command_and_the_timeline_has_it() {
        let reviewer = ScriptedClient::new(
            "r",
            [
                call(
                    "bash",
                    serde_json::json!({"command": "cargo test -q parse"}),
                ),
                finish_round(),
                say("Nothing."),
            ],
        );
        let peek = peeking(&[
            ("true", "", Duration::ZERO),
            ("bash -c cargo test -q parse", "1 passed", Duration::ZERO),
        ]);
        let (f, reviewer, _, _remote) = looping_with(
            "henk-loop-bash",
            &ssh_loop_settings("true"),
            reviewer,
            ScriptedClient::new("f", []),
            Arc::clone(&peek) as Arc<dyn crate::workspace::WorkspaceProvider>,
        )
        .await;
        let run = RunId::parse("r-loop-bash").unwrap();
        run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();
        let (names, results, _) = seen(&reviewer);
        assert!(names.iter().any(|n| n == "bash"), "{names:?}");
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
                .starts_with("exec bash -c cargo test -q parse exit 0")),
            "henk runs show lists the reviewer's command: {events:?}"
        );
        assert_eq!(peek.inner.live(), 0);
    }

    /// A review loop cancelled while its reviewer works closes its
    /// workspace, not just drops it.
    #[tokio::test]
    async fn a_cancelled_loop_closes_its_workspace() {
        let peek = peeking(&[]);
        let (f, reviewer, _, _remote) = looping(
            "henk-loop-ws-cancel",
            ScriptedClient::new("r", converge()).with_delay(Duration::from_secs(30)),
            ScriptedClient::new("f", []),
            Arc::clone(&peek) as Arc<dyn crate::workspace::WorkspaceProvider>,
        )
        .await;
        let run = RunId::parse("r-loop-ws-cancel").unwrap();
        let cancel = f.app.shutdown.child_token();
        let _cancellable = f.app.cancels.register(run.clone(), cancel.clone());
        let result = cancelled_once(
            Box::pin(run_review(&f.app, request(&run), cancel)),
            "the reviewer's first request",
            || !reviewer.requests().is_empty(),
            &f.app.cancels,
            &run,
        )
        .await;
        assert!(result.is_err_and(|e| e.is::<CancelledBy>()));
        assert_eq!(peek.inner.opened(), 1);
        assert_eq!(peek.inner.live(), 0);
        assert_eq!(peek.inner.unclosed(), 0, "closed, not just dropped");
    }

    /// A review loop cancelled during its workspace's setup stops at once,
    /// and the workspace dropped mid-setup is destroyed.
    #[tokio::test]
    async fn a_loop_cancelled_during_setup_stops_at_once_and_keeps_no_workspace() {
        let peek = peeking(&[("make deps", "deps ready", Duration::from_secs(45))]);
        let (f, reviewer, _, _remote) = looping_with(
            "henk-loop-ws-cancel-setup",
            &ssh_loop_settings("make\", \"deps"),
            ScriptedClient::new("r", converge()),
            ScriptedClient::new("f", []),
            Arc::clone(&peek) as Arc<dyn crate::workspace::WorkspaceProvider>,
        )
        .await;
        let run = RunId::parse("r-loop-ws-cancel-setup").unwrap();
        let cancel = f.app.shutdown.child_token();
        let _cancellable = f.app.cancels.register(run.clone(), cancel.clone());
        let result = cancelled_once(
            Box::pin(run_review(&f.app, request(&run), cancel)),
            "an open workspace",
            || peek.inner.opened() == 1,
            &f.app.cancels,
            &run,
        )
        .await;
        assert!(result.is_err_and(|e| e.is::<CancelledBy>()));
        assert!(
            reviewer.requests().is_empty(),
            "the setup is not waited for"
        );
        assert_eq!(peek.inner.opened(), 1);
        assert_eq!(peek.inner.live(), 0, "dropped mid-setup, so destroyed");
        assert_no_stage_running(&f.app, &run).await;
    }

    /// A review loop is announced as it happens, for the live view: the
    /// run's start first and its end last, the reviewer's session before
    /// its finding, the finding and then its verdict, and the tool calls,
    /// in feed order (#202).
    #[tokio::test]
    async fn a_review_loop_is_announced_as_it_happens_for_the_live_view() {
        let (f, _, _, _remote) = looping(
            "henk-loop-live",
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
        let run = RunId::parse("r-loop-live").unwrap();
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
        let verdicts: Vec<Option<String>> = seen
            .iter()
            .filter_map(|c| match &c.kind {
                crate::live::ChangeKind::Draft(d) => {
                    Some(d.decision.as_ref().map(|x| x.verdict.as_str().to_owned()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            verdicts.first(),
            Some(&None),
            "reported first: {verdicts:?}"
        );
        assert_eq!(
            verdicts.last(),
            Some(&Some("fixed".to_owned())),
            "then fixed: {verdicts:?}"
        );
        let started = seen
            .iter()
            .position(|c| matches!(&c.kind, crate::live::ChangeKind::Sessions(l) if l.iter().any(|l| l.name == "reviewer")))
            .unwrap();
        let reported = seen
            .iter()
            .position(|c| matches!(c.kind, crate::live::ChangeKind::Draft(_)))
            .unwrap();
        assert!(started < reported, "the reviewer starts before it reports");
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
    async fn a_review_shows_its_check_queued() {
        let f = fixture_on(
            DIFF,
            ScriptedClient::new("m", []),
            CONFIG,
            SHA,
            Arc::new(crate::workspace::host::HostProvider),
            None,
        )
        .await;
        let run = RunId::parse("r-loop-queued").unwrap();
        let mut queued = request(&run);
        queued.commit = Some(CommitSha::parse(SHA).unwrap());
        let handle = crate::coordinator::queue_check(&f.app, &queued).await;
        assert_eq!(handle, Some(ReviewHandle("queued-1".to_owned())));
        assert_eq!(f.writer.queued.lock().unwrap().len(), 1);
    }

    /// The requester's `[[people]]` entry: Discord id 3 is user 77 on both
    /// platforms.
    const PEOPLE: &str = "[[people]]\ndiscord_id = 3\nname = \"Lead\"\nrole = \"team lead\"\ngithub_id = 77\ngitlab_id = 77\n";

    /// Gives the fixture `config` in place of the one it was built with.
    fn configure(f: &mut Fixture, config: &str) {
        f.app.settings = Config::parse(config)
            .and_then(Config::into_settings)
            .unwrap_or_else(|e| panic!("{e}"));
    }

    /// The loop's configuration with `settings` in `[review.loop]` and
    /// `extra` after `[roles.executor]`.
    fn loop_config_and(settings: &str, extra: &str) -> String {
        format!("{}{extra}", loop_config_with(settings))
    }

    /// A reviewer that finds f1 in round 1 and nothing in round 2.
    fn one_finding() -> Vec<Result<Completion, henk_llm::LlmError>> {
        vec![
            report(2, "x must be 3", None),
            finish_round(),
            say("Done for now."),
            finish_round(),
            say("Nothing more."),
        ]
    }

    /// An executor that fixes f1.
    fn fixes_f1() -> Vec<Result<Completion, henk_llm::LlmError>> {
        vec![
            edit("let x = 1;", "let x = 3;"),
            judge("f1", "fixed", "x is 3 now."),
            say("Fixed."),
        ]
    }

    /// The trailer block of the commit on `feature`: its last paragraph.
    async fn pushed_trailers(remote: &std::path::Path) -> Vec<String> {
        let message = crate::git::tests::remote_log(remote, "%B").await;
        let (_, block) = message.trim_end().rsplit_once("\n\n").unwrap();
        block.lines().map(str::to_owned).collect()
    }

    /// The commands on a run's timeline.
    async fn execs(app: &App, run: &RunId) -> Vec<String> {
        app.store
            .events(run)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.message)
            .filter(|m| m.starts_with("exec "))
            .collect()
    }

    /// A loop on the host whose executor fixes f1, with `extra` after
    /// `[roles.executor]`, asked for by the Team Lead in [`PEOPLE`]; the
    /// remote is returned to be kept.
    async fn one_fix(name: &str, extra: &str) -> (Fixture, crate::git::ScratchDir, ReviewReport) {
        one_fix_by(name, extra, Some("discord:3")).await
    }

    /// [`one_fix`], asked for by `requester`, or by nobody.
    async fn one_fix_by(
        name: &str,
        extra: &str,
        requester: Option<&str>,
    ) -> (Fixture, crate::git::ScratchDir, ReviewReport) {
        let (mut f, _, _, remote) = looping(
            name,
            ScriptedClient::new("r", one_finding()),
            ScriptedClient::new("f", fixes_f1()),
            Arc::new(crate::workspace::host::HostProvider),
        )
        .await;
        configure(&mut f, &loop_config_and("", extra));
        let run = RunId::parse(format!("r-{name}")).unwrap();
        let asked = ReviewRequest {
            requester: requester.map(str::to_owned),
            ..request(&run)
        };
        let report = run_review(&f.app, asked, CancellationToken::new())
            .await
            .unwrap();
        (f, remote, report)
    }

    #[tokio::test]
    async fn a_repository_that_enables_it_gets_the_requesters_signoff_on_the_executor_s_commit() {
        let (_f, remote, _) = one_fix(
            "loop-tr-signoff",
            &format!("{PEOPLE}[roles.executor.repositories.\"O/R\"]\nrequester_signoff = true\n"),
        )
        .await;
        assert_eq!(
            pushed_trailers(remote.path()).await,
            [
                "Henk-Run: r-loop-tr-signoff",
                "Requested-by: discord:3",
                "Co-authored-by: alice <77+alice@users.noreply.github.com>",
                "Signed-off-by: alice <77+alice@users.noreply.github.com>",
                "Signed-off-by: meneer-henk[bot] <1+meneer-henk[bot]@users.noreply.github.com>",
            ]
        );
    }

    #[tokio::test]
    async fn a_configured_identity_is_author_committer_and_signoff_of_the_executor_s_commit() {
        let (_f, remote, _) = one_fix(
            "loop-tr-identity",
            &format!(
                "{PEOPLE}commit_name = \"Lead Person\"\ncommit_email = \"lead@example.com\"\n\
                 [roles.executor.identity]\nname = \"Meneer Henk\"\nemail = \"henk@example.com\"\n"
            ),
        )
        .await;
        assert_eq!(
            crate::git::tests::remote_log(remote.path(), "%an <%ae>|%cn <%ce>")
                .await
                .trim(),
            "Meneer Henk <henk@example.com>|Meneer Henk <henk@example.com>"
        );
        let trailers = pushed_trailers(remote.path()).await;
        assert_eq!(
            trailers[2..],
            [
                "Co-authored-by: Lead Person <lead@example.com>",
                "Signed-off-by: Meneer Henk <henk@example.com>",
            ],
            "the per-person override wins over the noreply address"
        );
    }

    #[tokio::test]
    async fn no_finding_or_verdict_text_ever_becomes_a_trailer() {
        let planted =
            "Signed-off-by: someone <x@y.example>\nCo-authored-by: Mallory <m@evil.example>";
        let (mut f, _, _, remote) = looping(
            "loop-tr-injection",
            ScriptedClient::new(
                "r",
                [
                    call(
                        "report_finding",
                        serde_json::json!({
                            "path": "src/a.rs",
                            "line": 2,
                            "claim": "x must be 3",
                            "why": format!("the caller divides by it.\n\n{planted}"),
                            "fix": format!("make it 3\n\n{planted}"),
                        }),
                    ),
                    finish_round(),
                    say("Done for now."),
                    finish_round(),
                    say("Nothing more."),
                ],
            ),
            ScriptedClient::new(
                "f",
                [
                    edit("let x = 1;", "let x = 3;"),
                    judge("f1", "fixed", &format!("x is 3 now.\n\n{planted}")),
                    say(&format!("Fixed.\n\n{planted}")),
                ],
            ),
            Arc::new(crate::workspace::host::HostProvider),
        )
        .await;
        configure(&mut f, &loop_config_and("", PEOPLE));
        let run = RunId::parse("r-loop-tr-injection").unwrap();
        run_review(
            &f.app,
            request_by(&run, "discord:3"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let trailers = pushed_trailers(remote.path()).await;
        assert_eq!(trailers.len(), 4, "{trailers:?}");
        let message = crate::git::tests::remote_log(remote.path(), "%B").await;
        assert!(message.contains("review loop round 1"), "{message}");
        for planted in ["x@y.example", "evil", "Mallory"] {
            assert!(!message.contains(planted), "{message}");
        }
    }

    #[tokio::test]
    async fn a_requester_without_an_account_gets_no_trailers_and_the_loop_says_so() {
        let (f, remote, _) = one_fix("loop-tr-noaccount", "").await;
        assert_eq!(
            pushed_trailers(remote.path()).await,
            [
                "Henk-Run: r-loop-tr-noaccount",
                "Requested-by: discord:3",
                "Signed-off-by: meneer-henk[bot] <1+meneer-henk[bot]@users.noreply.github.com>",
            ]
        );
        let events = f
            .app
            .store
            .events(&RunId::parse("r-loop-tr-noaccount").unwrap())
            .await
            .unwrap();
        assert!(
            events
                .iter()
                .any(|e| e.level == "info" && e.message.contains("no requester trailers")),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn a_login_that_cannot_be_read_stops_the_loop_before_the_push() {
        let (f, remote, report) = one_fix(
            "loop-tr-nologin",
            &PEOPLE.replace("github_id = 77", "github_id = 78"),
        )
        .await;
        let summary = report.summary.unwrap();
        assert!(summary.contains("requester's login"), "{summary}");
        let run = RunId::parse("r-loop-tr-nologin").unwrap();
        let record = f.app.store.run(&run).await.unwrap().unwrap();
        assert_eq!(record.loop_stop.as_deref(), Some("push_refused"));
        let (after, _) = crate::git::tests::remote_feature(remote.path()).await;
        assert_eq!(after, f.head.as_str(), "nothing pushed");
    }

    #[tokio::test]
    async fn a_review_nobody_known_asked_for_declares_no_requester() {
        // Not the executor's Team Lead, whom PEOPLE could credit: nobody
        // asked whom Henk knows by id (#399). A webhook sender's login is
        // a display name, so it is nobody too.
        for (case, requester) in [("none", None), ("login", Some("alice"))] {
            let name = format!("loop-tr-nobody-{case}");
            let (f, remote, _) = one_fix_by(&name, PEOPLE, requester).await;
            assert_eq!(
                pushed_trailers(remote.path()).await,
                [
                    format!("Henk-Run: r-{name}"),
                    "Signed-off-by: meneer-henk[bot] <1+meneer-henk[bot]@users.noreply.github.com>"
                        .to_owned(),
                ],
                "{case}"
            );
            let events = f
                .app
                .store
                .events(&RunId::parse(format!("r-{name}")).unwrap())
                .await
                .unwrap();
            assert!(
                events
                    .iter()
                    .any(|e| e.message.contains("nobody Henk knows by id asked")),
                "{case}: {events:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_dashboard_review_credits_the_signed_in_github_user() {
        let (_f, remote, _) = one_fix_by("loop-tr-dashboard", PEOPLE, Some("github:77")).await;
        assert_eq!(
            pushed_trailers(remote.path()).await,
            [
                "Henk-Run: r-loop-tr-dashboard",
                "Requested-by: github:77",
                "Co-authored-by: alice <77+alice@users.noreply.github.com>",
                "Signed-off-by: meneer-henk[bot] <1+meneer-henk[bot]@users.noreply.github.com>",
            ]
        );
    }

    #[tokio::test]
    async fn an_mcp_review_names_its_token_and_credits_no_person() {
        let (_f, remote, _) = one_fix_by("loop-tr-mcp", PEOPLE, Some("mcp:claude")).await;
        assert_eq!(
            pushed_trailers(remote.path()).await,
            [
                "Henk-Run: r-loop-tr-mcp",
                "Requested-by: mcp:claude",
                "Signed-off-by: meneer-henk[bot] <1+meneer-henk[bot]@users.noreply.github.com>",
            ]
        );
    }

    /// The ssh backend with the runner script run here in its test mode:
    /// the whole loop, through the runner, without a sandbox host.
    fn ssh_backend(
        name: &str,
    ) -> (
        Arc<crate::workspace::remote::tests::LocalRunner>,
        Arc<dyn crate::workspace::WorkspaceProvider>,
    ) {
        use crate::workspace::remote::tests::LocalRunner;
        let runner = LocalRunner::new(name);
        let provider = crate::workspace::ssh::SshProvider::with_runner(
            Arc::clone(&runner) as Arc<dyn crate::workspace::remote::Runner>
        );
        (runner, Arc::new(provider))
    }

    /// A loop on `provider` whose reviewer finds two things and then
    /// nothing. With `fixes`, the executor fixes the first and rejects the
    /// second, and that one commit is pushed, as Henk, with the default
    /// trailers, after the checks pass; without, it rejects both, and
    /// nothing is checked or pushed.
    async fn executor_on(
        name: &str,
        provider: Arc<dyn crate::workspace::WorkspaceProvider>,
        fixes: bool,
    ) {
        let reviewer = ScriptedClient::new(
            "r",
            [
                report(2, "x must be 3", None),
                report(1, "main must return a value", None),
                finish_round(),
                say("Done for now."),
                finish_round(),
                say("Nothing more."),
            ],
        );
        let first = if fixes {
            vec![
                edit("let x = 1;", "let x = 3;"),
                judge("f1", "fixed", "x is 3 now."),
            ]
        } else {
            vec![judge("f1", "rejected", "the caller never divides by x.")]
        };
        let executor = first
            .into_iter()
            .chain([judge("f2", "rejected", "main returns ()."), say("Settled.")]);
        let settings = ssh_loop_settings("true");
        let (mut f, _, _, remote) = looping_with(
            name,
            &settings,
            reviewer,
            ScriptedClient::new("f", executor),
            provider,
        )
        .await;
        configure(
            &mut f,
            &loop_config_and(
                &settings,
                &format!("check_commands = [[\"true\"]]\n{PEOPLE}"),
            ),
        );
        let run = RunId::parse(format!("r-{name}")).unwrap();
        let report = run_review(
            &f.app,
            request_by(&run, "discord:3"),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let record = f.app.store.run(&run).await.unwrap().unwrap();
        assert_eq!(record.status, RunStatus::Finished, "{:?}", record.error);
        assert_eq!(record.loop_stop.as_deref(), Some("converged"));
        let (head, log) = crate::git::tests::remote_feature(remote.path()).await;
        let checks = execs(&f.app, &run)
            .await
            .into_iter()
            .filter(|e| e.starts_with("exec true exit 0 in "))
            .count();
        let summary = report.summary.unwrap();
        if !fixes {
            assert_eq!(head, f.head.as_str(), "nothing pushed");
            assert_eq!(checks, 1, "the setup step only: no change, no checks");
            assert!(summary.contains("pushed 0 commits"), "{summary}");
            return;
        }
        assert_ne!(head, f.head.as_str(), "pushed");
        assert!(summary.contains("pushed 1 commit "), "{summary}");
        assert!(f.app.own_pushes.has(&head));
        assert!(
            log.starts_with("meneer-henk[bot] <1+meneer-henk[bot]@users.noreply.github.com>"),
            "{log}"
        );
        assert!(log.contains("review loop round 1"), "{log}");
        assert_eq!(
            pushed_trailers(remote.path()).await,
            [
                format!("Henk-Run: r-{name}"),
                "Requested-by: discord:3".to_owned(),
                "Co-authored-by: alice <77+alice@users.noreply.github.com>".to_owned(),
                "Signed-off-by: meneer-henk[bot] <1+meneer-henk[bot]@users.noreply.github.com>"
                    .to_owned(),
            ],
            "the default trailers, last and in order"
        );
        let (changed, content) =
            crate::git::tests::remote_change(remote.path(), f.head.as_str(), "src/a.rs").await;
        assert_eq!(changed, ["src/a.rs"]);
        assert_eq!(content, "fn main() {\n    let x = 3;\n}\n");
        assert_eq!(checks, 2, "the setup step and the check");
    }

    #[tokio::test]
    async fn the_executor_s_fix_is_pushed_as_one_commit_on_the_ssh_backend() {
        let (runner, provider) = ssh_backend("henk-loop-ssh-fix");
        executor_on("loop-ssh-fix", provider, true).await;
        assert_eq!(
            std::fs::read_dir(runner.base()).unwrap().count(),
            0,
            "the workspace is removed"
        );
    }

    #[tokio::test]
    async fn without_a_change_the_executor_pushes_nothing_on_the_ssh_backend() {
        let (runner, provider) = ssh_backend("henk-loop-ssh-nochange");
        executor_on("loop-ssh-nochange", provider, false).await;
        assert_eq!(std::fs::read_dir(runner.base()).unwrap().count(), 0);
    }

    #[tokio::test]
    #[ignore = "needs a sandbox host (HENK_TEST_SSH_*)"]
    async fn live_the_executor_s_fix_is_pushed_from_a_real_sandbox_host() {
        let provider =
            crate::workspace::ssh::SshProvider::new(crate::workspace::ssh::tests::live_target());
        executor_on("loop-ssh-live-fix", Arc::new(provider.clone()), true).await;
        executor_on("loop-ssh-live-nochange", Arc::new(provider), false).await;
    }

    #[tokio::test]
    #[ignore = "needs a cluster (HENK_TEST_KUBE_*)"]
    async fn live_kube_the_executor_s_fix_is_pushed_from_its_pod() {
        use crate::workspace::kubernetes::tests::{LIVE, live_provider, pods_left};
        let _one = LIVE.lock().await;
        let provider = live_provider().await;
        executor_on("loop-kube-live-fix", Arc::new(provider.clone()), true).await;
        executor_on("loop-kube-live-nochange", Arc::new(provider.clone()), false).await;
        assert_eq!(pods_left(&provider).await, Vec::<String>::new());
    }

    /// An executor that, when first asked, has someone push to the branch
    /// before it answers, as a person pushing while Henk works would.
    struct PushedUnderneath {
        script: ScriptedClient,
        remote: std::path::PathBuf,
        name: String,
        moved: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl ModelClient for PushedUnderneath {
        fn model(&self) -> &'static str {
            "f"
        }
        async fn complete(
            &self,
            request: &henk_llm::CompletionRequest,
        ) -> Result<Completion, henk_llm::LlmError> {
            if !self.moved.swap(true, std::sync::atomic::Ordering::SeqCst) {
                move_branch(&self.remote, &self.name).await;
            }
            self.script.complete(request).await
        }
    }

    #[tokio::test]
    async fn a_branch_that_moved_while_the_executor_worked_gets_nothing_pushed() {
        let (runner, ssh) = ssh_backend("henk-loop-moved-ssh");
        let host: Arc<dyn crate::workspace::WorkspaceProvider> =
            Arc::new(crate::workspace::host::HostProvider);
        for (backend, provider, settings) in [
            ("host", host, String::new()),
            ("ssh", ssh, ssh_loop_settings("true")),
        ] {
            let name = format!("loop-moved-{backend}");
            let (mut f, _, _, remote) = looping_with(
                &name,
                &settings,
                ScriptedClient::new("r", one_finding()),
                ScriptedClient::new("f", []),
                provider,
            )
            .await;
            f.app.models.insert(
                "f".to_owned(),
                Arc::new(PushedUnderneath {
                    script: ScriptedClient::new("f", fixes_f1()),
                    remote: remote.path().to_path_buf(),
                    name: name.clone(),
                    moved: std::sync::atomic::AtomicBool::new(false),
                }),
            );
            let run = RunId::parse(format!("r-{name}")).unwrap();
            let error = run_review(&f.app, request(&run), CancellationToken::new())
                .await
                .unwrap_err();
            assert!(
                format!("{error:#}").contains("the branch moved"),
                "{backend}: {error:#}"
            );
            let record = f.app.store.run(&run).await.unwrap().unwrap();
            assert_eq!(record.status, RunStatus::Failed, "{backend}");
            let (_, log) = crate::git::tests::remote_feature(remote.path()).await;
            assert!(
                log.starts_with("Later <later@example.com>"),
                "{backend}: only the other push is on the branch: {log}"
            );
            let findings = loop_findings(&f.app, &run).await;
            assert_ne!(findings[0].1, "fixed", "{backend}: {findings:?}");
            assert_no_stage_running(&f.app, &run).await;
        }
        assert_eq!(std::fs::read_dir(runner.base()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn on_a_sandbox_the_executor_runs_its_own_commands_and_their_change_is_pushed() {
        let command = "sed -i 's/x = 1/x = 3/' src/a.rs";
        let mut provider = crate::workspace::fake::FakeProvider::default();
        provider.script.insert(
            "true".to_owned(),
            crate::workspace::fake::Scripted::default(),
        );
        provider.script.insert(
            format!("bash -c {command}"),
            crate::workspace::fake::Scripted {
                writes: vec![(
                    "src/a.rs".to_owned(),
                    b"fn main() {\n    let x = 3;\n}\n".to_vec(),
                )],
                ..crate::workspace::fake::Scripted::default()
            },
        );
        let (f, _, executor, remote) = looping_with(
            "loop-executor-bash",
            &ssh_loop_settings("true"),
            ScriptedClient::new("r", one_finding()),
            ScriptedClient::new(
                "f",
                [
                    call("bash", serde_json::json!({"command": command})),
                    judge("f1", "fixed", "x is 3 now."),
                    say("Fixed."),
                ],
            ),
            Arc::new(provider),
        )
        .await;
        let run = RunId::parse("r-loop-executor-bash").unwrap();
        run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();
        let (changed, content) =
            crate::git::tests::remote_change(remote.path(), f.head.as_str(), "src/a.rs").await;
        assert_eq!(
            changed,
            ["src/a.rs"],
            "a change made through bash is pushed"
        );
        assert_eq!(content, "fn main() {\n    let x = 3;\n}\n");
        let (names, _, system) = seen(&executor);
        assert!(names.iter().any(|n| n == "bash"), "{names:?}");
        assert!(names.iter().any(|n| n == "run_checks"), "{names:?}");
        assert!(
            system.contains("commands of your own in the checkout with `bash`"),
            "{system}"
        );
    }

    #[tokio::test]
    async fn on_the_host_the_executor_gets_no_shell() {
        let (f, _, executor, _remote) = looping(
            "loop-executor-no-bash",
            ScriptedClient::new("r", one_finding()),
            ScriptedClient::new(
                "f",
                [
                    judge("f1", "rejected", "the caller never divides by x."),
                    say("Settled."),
                ],
            ),
            Arc::new(crate::workspace::host::HostProvider),
        )
        .await;
        let run = RunId::parse("r-loop-executor-no-bash").unwrap();
        run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();
        let (names, _, system) = seen(&executor);
        assert!(!names.iter().any(|n| n == "bash"), "{names:?}");
        assert!(names.iter().any(|n| n == "run_checks"), "{names:?}");
        assert!(!system.contains("`bash`"), "{system}");
    }

    /// mise and one setup step, every command on the fake backend passing.
    /// The step leaves a lockfile and a changed tracked file in the tree, as
    /// a dependency fetch can.
    fn set_up_backend() -> crate::workspace::fake::FakeProvider {
        use crate::workspace::fake::Scripted;
        let mut provider = crate::workspace::fake::FakeProvider::default();
        provider.script.insert(
            "env MISE_SAFE=1 mise settings get safe".to_owned(),
            Scripted {
                output: "true\n".to_owned(),
                ..Scripted::default()
            },
        );
        provider.script.insert(
            "env MISE_SAFE=1 mise exec -- make deps".to_owned(),
            Scripted {
                writes: vec![
                    ("deps.lock".to_owned(), b"locked\n".to_vec()),
                    (
                        "src/a.rs".to_owned(),
                        b"fn main() {\n    let x = 1;\n}\n// deps\n".to_vec(),
                    ),
                ],
                ..Scripted::default()
            },
        );
        for command in [
            "env MISE_SAFE=1 mise install",
            "env MISE_SAFE=1 mise exec -- true",
        ] {
            provider
                .script
                .insert(command.to_owned(), Scripted::default());
        }
        provider
    }

    const SET_UP: &str = "[workspace]\ntoolchain = \"mise\"\nsetup = [[\"make\", \"deps\"]]\n";

    /// A loop on `provider` with `settings`, a check, and an executor that
    /// answers `executor`.
    async fn set_up_loop(
        name: &str,
        settings: &str,
        executor: Vec<Result<Completion, henk_llm::LlmError>>,
        provider: &crate::workspace::fake::FakeProvider,
    ) -> Looping {
        let mut looping = looping_with(
            name,
            settings,
            ScriptedClient::new("r", one_finding()),
            ScriptedClient::new("f", executor),
            Arc::new(provider.clone()),
        )
        .await;
        configure(
            &mut looping.0,
            &loop_config_and(settings, "check_commands = [[\"true\"]]\n"),
        );
        looping
    }

    #[tokio::test]
    async fn the_setup_stage_installs_the_toolchain_and_runs_its_steps_before_the_reviewer() {
        let provider = set_up_backend();
        let (f, _, _, remote) = set_up_loop("loop-setup", SET_UP, fixes_f1(), &provider).await;
        let run = RunId::parse("r-loop-setup").unwrap();
        run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();
        let ran = provider.ran.lock().unwrap().clone();
        assert_eq!(
            ran.get(..3).unwrap(),
            [
                "env MISE_SAFE=1 mise settings get safe",
                "env MISE_SAFE=1 mise install",
                "env MISE_SAFE=1 mise exec -- make deps"
            ],
            "mise first, in safe mode, then the operator's steps, before anything else"
        );
        assert!(ran.len() > 3, "the check ran: {ran:?}");
        assert!(
            ran.iter()
                .skip(3)
                .all(|c| c == "env MISE_SAFE=1 mise exec -- true"),
            "the checks run through mise: {ran:?}"
        );
        assert!(!ran.iter().any(|c| c.contains("trust")), "{ran:?}");
        let (files, content) =
            crate::git::tests::remote_change(remote.path(), f.head.as_str(), "src/a.rs").await;
        assert_eq!(files, ["src/a.rs"], "nothing setup left is pushed");
        assert_eq!(
            content, "fn main() {\n    let x = 3;\n}\n// deps\n",
            "the fix on top of the tree setup left"
        );
        assert!(
            execs(&f.app, &run)
                .await
                .iter()
                .any(|e| e.starts_with("exec env MISE_SAFE=1 mise install exit 0")),
            "every setup step is on the run's timeline"
        );
    }

    #[tokio::test]
    async fn what_setup_leaves_in_the_tree_is_never_pushed_by_the_loop() {
        let provider = set_up_backend();
        let (f, _, _, remote) = set_up_loop(
            "loop-setup-leftovers",
            SET_UP,
            vec![
                judge("f1", "rejected", "the caller never divides by x."),
                say("Settled."),
            ],
            &provider,
        )
        .await;
        let run = RunId::parse("r-loop-setup-leftovers").unwrap();
        let report = run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();
        let summary = report.summary.unwrap();
        assert!(summary.contains("pushed 0 commits"), "{summary}");
        let (after, _) = crate::git::tests::remote_feature(remote.path()).await;
        assert_eq!(after, f.head.as_str(), "nothing pushed");
    }

    /// The run a loop that never got its workspace ended with, after the
    /// models were never asked and nothing was pushed.
    async fn never_set_up(
        f: &Fixture,
        reviewer: &ScriptedClient,
        executor: &ScriptedClient,
        remote: &crate::git::ScratchDir,
        run: &RunId,
    ) -> String {
        let _ = run_review(&f.app, request(run), CancellationToken::new()).await;
        assert!(reviewer.requests().is_empty() && executor.requests().is_empty());
        let (after, _) = crate::git::tests::remote_feature(remote.path()).await;
        assert_eq!(after, f.head.as_str(), "nothing pushed");
        assert_no_stage_running(&f.app, run).await;
        let record = f.app.store.run(run).await.unwrap().unwrap();
        assert_eq!(record.status, RunStatus::Failed);
        record.error.unwrap_or_default()
    }

    #[tokio::test]
    async fn a_mise_without_safe_mode_is_refused_before_the_loop_reads_the_repository() {
        let mut provider = set_up_backend();
        // An older mise fails on the setting it does not know.
        provider.script.insert(
            "env MISE_SAFE=1 mise settings get safe".to_owned(),
            crate::workspace::fake::Scripted {
                code: 1,
                output: "mise ERROR Unknown setting: safe".to_owned(),
                ..crate::workspace::fake::Scripted::default()
            },
        );
        let (f, reviewer, executor, remote) =
            set_up_loop("loop-setup-unsafe", SET_UP, fixes_f1(), &provider).await;
        let run = RunId::parse("r-loop-setup-unsafe").unwrap();
        let error = never_set_up(&f, &reviewer, &executor, &remote, &run).await;
        assert!(error.contains("no safe mode"), "{error}");
        assert_eq!(
            *provider.ran.lock().unwrap(),
            ["env MISE_SAFE=1 mise settings get safe"],
            "mise never installed and the models never ran"
        );
    }

    #[tokio::test]
    async fn a_failing_setup_step_stops_the_loop_before_the_reviewer() {
        let mut provider = crate::workspace::fake::FakeProvider::default();
        provider.script.insert(
            "make deps".to_owned(),
            crate::workspace::fake::Scripted {
                code: 2,
                output: "no rule to make target 'deps'".to_owned(),
                ..crate::workspace::fake::Scripted::default()
            },
        );
        let (f, reviewer, executor, remote) = set_up_loop(
            "loop-setup-fails",
            "[workspace]\nsetup = [[\"make\", \"deps\"]]\n",
            fixes_f1(),
            &provider,
        )
        .await;
        let run = RunId::parse("r-loop-setup-fails").unwrap();
        let error = never_set_up(&f, &reviewer, &executor, &remote, &run).await;
        assert!(
            error.contains("setup step `make deps` exited with 2"),
            "{error}"
        );
        assert!(
            !error.contains("no rule to make target"),
            "the error names the step, not its output: {error}"
        );
        assert_eq!(*provider.ran.lock().unwrap(), ["make deps"]);
        assert!(provider.closed(), "the workspace is destroyed");
        for reply in f.writer.replies.lock().unwrap().iter() {
            assert!(
                !reply.body.contains("no rule to make target"),
                "{}",
                reply.body
            );
        }
    }

    #[tokio::test]
    async fn a_changeset_against_the_rules_stops_the_loop_before_the_push() {
        use crate::workspace::Exported;
        use henk_domain::workspace::{FileMode, RawChange};
        let raw = |path: &str, mode: FileMode| Exported {
            raw: RawChange {
                path: path.to_owned(),
                mode,
                deleted: false,
            },
            content: b"x\n".to_vec(),
        };
        let too_many: Vec<Exported> = (0..20)
            .map(|i| raw(&format!("gen/{i}.txt"), FileMode::Regular))
            .collect();
        for (case, inject, why) in [
            (
                "link",
                vec![raw("docs", FileMode::Symlink)],
                "symbolic link",
            ),
            (
                "module",
                vec![raw("vendor/lib", FileMode::Gitlink)],
                "submodule",
            ),
            (
                "git",
                vec![raw(".git/hooks/pre-push", FileMode::Regular)],
                ".git",
            ),
            (
                "escape",
                vec![raw("../outside", FileMode::Regular)],
                "leaves the repository",
            ),
            (
                "many",
                too_many,
                "21 files changed; a run may change at most 20",
            ),
        ] {
            let provider = crate::workspace::fake::FakeProvider {
                inject,
                ..crate::workspace::fake::FakeProvider::default()
            };
            let name = format!("loop-refused-{case}");
            let (f, _, _, remote) = looping(
                &name,
                ScriptedClient::new("r", one_finding()),
                ScriptedClient::new("f", fixes_f1()),
                Arc::new(provider.clone()),
            )
            .await;
            let run = RunId::parse(format!("r-{name}")).unwrap();
            let report = run_review(&f.app, request(&run), CancellationToken::new())
                .await
                .unwrap();
            let summary = report.summary.unwrap();
            assert!(summary.contains("cannot be pushed"), "{case}: {summary}");
            assert!(summary.contains(why), "{case}: {summary}");
            let record = f.app.store.run(&run).await.unwrap().unwrap();
            assert_eq!(record.loop_stop.as_deref(), Some("push_refused"), "{case}");
            let (after, _) = crate::git::tests::remote_feature(remote.path()).await;
            assert_eq!(after, f.head.as_str(), "{case}: nothing pushed");
            assert!(provider.closed(), "{case}: the workspace is destroyed");
            assert_no_stage_running(&f.app, &run).await;
        }
    }

    /// `text` as a TOML basic string.
    fn toml_string(text: &str) -> String {
        format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
    }

    #[tokio::test]
    async fn the_loop_pushes_from_a_clean_checkout_with_only_the_changeset() {
        let check = r#"echo stray > stray.txt; mkdir -p "$HOME/.cache" && echo c > "$HOME/.cache/x"; mkdir -p .git/hooks && echo evil > .git/hooks/pre-commit && chmod +x .git/hooks/pre-commit; echo checked"#;
        let (mut f, _, _, remote) = looping(
            "loop-clean",
            ScriptedClient::new("r", one_finding()),
            ScriptedClient::new("f", fixes_f1()),
            Arc::new(crate::workspace::host::HostProvider),
        )
        .await;
        let checks = format!(
            "check_commands = [[\"sh\", \"-c\", {}]]\n",
            toml_string(check)
        );
        configure(&mut f, &loop_config_and("", &checks));
        let run = RunId::parse("r-loop-clean").unwrap();
        run_review(&f.app, request(&run), CancellationToken::new())
            .await
            .unwrap();
        let (head, _) = crate::git::tests::remote_feature(remote.path()).await;
        assert_ne!(head, f.head.as_str(), "pushed");
        // What the check left in the tree is part of the changeset and its
        // limits; its HOME and a .git it made are not, and no hook it
        // planted ran when Henk committed.
        let (changed, stray) =
            crate::git::tests::remote_change(remote.path(), f.head.as_str(), "stray.txt").await;
        assert_eq!(changed, ["src/a.rs", "stray.txt"]);
        assert_eq!(stray, "stray\n");
        let execs = execs(&f.app, &run).await;
        assert_eq!(execs.len(), 1, "{execs:?}");
        assert!(execs[0].contains("exit 0"), "{execs:?}");
        assert!(execs[0].ends_with("\nchecked"), "{execs:?}");
        let push = crate::git::scratch_prefix("henk-loop-r-loop-clean-push-1");
        let left: Vec<_> = std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .filter(|name| name.starts_with(&push))
            .collect();
        assert!(left.is_empty(), "the push checkout is gone: {left:?}");
    }

    const GITLAB_TOKEN: &str = "glpat-never-shown";

    /// GitLab's REST API for merge request g/p!7, project 11, whose branch
    /// `feature` lives at `remote`. Only a request with the token in its
    /// header gets an answer.
    async fn gitlab_rest(remote: &std::path::Path, head: &CommitSha) -> wiremock::MockServer {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        for (at, body) in [
            (
                "/api/v4/projects/g%2Fp/merge_requests/7",
                serde_json::json!({"iid": 7, "state": "opened", "sha": head.as_str(), "source_branch": "feature",
                       "target_branch": "main", "source_project_id": 11, "target_project_id": 11}),
            ),
            (
                "/api/v4/projects/g%2Fp",
                serde_json::json!({"id": 11, "path_with_namespace": "g/p", "default_branch": "main",
                       "http_url_to_repo": remote.to_string_lossy()}),
            ),
            (
                "/api/v4/projects/11/repository/branches/feature",
                serde_json::json!({"name": "feature", "protected": false, "default": false}),
            ),
            (
                "/api/v4/user",
                serde_json::json!({"id": 42, "username": "meneerhenk"}),
            ),
            (
                "/api/v4/users/77",
                serde_json::json!({"id": 77, "username": "alice", "name": "Alice Display"}),
            ),
        ] {
            Mock::given(method("GET"))
                .and(path(at))
                .and(header("PRIVATE-TOKEN", GITLAB_TOKEN))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(&server)
                .await;
        }
        server
    }

    #[tokio::test]
    async fn a_whole_loop_works_on_gitlab_through_the_gitlab_writer() {
        use henk_platform::gitlab::{GitLabRest, GitLabWriter};

        let (mut f, reviewer, executor, remote) = looping(
            "loop-gitlab",
            ScriptedClient::new("r", one_finding()),
            ScriptedClient::new("f", fixes_f1()),
            Arc::new(crate::workspace::host::HostProvider),
        )
        .await;
        let config = loop_config_and("", PEOPLE).replace(
            "github_owners = [\"o\"]\n",
            "github_owners = [\"o\"]\ngitlab_projects = [\"g/p\"]\n",
        );
        configure(&mut f, &config);
        let rest = gitlab_rest(remote.path(), &f.head).await;
        let mcp = FakeServer::new(Vec::new(), echo_behaviour());
        f.app.test_push_writer = None;
        f.app.gitlab = Some(Arc::new(GitLabWriter::new(
            Arc::new(mcp.connect("gitlab-write").await),
            "meneerhenk",
        )));
        let refused = f
            .app
            .push_writer(&RepoRef::parse(Platform::GitLab, "g/p").unwrap())
            .err()
            .unwrap();
        assert!(
            refused.to_string().contains("needs the token in $"),
            "{refused}"
        );
        f.app.gitlab = Some(Arc::new(
            GitLabWriter::new(Arc::new(mcp.connect("gitlab-write").await), "meneerhenk").with_rest(
                GitLabRest::new(
                    &format!("{}/api/v4", rest.uri()),
                    secrecy::SecretString::from(GITLAB_TOKEN.to_owned()),
                )
                .unwrap(),
            ),
        ));
        assert!(
            f.app
                .push_writer(&RepoRef::parse(Platform::GitLab, "g/p").unwrap())
                .is_ok()
        );

        let run = RunId::parse("r-loop-gitlab").unwrap();
        let mut on_gitlab = request_by(&run, "discord:3");
        on_gitlab.target.repo = RepoRef::parse(Platform::GitLab, "g/p").unwrap();
        let report = run_review(&f.app, on_gitlab, CancellationToken::new())
            .await
            .unwrap();
        let summary = report.summary.unwrap();
        assert!(summary.contains("pushed 1 commit "), "{summary}");
        let record = f.app.store.run(&run).await.unwrap().unwrap();
        assert_eq!(record.platform, Platform::GitLab);
        assert_eq!(record.loop_stop.as_deref(), Some("converged"));
        let (head, log) = crate::git::tests::remote_feature(remote.path()).await;
        assert_ne!(
            head,
            f.head.as_str(),
            "one commit, pushed as a fast-forward"
        );
        assert!(
            log.starts_with("meneerhenk <42-meneerhenk@users.noreply.127.0.0.1>"),
            "{log}"
        );
        assert_eq!(
            pushed_trailers(remote.path()).await[2..],
            [
                "Co-authored-by: alice <77-alice@users.noreply.127.0.0.1>",
                "Signed-off-by: meneerhenk <42-meneerhenk@users.noreply.127.0.0.1>",
            ],
            "the requester by the username their gitlab_id has, never the display name"
        );
        assert!(
            !format!("{:?}{:?}", reviewer.requests(), executor.requests()).contains(GITLAB_TOKEN),
            "no model ever sees the token"
        );
        for request in rest.received_requests().await.unwrap() {
            assert!(
                !request.url.as_str().contains(GITLAB_TOKEN),
                "{}",
                request.url
            );
        }
    }
}
