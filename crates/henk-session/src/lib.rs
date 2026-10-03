//! One abstract way to run a model session.
//!
//! A [`SessionSpec`] says what varies: the model, the system prompt, the
//! opening messages, the tools and the limits. [`run_session`] does what every
//! session shares: a lane row in the run store, the loop, the mapping of how
//! it stopped to a lane status, and a line on the run's timeline. Review lanes
//! and the planner are both sessions.
//!
//! [`platform_tools`] is the one place where a platform's MCP read tools are
//! filtered and guarded by scope (spec §8.5) before a model may call them.

use std::sync::Arc;

use henk_agent::mcp_tools::McpTool;
use henk_agent::{Agent, AgentConfig, StopCause, ToolSet, Verdict, mcp_tools};
use henk_domain::allowlist::Platform;
use henk_domain::marker::ModelId;
use henk_domain::run::RunId;
use henk_domain::scope::{self, Scope};
use henk_llm::{ChatMessage, ModelClient, Usage};
use henk_mcp::{McpError, McpSession, NameMap};
use henk_store::{LaneStatus, RunStore};
use tokio_util::sync::CancellationToken;
use tracing::{info, instrument};

/// What varies between sessions.
pub struct SessionSpec {
    /// Lane name for the run page: a configured lane name, or `planner`.
    pub name: String,
    /// The model.
    pub model: Arc<dyn ModelClient>,
    /// The system prompt, persona included.
    pub system: String,
    /// The first messages of the conversation.
    pub opening: Vec<ChatMessage>,
    /// Every tool the model may call.
    pub tools: ToolSet,
    /// Turn limit, deadline, output cap.
    pub limits: AgentConfig,
}

impl std::fmt::Debug for SessionSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionSpec")
            .field("name", &self.name)
            .field("model", &self.model.model())
            .field("tools", &self.tools)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

/// How a session ended.
#[derive(Debug)]
pub struct SessionOutcome {
    /// Why the loop stopped.
    pub stop: StopCause,
    /// Model calls made.
    pub turns: u32,
    /// Tokens over the run.
    pub usage: Usage,
    /// The last assistant text.
    pub final_text: String,
    /// The lane status recorded in the store.
    pub status: LaneStatus,
    /// The error recorded, when dropped.
    pub error: Option<String>,
}

impl SessionOutcome {
    /// Whether the session finished on its own terms (end of turn or the
    /// turn limit), as opposed to timing out, being cancelled or failing.
    #[must_use]
    pub fn finished(&self) -> bool {
        self.status == LaneStatus::Finished
    }
}

/// Runs one session and records it as a lane of `run`.
#[instrument(skip_all, fields(run = %run, session = %spec.name, model = %spec.model.model()))]
pub async fn run_session(
    store: &RunStore,
    run: &RunId,
    spec: SessionSpec,
    cancel: CancellationToken,
) -> SessionOutcome {
    let model_name = spec.model.model().to_owned();
    if let Err(error) = store.start_lane(run, &spec.name, &model_name) {
        tracing::warn!(%error, "could not record the lane start");
    }
    let agent = Agent::new(
        Arc::clone(&spec.model),
        spec.tools,
        spec.system,
        spec.limits,
    );
    let outcome = agent.run(spec.opening, cancel).await;

    let (status, error) = match &outcome.stop {
        StopCause::EndTurn | StopCause::MaxTurns => (LaneStatus::Finished, None),
        StopCause::Timeout => (LaneStatus::Dropped, Some("timed out".to_owned())),
        StopCause::Cancelled => (LaneStatus::Dropped, Some("cancelled".to_owned())),
        StopCause::ModelError(e) => (LaneStatus::Dropped, Some(e.to_string())),
    };
    if let Err(store_error) = store.finish_lane(
        run,
        &spec.name,
        status,
        u64::from(outcome.turns),
        outcome.usage.input_tokens,
        outcome.usage.output_tokens,
        error.as_deref(),
    ) {
        tracing::warn!(error = %store_error, "could not record the lane end");
    }
    let last_words: String = outcome.final_text.chars().take(200).collect();
    let _ = store.event(
        run,
        if error.is_some() { "warn" } else { "info" },
        &format!(
            "{}: {:?} after {} turns; last words: {last_words}",
            spec.name, outcome.stop, outcome.turns
        ),
    );
    info!(turns = outcome.turns, ?status, "session ended");
    SessionOutcome {
        stop: outcome.stop,
        turns: outcome.turns,
        usage: outcome.usage,
        final_text: outcome.final_text,
        status,
        error,
    }
}

/// The read tools of one platform MCP session that `scope` allows, each
/// wrapped so that every call passes [`henk_domain::scope::guard`] first.
///
/// # Errors
///
/// Returns the [`McpError`] of listing the server's tools.
pub async fn platform_tools(
    session: Arc<dyn McpSession>,
    platform: Platform,
    scope: Scope,
) -> Result<Vec<McpTool>, McpError> {
    let guard: henk_agent::Guard = Arc::new(move |tool: &str, args: &serde_json::Value| {
        match scope::guard(platform, tool, args, &scope) {
            scope::Verdict::Allow(rewritten) => Verdict::Allow(rewritten),
            scope::Verdict::Deny(reason) => Verdict::Deny(reason),
        }
    });
    let mut names = NameMap::new();
    mcp_tools(
        session,
        &mut names,
        |info| scope::is_exposed(platform, &info.name),
        guard,
    )
    .await
}

