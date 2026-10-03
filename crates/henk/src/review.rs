//! The review orchestrator (§3).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use henk_agent::{Agent, AgentConfig, StopCause, ToolSet, Verdict, mcp_tools, prompts};
use henk_domain::allowlist::Platform;
use henk_domain::finding::{Finding, FindingKey, FindingRegistry};
use henk_domain::marker::{Marker, MarkerKind, ModelId};
use henk_domain::review::{CommitSha, LaneOutcome, LaneResult, LaneSpec, ReviewOutcome};
use henk_domain::run::{RunId, RunKind};
use henk_domain::scope::{self, Scope};
use henk_llm::ChatMessage;
use henk_mcp::{McpSession, NameMap};
use henk_platform::{PlatformWriter, PullRequestState, ReviewTarget};
use henk_store::{LaneStatus, NewRun, RunStatus};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, instrument, warn};

use crate::app::App;
use crate::ids::new_run_id;
use crate::review_tools::{ImproveFinding, LaneContext, ListExistingFindings, PostFinding};

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
    let run = new_run_id();
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

    let result = review_body(
        app,
        &writer,
        &request.target,
        &commit,
        &info.title,
        &info.base_ref,
        &run,
        &link,
        cancel,
    )
    .await;

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
            report_failure(
                app,
                &writer,
                &request.target,
                &commit,
                handle.as_ref(),
                &run,
                &link,
                &error,
            )
            .await?;
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

#[allow(clippy::too_many_arguments)]
async fn report_failure(
    app: &App,
    writer: &Arc<dyn PlatformWriter>,
    target: &ReviewTarget,
    commit: &CommitSha,
    handle: Option<&henk_platform::ReviewHandle>,
    run: &RunId,
    link: &str,
    error: &anyhow::Error,
) -> anyhow::Result<()> {
    let message = format!("{error:#}");
    error!(error = %message, "review failed");
    let failed = ReviewOutcome {
        commit: commit.clone(),
        lanes: Vec::new(),
        open_findings: 0,
    };
    let body = Marker {
        run: run.clone(),
        model: ModelId::parse("none").unwrap_or_else(|_| unreachable!("constant")),
        requested_by: None,
        kind: Some(MarkerKind::Failure),
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

#[allow(clippy::too_many_arguments)]
async fn review_body(
    app: &App,
    writer: &Arc<dyn PlatformWriter>,
    target: &ReviewTarget,
    commit: &CommitSha,
    title: &str,
    base_ref: &str,
    run: &RunId,
    link: &str,
    cancel: CancellationToken,
) -> anyhow::Result<(ReviewOutcome, String)> {
    let platform = target.platform();

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

    // One read-only MCP session shared by the lanes of this review.
    let alias = app.read_mcp_alias(platform)?;
    let session: Arc<dyn McpSession> = Arc::new(app.connect_mcp(alias).await?);
    let scope = Scope::Review {
        repo: target.repo.clone(),
        number: target.number,
        commit: commit.clone(),
    };

    let mut set = spawn_lanes(
        app, &session, &scope, &registry, writer, target, commit, title, base_ref, run, &cancel,
    )
    .await?;

    let mut lanes = Vec::new();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(result) => lanes.push(result),
            Err(error) => {
                error!(%error, "a lane task panicked");
                lanes.push(LaneResult {
                    lane: henk_domain::review::LaneName::new("unknown"),
                    outcome: LaneOutcome::Dropped,
                });
            }
        }
    }
    lanes.sort_by(|a, b| a.lane.as_str().cmp(b.lane.as_str()));

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
        lanes,
        open_findings,
    };

    fold_outdated(writer, target, &after).await;

    let summary_text = outcome.summary();
    let body = Marker {
        run: run.clone(),
        model: ModelId::parse("orchestrator").unwrap_or_else(|_| unreachable!("constant")),
        requested_by: None,
        kind: Some(MarkerKind::Summary),
    }
    .attach(&format!("{summary_text}\n\nRun: {link}"));
    writer
        .post_comment(target, &body)
        .await
        .context("posting the summary")?;
    Ok((outcome, summary_text))
}

