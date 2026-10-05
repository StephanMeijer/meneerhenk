//! The review orchestrator (§3).

use std::collections::BTreeMap;
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
use henk_platform::{PlatformWriter, PullRequestState, ReviewTarget};
use henk_session::{SessionSpec, model_id, platform_tools, run_session};
use henk_store::{NewRun, RunStatus};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, instrument, warn};

use crate::app::App;
use crate::fact_check::{FactCheck, SessionFactCheck};
use crate::ids::new_run_id;
use crate::review_tools::{
    DiffFiles, GetFileDiff, ImproveFinding, LaneContext, ListChangedFiles, ListExistingFindings,
    PostFinding, ReadFile, WithdrawFinding, lane_continuation,
};

/// The review was cancelled because a newer commit arrived.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("superseded by a review of a newer commit")]
pub struct Superseded;

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
    diff: Arc<ReviewDiff>,
    excluded: Vec<&'static str>,
    fact_check: Option<Arc<dyn FactCheck>>,
    title: String,
    base_ref: String,
    cancel: CancellationToken,
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

    app.store.create_run(&NewRun {
        id: run.clone(),
        kind: RunKind::Review,
        platform,
        repo: request.target.repo.path(),
        target: request.target.number,
        commit: Some(commit.as_str().to_owned()),
        requester: request.requester.clone(),
        trigger: request.trigger.clone(),
        link: link.clone(),
    })?;
    info!(run = %run, commit = %commit.short(), "review started");

    if let Some((comment_id, is_review_comment)) = &request.acknowledge
        && let Err(error) = writer
            .acknowledge(&request.target, comment_id, *is_review_comment)
            .await
    {
        warn!(%error, "could not react to the request comment");
    }

    let handle = match writer.start_review(&request.target, &commit, &link).await {
        Ok(handle) => handle,
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
            app.store.finish_run(&run, status, Some(&summary), None)?;
            info!(run = %run, open = outcome.open_findings, "review ended");
            Ok(ReviewReport {
                run,
                outcome: Some(outcome),
                summary: Some(summary),
            })
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
    };
    let body = Marker {
        run: run.clone(),
        model: ModelId::parse("none").unwrap_or_else(|_| unreachable!("constant")),
        requested_by: None,
        kind: Some(MarkerKind::Failure),
        checked_by: None,
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
        .finish_run(run, RunStatus::Failed, None, Some(&message))?;
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
        return Err(Superseded.into());
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
    };

    fold_outdated(writer, target, &after).await;

    let summary_text = outcome.summary();
    let body = Marker {
        run: run.clone(),
        model: ModelId::parse("orchestrator").unwrap_or_else(|_| unreachable!("constant")),
        requested_by: None,
        kind: Some(MarkerKind::Summary),
        checked_by: None,
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
        target,
        commit,
        ..
    } = review;
    let platform = target.platform();

    // One read-only MCP session shared by the lanes of this review.
    let alias = app.read_mcp_alias(platform)?;
    let session: Arc<dyn McpSession> = Arc::new(app.connect_mcp(alias).await?);
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
        diff,
        excluded,
        fact_check: None,
        title: title.to_owned(),
        base_ref: base_ref.to_owned(),
        cancel: cancel.clone(),
    };
    lanes.fact_check = build_fact_check(review, &lanes).await?;

    let mut set = spawn_lanes(review, &lanes).await?;

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
            );
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
    let _ = app.store.event(
        run,
        "info",
        &format!(
            "diff: {} files, +{additions} -{deletions}; {} not reviewed (review.ignore)",
            diff.files().len(),
            diff.ignored().len()
        ),
    );
    Ok(Arc::new(diff))
}

