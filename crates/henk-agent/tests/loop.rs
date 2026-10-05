//! The loop against a scripted model and the fake MCP server.

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::unnecessary_wraps,
    missing_docs
)]

use std::sync::Arc;
use std::time::Duration;

use henk_agent::{Agent, AgentConfig, StopCause, Tool, ToolOutput, ToolSet, Verdict, mcp_tools};
use henk_llm::testing::ScriptedClient;
use henk_llm::{
    Block, ChatMessage, Completion, Role, StopReason, ToolArguments, ToolCall, ToolDef, ToolName,
    Usage,
};
use henk_mcp::NameMap;
use henk_mcp::testing::{FakeServer, echo_behaviour};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn text(text: &str) -> Result<Completion, henk_llm::LlmError> {
    Ok(Completion {
        message: ChatMessage::assistant(text),
        stop: StopReason::EndTurn,
        usage: Usage {
            input_tokens: 10,
            output_tokens: 2,
        },
    })
}

fn call(id: &str, name: &str, arguments: Value) -> Result<Completion, henk_llm::LlmError> {
    Ok(Completion {
        message: ChatMessage {
            role: Role::Assistant,
            blocks: vec![Block::ToolCall(ToolCall {
                id: id.to_owned(),
                name: name.to_owned(),
                arguments: ToolArguments::Parsed(arguments),
            })],
        },
        stop: StopReason::ToolUse,
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
        },
    })
}

fn config() -> AgentConfig {
    AgentConfig {
        max_turns: 5,
        timeout: Duration::from_secs(5),
        max_tool_output_chars: 100,
    }
}

struct Sleeper;

#[async_trait::async_trait]
impl Tool for Sleeper {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: ToolName::parse("sleep").unwrap(),
            description: String::new(),
            input_schema: json!({}),
        }
    }

    async fn call(&self, _: Value) -> ToolOutput {
        tokio::time::sleep(Duration::from_secs(30)).await;
        ToolOutput::ok("woke")
    }
}

#[tokio::test]
async fn round_trips_a_tool_call_through_mcp_with_a_guard() {
    let fake = FakeServer::new(
        vec![
            FakeServer::tool(
                "get_pull_request",
                "Reads a PR.",
                &["owner", "repo", "pullNumber"],
            ),
            FakeServer::tool("merge_pull_request", "Merges.", &["owner"]),
        ],
        echo_behaviour(),
    );
    let session = Arc::new(fake.connect("github").await);
    let mut names = NameMap::new();
    let guard = Arc::new(|tool: &str, args: &Value| {
        if tool != "get_pull_request" {
            return Verdict::Deny(format!("{tool} is not allowed"));
        }
        let mut pinned = args.clone();
        pinned["owner"] = json!("pinned-owner");
        Verdict::Allow(pinned)
    });
    let tools = mcp_tools(
        session,
        &mut names,
        |info| info.name != "merge_pull_request",
        guard,
    )
    .await
    .unwrap();
    assert_eq!(tools.len(), 1, "merge tool not exposed");
    let mut set = ToolSet::new();
    for tool in tools {
        set.add(tool);
    }
    assert_eq!(
        set.names().collect::<Vec<_>>(),
        vec!["github__get_pull_request"]
    );

    let model = Arc::new(ScriptedClient::new(
        "fake-model",
        [
            call(
                "c1",
                "github__get_pull_request",
                json!({"owner": "evil", "repo": "r", "pullNumber": 7}),
            ),
            text("Not bad."),
        ],
    ));
    let agent = Agent::new(model.clone(), set, "system", config());
    let outcome = agent
        .run(vec![ChatMessage::user("review")], CancellationToken::new())
        .await;

    assert!(matches!(outcome.stop, StopCause::EndTurn));
    assert_eq!(outcome.final_text, "Not bad.");
    assert_eq!(outcome.turns, 2);
    assert_eq!(outcome.usage.input_tokens, 20);
    let calls = fake.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].arguments["owner"], "pinned-owner",
        "guard rewrote the owner"
    );

    let second_request = &model.requests()[1];
    let last = second_request.messages.last().unwrap();
    assert!(matches!(&last.blocks[0], Block::ToolResult(r) if r.call_id == "c1" && !r.is_error));
    assert_eq!(second_request.tools.len(), 1);
    assert_eq!(second_request.system.as_deref(), Some("system"));
}

#[tokio::test]
async fn unknown_tools_and_malformed_arguments_become_error_results() {
    let model = Arc::new(ScriptedClient::new(
        "m",
        [
            Ok(Completion {
                message: ChatMessage {
                    role: Role::Assistant,
                    blocks: vec![
                        Block::ToolCall(ToolCall {
                            id: "a".into(),
                            name: "nope".into(),
                            arguments: ToolArguments::Parsed(json!({})),
                        }),
                        Block::ToolCall(ToolCall {
                            id: "b".into(),
                            name: "sleep".into(),
                            arguments: ToolArguments::Malformed("{oops".into()),
                        }),
                    ],
                },
                stop: StopReason::ToolUse,
                usage: Usage::default(),
            }),
            text("done"),
        ],
    ));
    let mut set = ToolSet::new();
    set.add(Sleeper);
    let agent = Agent::new(model.clone(), set, "s", config());
    let outcome = agent
        .run(vec![ChatMessage::user("go")], CancellationToken::new())
        .await;
    assert!(matches!(outcome.stop, StopCause::EndTurn));
    let results = &model.requests()[1].messages[2];
    let errors: Vec<bool> = results
        .blocks
        .iter()
        .map(|b| match b {
            Block::ToolResult(r) => r.is_error,
            _ => panic!("expected tool results"),
        })
        .collect();
    assert_eq!(errors, vec![true, true]);
}

