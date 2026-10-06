//! The planning orchestrator (§4).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use henk_agent::{AgentConfig, StopCause, ToolSet, prompts};
use henk_domain::marker::{Marker, MarkerKind, ModelId};
use henk_domain::plan::{ChangeBudget, SessionEntry, extract_plan, with_plan_section};
use henk_domain::run::{RunId, RunKind};
use henk_domain::scope::Scope;
use henk_llm::ChatMessage;
use henk_mcp::McpSession;
use henk_platform::{IssueTarget, IssueUpdate};
use henk_session::{SessionSpec, model_id, platform_tools, run_session};
use henk_store::{NewRun, RunStatus};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, instrument, warn};

use crate::app::App;
use crate::ids::new_run_id;
use crate::liveness::KeepAlive;
use crate::plan_tools::{
    AddLabels, AskQuestions, CreateSubIssue, LinkIssue, PlanContext, PlanState, SetDescription,
    SetFields, SetIssueType, SetTitle, WritePlan,
};
use crate::review::Interrupted;
use crate::web_fetch::WebFetch;

/// A request to plan one issue.
#[derive(Debug, Clone)]
pub struct PlanRequest {
    /// The issue.
    pub target: IssueTarget,
    /// A note from the requester, if any.
    pub note: Option<String>,
    /// What started it.
    pub trigger: String,
    /// The run id to use, when the caller already announced one.
    pub run: Option<RunId>,
}

/// How planning ended.
#[derive(Debug)]
pub struct PlanReport {
    /// The run.
    pub run: RunId,
    /// Whether a plan is in the issue now.
    pub planned: bool,
    /// What changed, in words.
    pub changes: Vec<String>,
}

/// Plans one issue end to end. Every plan ends (§4): either the plan is in
/// the issue, or a comment says planning failed, with the run link.
#[instrument(skip_all, fields(repo = %request.target.repo.path(), issue = request.target.number))]
pub async fn run_plan(
    app: &App,
    request: PlanRequest,
    cancel: CancellationToken,
) -> anyhow::Result<PlanReport> {
    let planning = app
        .settings
        .planning
        .as_ref()
        .ok_or_else(|| anyhow!("planning is not configured"))?;
    let platform = request.target.repo.platform();
    let writer = app.issue_writer(platform)?;
    if !app.settings.allowlist.allows(&request.target.repo) {
        return Err(anyhow!("{} is not on the allowlist", request.target.repo));
    }

    let issue = writer
        .issue(&request.target)
        .await
        .context("reading the issue")?;
    if issue.is_pull_request {
        return Err(anyhow!(
            "#{} is a pull request; I plan issues, not pull requests",
            request.target.number
        ));
    }
    if !issue.open {
        return Err(anyhow!(
            "#{} is closed; I plan open issues",
            request.target.number
        ));
    }

    let run = request.run.clone().unwrap_or_else(new_run_id);
    let link = app.settings.run_link(&run);
    let requester = planning.requester_id;
    app.store
        .create_run(&NewRun {
            id: run.clone(),
            kind: RunKind::Plan,
            platform,
            repo: request.target.repo.path(),
            target: request.target.number,
            commit: None,
            requester: Some(requester.to_string()),
            trigger: request.trigger.clone(),
            link: link.clone(),
        })
        .await?;
    info!(run = %run, "planning started");
    let _alive = KeepAlive::start(Arc::clone(&app.store), &app.live_runs, run.clone());

    let model = app.model(&planning.model)?;
    let context = Arc::new(PlanContext {
        run: run.clone(),
        model: model_id(model.as_ref()),
        requester: Some(requester.get()),
        target: request.target.clone(),
        writer: Arc::clone(&writer),
        state: Mutex::new(PlanState {
            budget: ChangeBudget::new(planning.change_budget),
            changes: Vec::new(),
            sub_issues: Vec::new(),
            plan: None,
            asked: false,
        }),
        sub_issue_cap: planning.sub_issue_cap,
    });

    let result = plan_body(
        app,
        &request,
        &issue.body,
        &issue.title,
        Arc::clone(&context),
        Arc::clone(&model),
        cancel,
    )
    .await;

    if result.is_err() && app.shutdown.is_cancelled() {
        return end_interrupted(app, &run).await;
    }

    let (plan, changes) = match context.state.lock() {
        Ok(state) => (state.plan.clone(), state.changes.clone()),
        Err(_) => (None, Vec::new()),
    };
    let when = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();

    finish_plan(
        app,
        &writer,
        &request,
        &run,
        &link,
        &model,
        requester.to_string(),
        result,
        plan,
        changes,
        when,
        &context.model,
    )
    .await
}

