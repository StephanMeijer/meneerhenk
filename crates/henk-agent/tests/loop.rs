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
        max_conversation_chars: 100_000,
        keep_recent_turns: 2,
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

#[tokio::test]
async fn old_tool_results_are_elided_once_the_conversation_is_over_budget() {
    // Three turns of one call each returning 500 chars; budget 900, keep 1.
    let model = Arc::new(ScriptedClient::new(
        "m",
        [
            call("c1", "echo", json!({"n": 1})),
            call("c2", "echo", json!({"n": 2})),
            call("c3", "echo", json!({"n": 3})),
            text("done"),
        ],
    ));
    let mut set = ToolSet::new();
    set.add(Big);
    let agent = Agent::new(
        model.clone(),
        set,
        "s",
        AgentConfig {
            max_tool_output_chars: 10_000,
            max_conversation_chars: 900,
            keep_recent_turns: 1,
            ..config()
        },
    );
    let outcome = agent
        .run(vec![ChatMessage::user("go")], CancellationToken::new())
        .await;
    assert!(matches!(outcome.stop, StopCause::EndTurn));
    // What the model saw on its last call: the first two results stubbed,
    // the third intact.
    let last_request = model.requests().last().unwrap().clone();
    let results: Vec<String> = last_request
        .messages
        .iter()
        .flat_map(|m| &m.blocks)
        .filter_map(|b| match b {
            Block::ToolResult(r) => Some(r.content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 3);
    assert!(
        results[0].starts_with("[result of echo from turn 1 elided (500 chars)"),
        "{}",
        results[0]
    );
    assert!(
        results[1].starts_with("[result of echo from turn 2 elided"),
        "{}",
        results[1]
    );
    assert_eq!(results[2].len(), 500);
}

struct Big;

#[async_trait::async_trait]
impl Tool for Big {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: ToolName::parse("echo").unwrap(),
            description: String::new(),
            input_schema: json!({"type": "object"}),
        }
    }

    async fn call(&self, _: Value) -> ToolOutput {
        ToolOutput::ok("y".repeat(500))
    }
}

#[tokio::test]
async fn a_continuation_gets_one_more_turn_then_the_run_ends() {
    let model = Arc::new(ScriptedClient::new(
        "m",
        [text("I am done."), text("Still done.")],
    ));
    let nudges = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let seen = Arc::clone(&nudges);
    let agent = Agent::new(model.clone(), ToolSet::new(), "s", config()).with_continuation(
        Box::new(move |ending| {
            assert_eq!(ending.reason, henk_agent::EndReason::EndTurn);
            if seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                Some(format!("Look again (turn {}).", ending.turn))
            } else {
                None
            }
        }),
    );
    let outcome = agent
        .run(vec![ChatMessage::user("go")], CancellationToken::new())
        .await;
    assert!(matches!(outcome.stop, StopCause::EndTurn));
    assert_eq!(outcome.turns, 2);
    assert_eq!(nudges.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(outcome.final_text, "Still done.");
    let second = &model.requests()[1];
    assert_eq!(
        second.messages.last().unwrap().text(),
        "Look again (turn 1)."
    );
}

#[tokio::test]
async fn an_output_cap_without_tool_calls_reaches_the_continuation() {
    let cut = Ok(Completion {
        message: ChatMessage::assistant("<think>thinking thinking"),
        stop: StopReason::MaxTokens,
        usage: henk_llm::Usage::default(),
    });
    let model = Arc::new(ScriptedClient::new("m", [cut, text("posted")]));
    let reasons = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&reasons);
    let agent = Agent::new(model, ToolSet::new(), "s", config()).with_continuation(Box::new(
        move |ending| {
            sink.lock().unwrap().push(ending.reason);
            matches!(ending.reason, henk_agent::EndReason::OutputCap)
                .then(|| "Post what you are sure of.".to_owned())
        },
    ));
    let outcome = agent
        .run(vec![ChatMessage::user("go")], CancellationToken::new())
        .await;
    assert_eq!(outcome.turns, 2);
    assert_eq!(
        *reasons.lock().unwrap(),
        vec![
            henk_agent::EndReason::OutputCap,
            henk_agent::EndReason::EndTurn
        ]
    );
}