/// The model id to put in markers: the model's own name, validated.
#[must_use]
pub fn model_id(model: &dyn ModelClient) -> ModelId {
    ModelId::parse(model.model().to_owned())
        .unwrap_or_else(|_| ModelId::parse("model").unwrap_or_else(|_| unreachable!("constant")))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::unnecessary_wraps
    )]

    use std::time::Duration;

    use henk_agent::Tool as _;
    use henk_domain::allowlist::RepoRef;
    use henk_domain::review::CommitSha;
    use henk_llm::testing::ScriptedClient;
    use henk_llm::{Completion, LlmError, StopReason};
    use henk_mcp::testing::{FakeServer, echo_behaviour};
    use serde_json::json;

    use super::*;

    fn text(text: &str) -> Result<Completion, LlmError> {
        Ok(Completion {
            message: ChatMessage::assistant(text),
            stop: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 12,
                output_tokens: 3,
            },
        })
    }

    fn run_id() -> RunId {
        RunId::parse("r-1").unwrap()
    }

    fn store_with_run() -> RunStore {
        let store = RunStore::in_memory().unwrap();
        store
            .create_run(&henk_store::NewRun {
                id: run_id(),
                kind: henk_domain::run::RunKind::Plan,
                platform: Platform::GitHub,
                repo: "o/r".into(),
                target: 1,
                commit: None,
                requester: None,
                trigger: "test".into(),
                link: "l".into(),
            })
            .unwrap();
        store
    }

    fn spec(model: Arc<dyn ModelClient>) -> SessionSpec {
        SessionSpec {
            name: "planner".into(),
            model,
            system: "s".into(),
            opening: vec![ChatMessage::user("go")],
            tools: ToolSet::new(),
            limits: AgentConfig {
                max_turns: 3,
                timeout: Duration::from_secs(5),
                max_tool_output_chars: 100,
            },
        }
    }

    #[tokio::test]
    async fn a_finished_session_records_a_lane_row() {
        let store = store_with_run();
        let model: Arc<dyn ModelClient> = Arc::new(ScriptedClient::new("m", [text("done")]));
        let outcome = run_session(&store, &run_id(), spec(model), CancellationToken::new()).await;
        assert!(outcome.finished());
        assert_eq!(outcome.final_text, "done");
        let lanes = store.lanes(&run_id()).unwrap();
        assert_eq!(lanes.len(), 1);
        assert_eq!(lanes[0].name, "planner");
        assert_eq!(lanes[0].model, "m");
        assert_eq!(lanes[0].status, LaneStatus::Finished);
        assert_eq!(lanes[0].input_tokens, 12);
        assert_eq!(store.events(&run_id()).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_model_error_drops_the_session_with_the_error_text() {
        let store = store_with_run();
        let model: Arc<dyn ModelClient> = Arc::new(ScriptedClient::new(
            "m",
            [Err(LlmError::Unauthorized {
                status: 401,
                body: "nope".into(),
            })],
        ));
        let outcome = run_session(&store, &run_id(), spec(model), CancellationToken::new()).await;
        assert!(!outcome.finished());
        assert_eq!(outcome.status, LaneStatus::Dropped);
        assert!(outcome.error.as_deref().unwrap_or("").contains("401"));
        let lanes = store.lanes(&run_id()).unwrap();
        assert_eq!(lanes[0].status, LaneStatus::Dropped);
    }

    #[tokio::test]
    async fn platform_tools_exposes_only_the_scope_table_and_pins_arguments() {
        let fake = FakeServer::new(
            vec![
                FakeServer::tool(
                    "pull_request_read",
                    "",
                    &["owner", "repo", "pullNumber", "method"],
                ),
                FakeServer::tool("merge_pull_request", "", &["owner"]),
            ],
            echo_behaviour(),
        );
        let session: Arc<dyn McpSession> = Arc::new(fake.connect("github").await);
        let scope = Scope::Review {
            repo: RepoRef::parse(Platform::GitHub, "docspec/app").unwrap(),
            number: 7,
            commit: CommitSha::parse("0123456789abcdef0123456789abcdef01234567").unwrap(),
        };
        let tools = platform_tools(session, Platform::GitHub, scope)
            .await
            .unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(
            tools[0].definition().name.as_str(),
            "github__pull_request_read"
        );
        let output = tools[0]
            .call(json!({"owner": "evil", "repo": "x", "pullNumber": 1, "method": "get"}))
            .await;
        assert!(!output.is_error, "{output:?}");
        assert_eq!(fake.calls()[0].arguments["owner"], "docspec");
        assert_eq!(fake.calls()[0].arguments["pullNumber"], 7);
    }

    #[test]
    fn model_id_falls_back_for_unusable_names() {
        let good = ScriptedClient::new("claude-x", []);
        assert_eq!(model_id(&good).as_str(), "claude-x");
        let bad = ScriptedClient::new("has space", []);
        assert_eq!(model_id(&bad).as_str(), "model");
    }
}