#[expect(
    clippy::too_many_arguments,
    reason = "everything the plan session produced, recorded in one place"
)]
async fn finish_plan(
    app: &App,
    writer: &Arc<dyn henk_platform::IssueWriter>,
    request: &PlanRequest,
    run: &RunId,
    link: &str,
    model: &Arc<dyn henk_llm::ModelClient>,
    requester: String,
    result: anyhow::Result<()>,
    plan: Option<String>,
    changes: Vec<String>,
    when: String,
    model_id: &ModelId,
) -> anyhow::Result<PlanReport> {
    match (result, plan) {
        (Ok(()), Some(_)) => {
            // Append the session entry under the plan (§4).
            let current = writer
                .issue(&request.target)
                .await
                .map(|i| i.body)
                .unwrap_or_default();
            if let Some(mut section) = extract_plan(&current) {
                section.sessions.push(
                    SessionEntry {
                        when,
                        model: model.model().to_owned(),
                        requester: Some(requester),
                        changes: changes.clone(),
                        run_link: link.to_owned(),
                    }
                    .render(),
                );
                let body = with_plan_section(&current, &section);
                if let Err(error) = writer
                    .update_issue(
                        &request.target,
                        IssueUpdate {
                            body: Some(body),
                            ..Default::default()
                        },
                    )
                    .await
                {
                    warn!(%error, "could not append the session entry");
                }
            }
            app.store
                .finish_run(run, RunStatus::Finished, Some("plan written"), None)
                .await?;
            info!(run = %run, changes = changes.len(), "planning ended");
            Ok(PlanReport {
                run: run.clone(),
                planned: true,
                changes,
            })
        }
        (outcome, _) => {
            let reason = match outcome {
                Ok(()) => "the model ended without writing a plan".to_owned(),
                Err(error) => format!("{error:#}"),
            };
            error!(reason = %reason, "planning failed");
            let body = Marker {
                run: run.clone(),
                model: model_id.clone(),
                requested_by: None,
                kind: Some(MarkerKind::Failure),
                checked_by: None,
                withdrawn: None,
            }
            .attach(&format!(
                "Planning failed. That is my failure, not the issue's.\n\nRun: {link}"
            ));
            if let Err(post_error) = writer.comment(&request.target, &body).await {
                error!(%post_error, "could not post the failure comment");
            }
            app.store
                .finish_run(run, RunStatus::Failed, None, Some(&reason))
                .await?;
            Err(anyhow!("planning failed: {reason}"))
        }
    }
}

/// Stopped by Ctrl-C or a shutdown: not Henk's failure, so nothing is
/// posted; the run ends with the reason (#7).
async fn end_interrupted(app: &App, run: &RunId) -> anyhow::Result<PlanReport> {
    warn!(run = %run, "planning interrupted");
    app.store
        .finish_run(run, RunStatus::Failed, None, Some(&Interrupted.to_string()))
        .await?;
    Err(Interrupted.into())
}

