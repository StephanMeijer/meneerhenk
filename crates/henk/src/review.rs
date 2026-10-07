//! The review orchestrator (§3).

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
use henk_store::{NewRun, RunStatus};
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

    let (info, commit) = preflight(app, &writer, &request).await?;

    app.store
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
        .await?;
    info!(run = %run, commit = %commit.short(), "review started");
    let _alive = KeepAlive::start(Arc::clone(&app.store), &app.live_runs, run.clone());

    if let Some((comment_id, is_review_comment)) = &request.acknowledge
        && let Err(error) = writer
            .acknowledge(&request.target, comment_id, *is_review_comment)
            .await
    {
        warn!(%error, "could not react to the request comment");
    }

    let handle = match writer.start_review(&request.target, &commit, &link).await {
        Ok(handle) => {
            if let Some(ReviewHandle(check_id)) = &handle
                && let Err(error) = app.store.set_check(&run, check_id).await
            {
                warn!(%error, "could not store the check id");
            }
            handle
        }
        Err(error) => {
            warn!(%error, "could not mark the review as started");
            None
        }
    };

    let review = ReviewRun {
        app,
        writer: &writer,
        target: &request.target,
        commit: &commit,
        run: &run,
        link: &link,
    };
    let result = review_body(review, &info.title, &info.base_ref, cancel).await;

    match result {
        Ok((outcome, summary)) => {
            let status = if outcome.completed() {
                RunStatus::Finished
            } else {
                RunStatus::Failed
            };
            if let Err(error) = writer
                .finish_review(&request.target, &commit, handle.as_ref(), &outcome, &link)
                .await
            {
                error!(%error, "could not finish the check");
            }
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

async fn preflight(
    app: &App,
    writer: &Arc<dyn PlatformWriter>,
    request: &ReviewRequest,
) -> anyhow::Result<(henk_platform::PullRequestInfo, CommitSha)> {
    let platform = request.target.platform();
    if !app.settings.allowlist.allows(&request.target.repo) {
        return Err(anyhow!("{} is not on the allowlist", request.target.repo));
    }
    if app.settings.lanes.is_empty() {
        return Err(anyhow!("no review lanes are configured"));
    }

    let info = writer
        .pull_request(&request.target)
        .await
        .context("reading the pull request")?;
    if info.state != PullRequestState::Open {
        return Err(anyhow!(
            "pull request #{} is not open",
            request.target.number
        ));
    }
    if info.draft && platform == Platform::GitHub && !app.settings.review.github_drafts {
        return Err(anyhow!(
            "pull request #{} is a draft; drafts are not reviewed",
            request.target.number
        ));
    }
    let commit = request.commit.clone().unwrap_or_else(|| info.head.clone());

    Ok((info, commit))
}

/// A newer commit arrived: no comment, a neutral check, and the run ends
/// `failed` with the reason (§3.3). Not Henk's failure, so nothing says it is.
async fn report_superseded(
    review: ReviewRun<'_>,
    handle: Option<&henk_platform::ReviewHandle>,
) -> anyhow::Result<()> {
    let ReviewRun {
        app,
        writer,
        target,
        commit,
        run,
        link,
    } = review;
    info!(run = %run, "review superseded by a newer commit");
    let outcome = ReviewOutcome::superseded(commit.clone());
    if let Err(finish_error) = writer
        .finish_review(target, commit, handle, &outcome, link)
        .await
    {
        error!(%finish_error, "could not finish the check of a superseded review");
    }
    app.store
        .finish_run(run, RunStatus::Failed, None, Some(&Superseded.to_string()))
        .await?;
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
    if let Err(post_error) = writer.post_comment(target, &body).await {
        error!(%post_error, "could not post that the review was cancelled");
    }
    let outcome = ReviewOutcome::cancelled(commit.clone());
    if let Err(finish_error) = writer
        .finish_review(target, commit, handle, &outcome, link)
        .await
    {
        error!(%finish_error, "could not finish the check of a cancelled review");
    }
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
/// slot (#69). It never started, so there is no check to close; its run is
/// still recorded and ends `cancelled`, so the run page the dashboard
/// links to exists and says who, and one comment says so.
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
    app.store
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
        .await?;
    info!(run = %run, by, "review cancelled from the dashboard before it started");
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
            if let Err(post_error) = writer.post_comment(&request.target, &body).await {
                error!(%post_error, "could not post that the review was cancelled");
            }
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

/// Henk was told to stop: the check completes as interrupted, the run
/// ends `failed`, and nothing is posted; the next review posts (§3.3).
async fn report_interrupted(
    review: ReviewRun<'_>,
    handle: Option<&henk_platform::ReviewHandle>,
) -> anyhow::Result<()> {
    let ReviewRun {
        app,
        writer,
        target,
        commit,
        run,
        link,
    } = review;
    warn!(run = %run, "review interrupted");
    let outcome = ReviewOutcome::interrupted(commit.clone());
    if let Err(finish_error) = writer
        .finish_review(target, commit, handle, &outcome, link)
        .await
    {
        error!(%finish_error, "could not finish the check of an interrupted review");
    }
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
    } = review;
    let message = format!("{error:#}");
    error!(error = %message, "review failed");
    let failed = ReviewOutcome {
        commit: commit.clone(),
        lanes: Vec::new(),
        open_findings: 0,
        nothing_to_review: false,
        stopped: None,
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
    if let Err(post_error) = writer.post_comment(target, &body).await {
        error!(%post_error, "could not post the failure comment");
    }
    if let Err(finish_error) = writer
        .finish_review(target, commit, handle, &failed, link)
        .await
    {
        error!(%finish_error, "could not finish the check after failure");
    }
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

    let diff = fetch_diff(review, base_ref).await?;
    // Every changed file left out by review.ignore: nothing for a lane to
    // read. The review still counts, folds and summarises (§3.2).
    let nothing_to_review = diff.is_empty();
    let results = if nothing_to_review {
        Vec::new()
    } else {
        run_lanes(review, registry, diff, title, base_ref, &cancel).await?
    };
    if cancel.is_cancelled() {
        if let Some(by) = review.app.cancels.cancelled_by(review.run) {
            return Err(CancelledBy(by).into());
        }
        // A shutdown cancels every review; superseding cancels only one.
        return Err(if review.app.shutdown.is_cancelled() {
            Interrupted.into()
        } else {
            Superseded.into()
        });
    }

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
    };

    fold_outdated(writer, target, &after).await;

    let summary_text = outcome.summary();
    let body = Marker {
        run: run.clone(),
        model: ModelId::parse("orchestrator").unwrap_or_else(|_| unreachable!("constant")),
        requested_by: None,
        kind: Some(MarkerKind::Summary),
        checked_by: None,
        withdrawn: None,
    }
    .attach(&format!("{summary_text}\n\nRun: {link}"));
    writer
        .post_comment(target, &body)
        .await
        .context("posting the summary")?;
    Ok((outcome, summary_text))
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
    if !book.is_empty() {
        let verdicts = match &fact_check {
            _ if cancel.is_cancelled() => None,
            Some(checker) => Some(
                checker
                    .check_all(&book, &crate::drafts::open_findings(&lanes.registry))
                    .await,
            ),
            None => Some(
                book.iter()
                    .map(|d| (d.id, henk_domain::draft::Verdict::NoCheck))
                    .collect(),
            ),
        };
        match verdicts {
            Some(verdicts) if !cancel.is_cancelled() => {
                crate::drafts::write_all(&writes, &book, &verdicts).await;
            }
            _ => crate::drafts::cancel_all(&writes, &book).await,
        }
    }
    workspaces.close_all().await;
    results.sort_by(|a, b| a.lane.as_str().cmp(b.lane.as_str()));
    Ok(results)
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
    let _ = app
        .store
        .event(
            run,
            "info",
            &format!(
                "diff: {} files, +{additions} -{deletions}; {} not reviewed (review.ignore)",
                diff.files().len(),
                diff.ignored().len()
            ),
        )
        .await;
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
        assert_eq!(lanes[0].status, LaneStatus::Dropped);
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
            LaneStatus::Dropped
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
            LaneStatus::Dropped
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
        assert_eq!(record.status, RunStatus::Failed);
        assert_eq!(
            record.error.as_deref(),
            Some("superseded by a review of a newer commit")
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