#[allow(clippy::too_many_arguments)]
async fn spawn_lanes(
    app: &App,
    session: &Arc<dyn McpSession>,
    scope: &Scope,
    registry: &Arc<Mutex<FindingRegistry>>,
    writer: &Arc<dyn PlatformWriter>,
    target: &ReviewTarget,
    commit: &CommitSha,
    title: &str,
    base_ref: &str,
    run: &RunId,
    cancel: &CancellationToken,
) -> anyhow::Result<JoinSet<LaneResult>> {
    let mut set = JoinSet::new();
    for lane in &app.settings.lanes {
        let agent = build_lane(
            app,
            lane,
            session,
            scope,
            Arc::clone(registry),
            Arc::clone(writer),
            target,
            commit,
            title,
            base_ref,
            run,
        )
        .await?;
        let lane_name = lane.name.clone();
        let model_name = agent.model_name().to_owned();
        app.store.start_lane(run, lane_name.as_str(), &model_name)?;
        let store = Arc::clone(&app.store);
        let run_id = run.clone();
        let cancel = cancel.clone();
        let number = target.number;
        set.spawn(async move {
            let opening = ChatMessage::user(format!(
                "Review pull request #{number} at commit {}. Read the diff first.",
                commit_short(&run_id, &model_name)
            ));
            let outcome = agent.run(vec![opening], cancel).await;
            let (status, lane_outcome, error) = match &outcome.stop {
                StopCause::EndTurn | StopCause::MaxTurns => {
                    (LaneStatus::Finished, LaneOutcome::Finished, None)
                }
                StopCause::Timeout => (
                    LaneStatus::Dropped,
                    LaneOutcome::Dropped,
                    Some("timed out".to_owned()),
                ),
                StopCause::Cancelled => (
                    LaneStatus::Dropped,
                    LaneOutcome::Dropped,
                    Some("cancelled".to_owned()),
                ),
                StopCause::ModelError(e) => (
                    LaneStatus::Dropped,
                    LaneOutcome::Dropped,
                    Some(e.to_string()),
                ),
            };
            let _ = store.finish_lane(
                &run_id,
                lane_name.as_str(),
                status,
                u64::from(outcome.turns),
                outcome.usage.input_tokens,
                outcome.usage.output_tokens,
                error.as_deref(),
            );
            let _ = store.event(
                &run_id,
                if error.is_some() { "warn" } else { "info" },
                &format!(
                    "lane {lane_name}: {:?}; last words: {}",
                    outcome.stop,
                    outcome.final_text.chars().take(200).collect::<String>()
                ),
            );
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

fn commit_short(_run: &RunId, _model: &str) -> String {
    // Placeholder kept trivially simple; the commit is in the system prompt.
    "the reviewed commit".to_owned()
}

#[allow(clippy::too_many_arguments)]
async fn build_lane(
    app: &App,
    lane: &LaneSpec,
    session: &Arc<dyn McpSession>,
    scope: &Scope,
    registry: Arc<Mutex<FindingRegistry>>,
    writer: Arc<dyn PlatformWriter>,
    target: &ReviewTarget,
    commit: &CommitSha,
    title: &str,
    base_ref: &str,
    run: &RunId,
) -> anyhow::Result<Agent> {
    let platform = target.platform();
    let model = app.model(lane.model.as_str())?;

    let guard_scope = scope.clone();
    let guard: henk_agent::Guard = Arc::new(move |tool: &str, args: &serde_json::Value| {
        match scope::guard(platform, tool, args, &guard_scope) {
            scope::Verdict::Allow(rewritten) => Verdict::Allow(rewritten),
            scope::Verdict::Deny(reason) => Verdict::Deny(reason),
        }
    });
    let mut names = NameMap::new();
    let tools = mcp_tools(
        Arc::clone(session),
        &mut names,
        |info| scope::is_exposed(platform, &info.name),
        guard,
    )
    .await
    .context("listing MCP tools")?;
    if tools.is_empty() {
        return Err(anyhow!(
            "the MCP server exposes none of the tools a review needs"
        ));
    }
    let mut set = ToolSet::new();
    for tool in tools {
        set.add(tool);
    }
    let context = Arc::new(LaneContext {
        run: run.clone(),
        lane: lane.name.clone(),
        model: ModelId::parse(model.model().to_owned()).unwrap_or_else(|_| lane.model.clone()),
        target: target.clone(),
        commit: commit.clone(),
        registry,
        writer,
        store: Arc::clone(&app.store),
    });
    set.add(ListExistingFindings(Arc::clone(&context)));
    set.add(PostFinding(Arc::clone(&context)));
    set.add(ImproveFinding(context));

    let system = format!(
        "{}\n\n{}",
        prompts::PERSONA,
        prompts::render(
            prompts::REVIEW_LANE,
            &[
                ("number", &target.number.to_string()),
                ("repo", &target.repo.path()),
                ("commit", commit.as_str()),
                ("base", base_ref),
                ("title", title),
            ],
        )
    );
    let config = AgentConfig {
        max_turns: app.settings.review.lane_max_turns,
        timeout: Duration::from_secs(app.settings.review.lane_timeout_secs),
        ..AgentConfig::default()
    };
    Ok(Agent::new(model, set, system, config))
}