async fn planner_tools(
    session: Arc<dyn McpSession>,
    platform: henk_domain::allowlist::Platform,
    scope: &Scope,
    context: Arc<PlanContext>,
) -> anyhow::Result<ToolSet> {
    let tools = platform_tools(session, platform, scope.clone(), &[])
        .await
        .context("listing MCP tools")?;
    let mut set = ToolSet::new();
    for tool in tools {
        set.add(tool);
    }
    set.add(WebFetch::new()?);
    set.add(SetTitle(Arc::clone(&context)));
    set.add(SetDescription(Arc::clone(&context)));
    set.add(AddLabels(Arc::clone(&context)));
    set.add(SetIssueType(Arc::clone(&context)));
    set.add(SetFields(Arc::clone(&context)));
    set.add(LinkIssue(Arc::clone(&context)));
    set.add(CreateSubIssue(Arc::clone(&context)));
    set.add(AskQuestions(Arc::clone(&context)));
    set.add(WritePlan(Arc::clone(&context)));

    Ok(set)
}

async fn plan_body(
    app: &App,
    request: &PlanRequest,
    body: &str,
    title: &str,
    context: Arc<PlanContext>,
    model: Arc<dyn henk_llm::ModelClient>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let planning = app
        .settings
        .planning
        .as_ref()
        .ok_or_else(|| anyhow!("planning is not configured"))?;
    let platform = request.target.repo.platform();
    let session = app.read_session(platform).await?;
    let scope = Scope::Plan {
        repo: request.target.repo.clone(),
        issue: request.target.number,
    };

    let set = planner_tools(Arc::clone(&session), platform, &scope, Arc::clone(&context)).await?;

    let previous = extract_plan(body)
        .map(|s| s.plan)
        .filter(|p| !p.trim().is_empty());
    let previous_text = match &previous {
        Some(plan) => format!(
            "A previous plan exists. Work out what was done since it was written (commits, merged pull requests, closed sub-issues) and open the new plan with a section \"Done since the previous plan\". Use the answers people gave to earlier questions. The previous plan:\n\n{plan}"
        ),
        None => "There is no previous plan.".to_owned(),
    };
    let note = request
        .note
        .as_deref()
        .map_or(String::new(), |n| format!("Their note: {n}"));
    let reference = format!("#{}", request.target.number);
    let system = format!(
        "{}\n\n{}",
        prompts::PERSONA,
        prompts::render(
            prompts::PLANNER,
            &[
                ("ref", &reference),
                ("repo", &request.target.repo.path()),
                ("note", &note),
                ("sub_issue_cap", &planning.sub_issue_cap.to_string()),
                ("change_budget", &planning.change_budget.to_string()),
                ("previous", &previous_text),
            ],
        )
    );
    let limits = AgentConfig {
        max_turns: planning.max_turns,
        timeout: Duration::from_secs(planning.timeout_secs),
        ..AgentConfig::default()
    };
    let opening = ChatMessage::user(format!(
        "Plan issue {reference}: {title}\n\nCurrent description (without any earlier plan section):\n\n{}",
        henk_domain::plan::body_without_plan(body)
    ));
    let spec = SessionSpec {
        name: "planner".to_owned(),
        model,
        system,
        opening: vec![opening],
        tools: set,
        limits,
        continuation: None,
        turn_warning: None,
    };
    let outcome = run_session(app.store.as_ref(), &context.run, spec, cancel).await;
    session_result(outcome.stop, planning.timeout_secs)
}