#[tokio::test]
async fn stops_at_the_turn_limit() {
    let script = (0..10).map(|i| call(&format!("c{i}"), "sleep", json!({})));
    let model = Arc::new(ScriptedClient::new("m", script));
    let mut set = ToolSet::new();
    set.add(Echo);
    let agent = Agent::new(
        model,
        set,
        "s",
        AgentConfig {
            max_turns: 3,
            ..config()
        },
    );
    let outcome = agent
        .run(vec![ChatMessage::user("go")], CancellationToken::new())
        .await;
    assert!(matches!(outcome.stop, StopCause::MaxTurns));
    assert_eq!(outcome.turns, 3);
}

struct Echo;

#[async_trait::async_trait]
impl Tool for Echo {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: ToolName::parse("sleep").unwrap(),
            description: String::new(),
            input_schema: json!({}),
        }
    }

    async fn call(&self, _: Value) -> ToolOutput {
        ToolOutput::ok("x".repeat(500))
    }
}

#[tokio::test]
async fn long_tool_output_is_truncated() {
    let model = Arc::new(ScriptedClient::new(
        "m",
        [call("c", "sleep", json!({})), text("ok")],
    ));
    let mut set = ToolSet::new();
    set.add(Echo);
    let agent = Agent::new(model.clone(), set, "s", config());
    let _ = agent
        .run(vec![ChatMessage::user("go")], CancellationToken::new())
        .await;
    let Block::ToolResult(result) = &model.requests()[1].messages[2].blocks[0] else {
        panic!()
    };
    assert!(result.content.starts_with(&"x".repeat(100)));
    assert!(
        result
            .content
            .contains("[output truncated at 100 characters]")
    );
}

#[tokio::test(start_paused = true)]
async fn times_out_during_a_slow_tool() {
    let model = Arc::new(ScriptedClient::new("m", [call("c", "sleep", json!({}))]));
    let mut set = ToolSet::new();
    set.add(Sleeper);
    let agent = Agent::new(
        model,
        set,
        "s",
        AgentConfig {
            timeout: Duration::from_millis(50),
            ..config()
        },
    );
    let outcome = agent
        .run(vec![ChatMessage::user("go")], CancellationToken::new())
        .await;
    assert!(matches!(outcome.stop, StopCause::Timeout));
    assert_eq!(outcome.turns, 1);
    // The slow tool was allowed to finish; its result is in the conversation
    // and no second model call was made.
    let last = outcome.messages.last().unwrap();
    assert!(matches!(
        last.blocks.first(),
        Some(Block::ToolResult(result)) if result.content == "woke"
    ));
}

#[tokio::test(start_paused = true)]
async fn a_slow_model_call_is_abandoned_at_the_deadline() {
    let model = Arc::new(ScriptedClient::new("m", []).with_delay(Duration::from_secs(30)));
    let agent = Agent::new(
        model,
        ToolSet::new(),
        "s",
        AgentConfig {
            timeout: Duration::from_millis(50),
            ..config()
        },
    );
    let outcome = agent
        .run(vec![ChatMessage::user("go")], CancellationToken::new())
        .await;
    assert!(matches!(outcome.stop, StopCause::Timeout));
    assert_eq!(outcome.turns, 1);
    assert_eq!(outcome.messages.len(), 1, "no answer was recorded");
}

#[tokio::test]
async fn cancellation_stops_the_run() {
    let model = Arc::new(ScriptedClient::new("m", [call("c", "sleep", json!({}))]));
    let mut set = ToolSet::new();
    set.add(Sleeper);
    let agent = Agent::new(model, set, "s", config());
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        token.cancel();
    });
    let outcome = agent.run(vec![ChatMessage::user("go")], cancel).await;
    assert!(matches!(outcome.stop, StopCause::Cancelled));
}

#[tokio::test]
async fn model_errors_end_the_run() {
    let model = Arc::new(ScriptedClient::new(
        "m",
        [Err(henk_llm::LlmError::Unauthorized {
            status: 401,
            body: String::new(),
        })],
    ));
    let agent = Agent::new(model, ToolSet::new(), "s", config());
    let outcome = agent
        .run(vec![ChatMessage::user("go")], CancellationToken::new())
        .await;
    assert!(matches!(
        outcome.stop,
        StopCause::ModelError(henk_llm::LlmError::Unauthorized { .. })
    ));
    assert_eq!(outcome.final_text, "");
}