async fn spawn_lanes(
    review: ReviewRun<'_>,
    lanes: &LaneInputs,
) -> anyhow::Result<JoinSet<LaneResult>> {
    let ReviewRun { app, run, .. } = review;
    let mut set = JoinSet::new();
    for lane in &app.settings.lanes {
        let spec = build_lane(review, lanes, lane).await?;
        let lane_name = lane.name.clone();
        let store = Arc::clone(&app.store);
        let run_id = run.clone();
        let cancel = lanes.cancel.clone();
        let context = Arc::clone(&spec.context);
        set.spawn(async move {
            let outcome = run_session(&store, &run_id, spec.session, cancel).await;
            let unopened = context.unopened_files();
            if !unopened.is_empty() {
                let shown: Vec<&str> = unopened.iter().take(20).map(String::as_str).collect();
                let _ = store.event(
                    &run_id,
                    "warn",
                    &format!(
                        "{lane_name}: never asked the diff of {} changed file(s): {}",
                        unopened.len(),
                        shown.join(", ")
                    ),
                );
            }
            // The lane row (henk-session) says finished for a time limit;
            // the summary distinguishes it as stopped, from `stop`.
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

async fn build_lane(
    review: ReviewRun<'_>,
    lanes: &LaneInputs,
    lane: &LaneSpec,
) -> anyhow::Result<Lane> {
    let ReviewRun {
        app,
        writer,
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
    // line range); every other read tool is exposed as it is.
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
        target: target.clone(),
        commit: commit.clone(),
        registry: Arc::clone(&lanes.registry),
        writer: Arc::clone(writer),
        store: Arc::clone(&app.store),
        files: Arc::new(DiffFiles::new(Arc::clone(&lanes.diff))),
        fact_check: lanes.fact_check.clone(),
        rejections: Mutex::new(BTreeMap::new()),
    });
    set.add(ListChangedFiles(Arc::clone(&context.files)));
    set.add(GetFileDiff(Arc::clone(&context.files)));
    if let Some(inner) = file_reader {
        set.add(ReadFile { inner });
    }
    set.add(ListExistingFindings(Arc::clone(&context)));
    set.add(PostFinding(Arc::clone(&context)));
    set.add(ImproveFinding(Arc::clone(&context)));
    set.add(WithdrawFinding(Arc::clone(&context)));

    let system = format!(
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
    let limits = AgentConfig {
        max_turns: app.settings.review.lane_max_turns,
        timeout: Duration::from_secs(app.settings.review.lane_timeout_secs),
        max_conversation_chars: app.settings.review.max_conversation_chars,
        keep_recent_turns: app.settings.review.keep_recent_turns,
        ..AgentConfig::default()
    };
    let opening = ChatMessage::user(format!(
        "Review {} {} at commit {}. Read the diff first.",
        kind_name(platform),
        target_ref(platform, target.number),
        commit.short()
    ));
    Ok(Lane {
        session: SessionSpec {
            name: lane.name.as_str().to_owned(),
            model,
            system,
            opening: vec![opening],
            tools: set,
            limits,
            continuation: Some(lane_continuation(Arc::clone(&context))),
        },
        context,
    })
}

/// The fact-check every lane's writes pass, when one is configured (§3.2).
/// It reads the same diff and the same guarded file read as the lanes.
async fn build_fact_check(
    review: ReviewRun<'_>,
    lanes: &LaneInputs,
) -> anyhow::Result<Option<Arc<dyn FactCheck>>> {
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
    let system = prompts::render(
        prompts::FACT_CHECK,
        &[
            ("kind", kind_name(platform)),
            ("ref", &target_ref(platform, target.number)),
            ("repo", &target.repo.path()),
            ("commit", commit.as_str()),
        ],
    );
    Ok(Some(Arc::new(SessionFactCheck {
        store: Arc::clone(&app.store),
        run: run.clone(),
        models,
        diff: Arc::clone(diff),
        file_reader,
        system,
        limits: AgentConfig {
            max_turns: config.max_turns,
            timeout: Duration::from_secs(config.timeout_secs),
            max_conversation_chars: app.settings.review.max_conversation_chars,
            keep_recent_turns: app.settings.review.keep_recent_turns,
            ..AgentConfig::default()
        },
        cancel: cancel.clone(),
        sequence: std::sync::atomic::AtomicU32::new(0),
    })))
}

/// A lane ready to run: its session and the context its tools share, kept
/// so the orchestrator can read what the lane did.
struct Lane {
    session: SessionSpec,
    context: Arc<LaneContext>,
}