/// Whether the planner's session ended in a way a plan can come from. A
/// refusal is a failed plan, said on the issue, not a quiet success (#40).
fn session_result(stop: StopCause, timeout_secs: u64) -> anyhow::Result<()> {
    match stop {
        StopCause::EndTurn | StopCause::MaxTurns => Ok(()),
        StopCause::Timeout => Err(anyhow!("the time limit of {timeout_secs}s was reached")),
        StopCause::Cancelled => Err(anyhow!("cancelled")),
        StopCause::ModelError(error) => Err(anyhow!("model error: {error}")),
        StopCause::Refused(why) => Err(anyhow!("the model declined to plan ({why})")),
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing
    )]

    use std::collections::BTreeMap;

    use henk_domain::allowlist::{Platform, RepoRef};
    use henk_domain::plan::PlanSection;
    use henk_llm::testing::ScriptedClient;
    use henk_llm::{
        Block, ChatMessage, Completion, ModelClient, Role, StopReason, ToolArguments, ToolCall,
        Usage,
    };
    use henk_mcp::testing::{FakeServer, echo_behaviour};
    use henk_platform::IssueWriter;
    use serde_json::json;

    use super::*;
    use crate::config::Config;
    use crate::plan_tools::tests::FakeIssueWriter;

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
[planning]
model = "m"
requester_id = 3
"#;

    fn done() -> Completion {
        Completion {
            message: ChatMessage::assistant("Plan written."),
            stop: StopReason::EndTurn,
            usage: Usage::default(),
        }
    }

    fn write_plan(markdown: &str) -> Completion {
        Completion {
            message: ChatMessage {
                role: Role::Assistant,
                blocks: vec![Block::ToolCall(ToolCall {
                    id: "call-1".to_owned(),
                    name: "write_plan".to_owned(),
                    arguments: ToolArguments::Parsed(json!({"markdown": markdown})),
                })],
            },
            stop: StopReason::ToolUse,
            usage: Usage::default(),
        }
    }

    struct Fixture {
        app: App,
        tracker: Arc<FakeIssueWriter>,
        model: Arc<ScriptedClient>,
    }

    async fn fixture(tracker: FakeIssueWriter, script: Vec<Completion>) -> Fixture {
        let settings = Config::parse(CONFIG)
            .and_then(Config::into_settings)
            .unwrap_or_else(|e| panic!("{e}"));
        let server = FakeServer::new(
            vec![FakeServer::tool("issue_read", "", &["owner", "repo"])],
            echo_behaviour(),
        );
        let session: Arc<dyn McpSession> = Arc::new(server.connect("github").await);
        let model = Arc::new(ScriptedClient::new("scripted", script.into_iter().map(Ok)));
        let mut models: BTreeMap<String, Arc<dyn henk_llm::ModelClient>> = BTreeMap::new();
        models.insert("m".to_owned(), Arc::clone(&model) as Arc<dyn ModelClient>);
        let tracker = Arc::new(tracker);
        Fixture {
            app: App {
                settings,
                store: Arc::new(henk_store::SqliteStore::in_memory().unwrap()),
                models,
                github: None,
                gitlab: None,
                shutdown: CancellationToken::new(),
                live_runs: crate::liveness::LiveRuns::default(),
                workspace_provider: std::sync::Arc::new(crate::workspace::host::HostProvider),
                test_writer: None,
                test_session: Some(session),
                test_address_writer: None,
                test_issue_writer: Some(Arc::clone(&tracker) as Arc<dyn IssueWriter>),
            },
            tracker,
            model,
        }
    }

    fn request(run: &str) -> PlanRequest {
        PlanRequest {
            target: IssueTarget {
                repo: RepoRef::parse(Platform::GitHub, "o/r").unwrap(),
                number: 9,
            },
            note: None,
            trigger: "test".to_owned(),
            run: Some(RunId::parse(run).unwrap()),
        }
    }

    #[tokio::test]
    async fn a_first_plan_is_written_with_its_session_log() {
        let f = fixture(
            FakeIssueWriter::new(Platform::GitHub, None, "Export runs as CSV."),
            vec![write_plan("## Goal\n\nExport runs."), done()],
        )
        .await;
        let report = run_plan(&f.app, request("r-plan-1"), CancellationToken::new())
            .await
            .unwrap();
        assert!(report.planned);

        let body = f.tracker.issue.lock().unwrap().body.clone();
        assert!(body.starts_with("Export runs as CSV."), "{body}");
        let section = extract_plan(&body).unwrap();
        assert_eq!(section.plan, "## Goal\n\nExport runs.");
        assert_eq!(section.sessions.len(), 1, "{body}");
        assert!(
            section.sessions[0].contains("/runs/r-plan-1"),
            "{}",
            section.sessions[0]
        );
        assert!(f.tracker.comments.lock().unwrap().is_empty());
        let record = f
            .app
            .store
            .run(&RunId::parse("r-plan-1").unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.status, RunStatus::Finished);

        let system = f.model.requests()[0].system.clone().unwrap();
        assert!(system.contains("There is no previous plan."), "{system}");
    }

    #[tokio::test]
    async fn a_new_plan_replaces_the_old_one_which_it_starts_from() {
        let old = PlanSection {
            plan: "## Goal\n\nThe old plan.".to_owned(),
            sessions: vec!["- earlier session".to_owned()],
        };
        let body = with_plan_section("Export runs as CSV.", &old);
        let f = fixture(
            FakeIssueWriter::new(Platform::GitHub, None, &body),
            vec![
                write_plan(
                    "## Done since the previous plan\n\nNothing.\n\n## Goal\n\nThe new plan.",
                ),
                done(),
            ],
        )
        .await;
        run_plan(&f.app, request("r-plan-2"), CancellationToken::new())
            .await
            .unwrap();

        let body = f.tracker.issue.lock().unwrap().body.clone();
        let section = extract_plan(&body).unwrap();
        assert!(section.plan.ends_with("The new plan."), "{body}");
        assert!(
            !body.contains("The old plan."),
            "replaced, not appended: {body}"
        );
        assert_eq!(section.sessions.len(), 2, "the log keeps earlier sessions");
        let system = f.model.requests()[0].system.clone().unwrap();
        assert!(system.contains("Done since the previous plan"), "{system}");
        assert!(system.contains("The old plan."), "{system}");
    }

    #[tokio::test]
    async fn a_plan_that_never_gets_written_fails_on_the_issue_with_its_run_link() {
        let f = fixture(
            FakeIssueWriter::new(Platform::GitHub, None, "Export runs."),
            vec![done()],
        )
        .await;
        let error = run_plan(&f.app, request("r-plan-3"), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("without writing a plan"),
            "{error}"
        );
        let comments = f.tracker.comments.lock().unwrap().clone();
        assert_eq!(comments.len(), 1);
        assert!(
            comments[0].starts_with("Planning failed."),
            "{}",
            comments[0]
        );
        assert!(comments[0].contains("/runs/r-plan-3"), "{}", comments[0]);
        let record = f
            .app
            .store
            .run(&RunId::parse("r-plan-3").unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.status, RunStatus::Failed);
    }

    #[tokio::test]
    async fn closed_issues_and_pull_requests_are_not_planned() {
        let closed = FakeIssueWriter::new(Platform::GitHub, None, "Done.");
        closed.issue.lock().unwrap().open = false;
        let f = fixture(closed, vec![]).await;
        let error = run_plan(&f.app, request("r-plan-4"), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("is closed"), "{error}");

        let pull = FakeIssueWriter::new(Platform::GitHub, None, "Code.");
        pull.issue.lock().unwrap().is_pull_request = true;
        let f = fixture(pull, vec![]).await;
        let error = run_plan(&f.app, request("r-plan-5"), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("is a pull request"), "{error}");
        assert!(
            f.tracker.comments.lock().unwrap().is_empty(),
            "a refusal posts nothing"
        );
        assert!(f.model.requests().is_empty(), "and asks no model");
        assert!(
            f.app
                .store
                .run(&RunId::parse("r-plan-5").unwrap())
                .await
                .unwrap()
                .is_none(),
            "and records no run"
        );
    }

    #[test]
    fn a_refused_plan_fails_and_says_why() {
        let error = session_result(StopCause::Refused("refusal".to_owned()), 60).unwrap_err();
        assert_eq!(error.to_string(), "the model declined to plan (refusal)");
        assert!(session_result(StopCause::EndTurn, 60).is_ok());
        assert!(
            session_result(StopCause::Timeout, 60)
                .unwrap_err()
                .to_string()
                .contains("60s")
        );
    }
}
