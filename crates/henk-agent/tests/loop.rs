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

use henk_agent::{
    Agent, AgentConfig, AgentEvent, CallOutcome, RepeatFiring, StopCause, Tool, ToolOutput,
    ToolSet, Verdict, mcp_tools,
};
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
            ..Usage::default()
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
            ..Usage::default()
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
        max_repeated_calls: 3,
        record_argument_bytes: 4096,
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

fn warning() -> henk_agent::TurnWarning {
    henk_agent::TurnWarning {
        turns_left: 3,
        message: "3 turns left: wrap up.".to_owned(),
    }
}

fn busy_tools() -> ToolSet {
    let mut tools = ToolSet::new();
    tools.add(Echo);
    tools
}

#[tokio::test]
async fn the_turn_warning_comes_once_before_the_third_to_last_turn() {
    // A model that never stops calling a tool: it runs into max_turns = 5.
    // Each call differs, so the repeat guard stays out of it.
    let script: Vec<_> = (0..5)
        .map(|i| call(&format!("c{i}"), "sleep", json!({"n": i})))
        .collect();
    let model = Arc::new(ScriptedClient::new("m", script));
    let agent = Agent::new(model.clone(), busy_tools(), "s", config()).with_turn_warning(warning());
    let outcome = agent
        .run(vec![ChatMessage::user("go")], CancellationToken::new())
        .await;
    assert!(matches!(outcome.stop, StopCause::MaxTurns));
    let requests = model.requests();
    assert_eq!(requests.len(), 5);
    let carries = |i: usize| {
        requests[i]
            .messages
            .iter()
            .any(|m| m.text() == "3 turns left: wrap up.")
    };
    assert!(!carries(1), "not before the third-to-last turn");
    assert!(carries(2), "turn 3 of 5 is the third-to-last");
    assert_eq!(
        requests[2].messages.last().unwrap().text(),
        "3 turns left: wrap up."
    );
    let count = requests[4]
        .messages
        .iter()
        .filter(|m| m.text() == "3 turns left: wrap up.")
        .count();
    assert_eq!(count, 1, "sent once");
}

#[tokio::test]
async fn no_turn_warning_when_the_limit_is_that_small() {
    let model = Arc::new(ScriptedClient::new(
        "m",
        [
            call("c1", "sleep", json!({})),
            call("c2", "sleep", json!({})),
        ],
    ));
    let limits = AgentConfig {
        max_turns: 2,
        ..config()
    };
    let agent = Agent::new(model.clone(), busy_tools(), "s", limits).with_turn_warning(warning());
    agent
        .run(vec![ChatMessage::user("go")], CancellationToken::new())
        .await;
    assert!(
        model
            .requests()
            .iter()
            .flat_map(|r| &r.messages)
            .all(|m| m.text() != "3 turns left: wrap up.")
    );
}

#[tokio::test]
async fn a_refusal_ends_the_run_as_a_refusal_and_is_never_nudged() {
    let refused = Ok(Completion {
        message: ChatMessage::assistant(""),
        stop: StopReason::Refused("content_filter".to_owned()),
        usage: henk_llm::Usage::default(),
    });
    let model = Arc::new(ScriptedClient::new("m", [refused, text("second try")]));
    let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&asked);
    let agent = Agent::new(model.clone(), ToolSet::new(), "s", config()).with_continuation(
        Box::new(move |_| {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Some("Please try again.".to_owned())
        }),
    );
    let outcome = agent
        .run(
            vec![ChatMessage::user("review this")],
            CancellationToken::new(),
        )
        .await;
    assert!(matches!(&outcome.stop, StopCause::Refused(why) if why == "content_filter"));
    assert!(!outcome.stop.is_clean(), "a refusal is not a clean end");
    assert_eq!(outcome.turns, 1);
    assert_eq!(
        asked.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no nudge after a refusal"
    );
    assert_eq!(model.requests().len(), 1, "no second model call");
}

/// Counts its calls by tool name; every call succeeds.
struct Counter {
    name: &'static str,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl Tool for Counter {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: ToolName::parse(self.name).unwrap(),
            description: String::new(),
            input_schema: json!({}),
        }
    }

    async fn call(&self, _: Value) -> ToolOutput {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        ToolOutput::ok("same as before")
    }
}

/// A tool set of `read` and `list`, with their call counts.
fn counted_tools() -> (
    ToolSet,
    Arc<std::sync::atomic::AtomicUsize>,
    Arc<std::sync::atomic::AtomicUsize>,
) {
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let lists = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut set = ToolSet::new();
    set.add(Counter {
        name: "read",
        calls: Arc::clone(&reads),
    });
    set.add(Counter {
        name: "list",
        calls: Arc::clone(&lists),
    });
    (set, reads, lists)
}

fn count(calls: &std::sync::atomic::AtomicUsize) -> usize {
    calls.load(std::sync::atomic::Ordering::SeqCst)
}

/// The tool results of one message, as (`call_id`, content, `is_error`).
fn results_of(message: &ChatMessage) -> Vec<(String, String, bool)> {
    message
        .blocks
        .iter()
        .filter_map(|b| match b {
            Block::ToolResult(r) => Some((r.call_id.clone(), r.content.clone(), r.is_error)),
            _ => None,
        })
        .collect()
}

fn roomy() -> AgentConfig {
    AgentConfig {
        max_turns: 12,
        ..config()
    }
}

#[tokio::test]
async fn a_call_repeated_past_the_limit_is_refused_and_not_run() {
    let (set, reads, _) = counted_tools();
    let model = Arc::new(ScriptedClient::new(
        "m",
        [
            call("c1", "read", json!({"path": "a.rs", "line": 1})),
            call("c2", "read", json!({"line": 1, "path": "a.rs"})),
            call("c3", "read", json!({"path": "a.rs", "line": 1})),
            call("c4", "read", json!({"line": 1, "path": "a.rs"})),
            text("Done."),
        ],
    ));
    let agent = Agent::new(model.clone(), set, "s", roomy());
    let outcome = agent
        .run(vec![ChatMessage::user("go")], CancellationToken::new())
        .await;

    assert!(matches!(outcome.stop, StopCause::EndTurn));
    assert_eq!(count(&reads), 3, "the fourth identical call is not run");
    assert_eq!(
        outcome.repeats,
        vec![RepeatFiring {
            tool: "read".to_owned(),
            repeats: 4,
            ended: false,
        }]
    );
    let refused = results_of(&model.requests()[4].messages[8]);
    assert_eq!(refused.len(), 1);
    let (id, content, is_error) = &refused[0];
    assert_eq!(id, "c4");
    assert!(is_error);
    assert!(content.starts_with("Refused:"), "{content}");
    assert!(henk_domain::text::is_in_style(content));
}

#[tokio::test]
async fn repeating_after_the_refusal_ends_the_run_as_stuck() {
    let (set, reads, lists) = counted_tools();
    let same = || call("c", "read", json!({"path": "a.rs"}));
    let last = Ok(Completion {
        message: ChatMessage {
            role: Role::Assistant,
            blocks: vec![
                Block::ToolCall(ToolCall {
                    id: "c5".into(),
                    name: "read".into(),
                    arguments: ToolArguments::Parsed(json!({"path": "a.rs"})),
                }),
                Block::ToolCall(ToolCall {
                    id: "c6".into(),
                    name: "list".into(),
                    arguments: ToolArguments::Parsed(json!({})),
                }),
            ],
        },
        stop: StopReason::ToolUse,
        usage: Usage::default(),
    });
    let model = Arc::new(ScriptedClient::new(
        "m",
        [same(), same(), same(), same(), last, text("never asked")],
    ));
    let agent = Agent::new(model.clone(), set, "s", roomy());
    let outcome = agent
        .run(vec![ChatMessage::user("go")], CancellationToken::new())
        .await;

    assert!(
        matches!(&outcome.stop, StopCause::Stuck { tool, repeats: 5 } if tool == "read"),
        "{:?}",
        outcome.stop
    );
    assert!(!outcome.stop.is_clean(), "stuck is not a clean end");
    assert_eq!(outcome.turns, 5, "no model call after the stuck one");
    assert_eq!(count(&reads), 3);
    assert_eq!(count(&lists), 0, "calls after the stuck one are not run");
    assert_eq!(
        outcome
            .repeats
            .iter()
            .map(|f| (f.repeats, f.ended))
            .collect::<Vec<_>>(),
        vec![(4, false), (5, true)]
    );
    // Every call of the last turn has a result, so the conversation is
    // well formed for a transcript.
    let last_results = results_of(outcome.messages.last().unwrap());
    let ids: Vec<_> = last_results.iter().map(|(id, ..)| id.as_str()).collect();
    assert_eq!(ids, vec!["c5", "c6"]);
    assert!(last_results.iter().all(|(_, _, is_error)| *is_error));
}

#[tokio::test]
async fn identical_calls_in_the_refusal_turn_are_refused_not_stuck() {
    let (set, reads, _) = counted_tools();
    let read = |id: &str| {
        Block::ToolCall(ToolCall {
            id: id.into(),
            name: "read".into(),
            arguments: ToolArguments::Parsed(json!({"path": "a.rs"})),
        })
    };
    // Turn 4 holds two identical calls; the model has not seen the
    // refusal of the first when it makes the second.
    let both = Ok(Completion {
        message: ChatMessage {
            role: Role::Assistant,
            blocks: vec![read("c4"), read("c5")],
        },
        stop: StopReason::ToolUse,
        usage: Usage::default(),
    });
    let model = Arc::new(ScriptedClient::new(
        "m",
        [
            call("c1", "read", json!({"path": "a.rs"})),
            call("c2", "read", json!({"path": "a.rs"})),
            call("c3", "read", json!({"path": "a.rs"})),
            both,
            text("Done."),
        ],
    ));
    let agent = Agent::new(model.clone(), set, "s", roomy());
    let outcome = agent
        .run(vec![ChatMessage::user("go")], CancellationToken::new())
        .await;

    assert!(
        matches!(outcome.stop, StopCause::EndTurn),
        "{:?}",
        outcome.stop
    );
    assert_eq!(count(&reads), 3);
    assert_eq!(
        outcome
            .repeats
            .iter()
            .map(|f| (f.repeats, f.ended))
            .collect::<Vec<_>>(),
        vec![(4, false), (5, false)]
    );
    let refused = results_of(&model.requests()[4].messages[8]);
    assert_eq!(refused.len(), 2);
    assert!(
        refused
            .iter()
            .all(|(_, content, is_error)| *is_error && content.starts_with("Refused:"))
    );
}

#[tokio::test]
async fn other_arguments_or_a_call_in_between_do_not_trigger_the_guard() {
    let (set, reads, lists) = counted_tools();
    let model = Arc::new(ScriptedClient::new(
        "m",
        [
            call("c1", "read", json!({"path": "a.rs"})),
            call("c2", "read", json!({"path": "b.rs"})),
            call("c3", "read", json!({"path": "c.rs"})),
            call("c4", "read", json!({"path": "d.rs"})),
            call("c5", "read", json!({"path": "a.rs"})),
            call("c6", "read", json!({"path": "a.rs"})),
            call("c7", "read", json!({"path": "a.rs"})),
            call("c8", "list", json!({})),
            call("c9", "read", json!({"path": "a.rs"})),
            call("c10", "read", json!({"path": "a.rs"})),
            text("Done."),
        ],
    ));
    let agent = Agent::new(model.clone(), set, "s", roomy());
    let outcome = agent
        .run(vec![ChatMessage::user("go")], CancellationToken::new())
        .await;

    assert!(matches!(outcome.stop, StopCause::EndTurn));
    assert_eq!(count(&reads), 9, "every call ran");
    assert_eq!(count(&lists), 1);
    assert!(outcome.repeats.is_empty());
}

#[tokio::test]
async fn a_limit_of_zero_turns_the_guard_off() {
    let (set, reads, _) = counted_tools();
    let mut script: Vec<_> = (0..8)
        .map(|i| call(&format!("c{i}"), "read", json!({"path": "a.rs"})))
        .collect();
    script.push(text("Done."));
    let model = Arc::new(ScriptedClient::new("m", script));
    let limits = AgentConfig {
        max_repeated_calls: 0,
        ..roomy()
    };
    let agent = Agent::new(model.clone(), set, "s", limits);
    let outcome = agent
        .run(vec![ChatMessage::user("go")], CancellationToken::new())
        .await;

    assert!(matches!(outcome.stop, StopCause::EndTurn));
    assert_eq!(count(&reads), 8);
    assert!(outcome.repeats.is_empty());
}

#[tokio::test]
async fn the_guard_fires_as_an_event() {
    let (set, _, _) = counted_tools();
    let same = || call("c", "read", json!({"path": "a.rs"}));
    let model = Arc::new(ScriptedClient::new("m", [same(), same(), text("Done.")]));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let limits = AgentConfig {
        max_repeated_calls: 1,
        ..roomy()
    };
    let agent = Agent::new(model, set, "s", limits).with_events(tx);
    agent
        .run(vec![ChatMessage::user("go")], CancellationToken::new())
        .await;
    drop(agent);
    let mut fired = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let AgentEvent::RepeatRefused { .. } = event {
            fired.push(event);
        }
    }
    assert_eq!(
        fired,
        vec![AgentEvent::RepeatRefused {
            tool: "read".to_owned(),
            repeats: 2,
            ended: false,
        }]
    );
}

/// A tool that always reports an error.
struct Failing;

#[async_trait::async_trait]
impl Tool for Failing {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: ToolName::parse("fail").unwrap(),
            description: String::new(),
            input_schema: json!({}),
        }
    }

    async fn call(&self, _: Value) -> ToolOutput {
        ToolOutput::error("it broke")
    }
}

/// Every `ToolCalled` event, as (turn, name, origin, arguments, outcome).
fn calls_of(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<AgentEvent>,
) -> Vec<(u32, String, String, String, CallOutcome)> {
    let mut calls = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let AgentEvent::ToolCalled {
            turn,
            name,
            origin,
            arguments,
            outcome,
            ..
        } = event
        {
            calls.push((turn, name, origin, arguments, outcome));
        }
    }
    calls
}

/// `read` and `list`, a tool that fails, one that sleeps, and an MCP tool
/// the guard refuses, with the fake server behind it.
async fn every_kind_of_tool() -> (ToolSet, FakeServer) {
    let fake = FakeServer::new(
        vec![FakeServer::tool(
            "merge_pull_request",
            "Merges.",
            &["owner"],
        )],
        echo_behaviour(),
    );
    let session = Arc::new(fake.connect("github").await);
    let mut names = NameMap::new();
    let guard = Arc::new(|tool: &str, _: &Value| Verdict::Deny(format!("{tool} is not allowed")));
    let (mut set, _, _) = counted_tools();
    for tool in mcp_tools(session, &mut names, |_| true, guard)
        .await
        .unwrap()
    {
        set.add(tool);
    }
    set.add(Failing);
    set.add(Sleeper);
    (set, fake)
}

#[tokio::test]
async fn every_call_is_an_event_with_how_it_ended() {
    let (set, fake) = every_kind_of_tool().await;
    let tool_call = |id: &str, name: &str, arguments: ToolArguments| {
        Block::ToolCall(ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
        })
    };
    let first = Completion {
        message: ChatMessage {
            role: Role::Assistant,
            blocks: vec![
                tool_call("a", "read", ToolArguments::Parsed(json!({"path": "a.rs"}))),
                tool_call("b", "fail", ToolArguments::Parsed(json!({}))),
                tool_call("c", "nope", ToolArguments::Parsed(json!({}))),
                tool_call("d", "sleep", ToolArguments::Malformed("{oops".into())),
                tool_call(
                    "e",
                    "github__merge_pull_request",
                    ToolArguments::Parsed(json!({"owner": "o"})),
                ),
            ],
        },
        stop: StopReason::ToolUse,
        usage: Usage::default(),
    };
    let model = Arc::new(ScriptedClient::new(
        "m",
        [
            Ok(first),
            call("f", "read", json!({"path": "a.rs"})),
            call("g", "read", json!({"path": "a.rs"})),
            text("Done."),
        ],
    ));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let limits = AgentConfig {
        max_repeated_calls: 1,
        ..config()
    };
    let agent = Agent::new(model, set, "s", limits).with_events(tx);
    agent
        .run(vec![ChatMessage::user("go")], CancellationToken::new())
        .await;
    drop(agent);
    let seen = |turn, name: &str, origin: &str, arguments: &str, outcome| {
        (
            turn,
            name.to_owned(),
            origin.to_owned(),
            arguments.to_owned(),
            outcome,
        )
    };
    assert_eq!(
        calls_of(&mut rx),
        [
            seen(1, "read", "henk", r#"{"path":"a.rs"}"#, CallOutcome::Ok),
            seen(1, "fail", "henk", "{}", CallOutcome::Error),
            seen(1, "nope", "", "{}", CallOutcome::UnknownTool),
            seen(1, "sleep", "henk", "{oops", CallOutcome::MalformedArguments),
            seen(
                1,
                "github__merge_pull_request",
                "github",
                r#"{"owner":"o"}"#,
                CallOutcome::RefusedByScope
            ),
            seen(2, "read", "henk", r#"{"path":"a.rs"}"#, CallOutcome::Ok),
            seen(
                3,
                "read",
                "henk",
                r#"{"path":"a.rs"}"#,
                CallOutcome::RefusedAsRepeat
            ),
        ]
    );
    assert!(
        fake.calls().is_empty(),
        "a refused call never reaches the server"
    );
}

#[tokio::test]
async fn a_call_cut_off_by_a_cancel_is_an_event() {
    let mut set = ToolSet::new();
    set.add(Sleeper);
    let sleep = |id: &str| {
        Block::ToolCall(ToolCall {
            id: id.into(),
            name: "sleep".into(),
            arguments: ToolArguments::Parsed(json!({})),
        })
    };
    // The cancel lands during the first call; the second never starts but
    // is still on the record.
    let both = Ok(Completion {
        message: ChatMessage {
            role: Role::Assistant,
            blocks: vec![sleep("s1"), sleep("s2")],
        },
        stop: StopReason::ToolUse,
        usage: Usage::default(),
    });
    let model = Arc::new(ScriptedClient::new("m", [both]));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let agent = Agent::new(model, set, "s", config()).with_events(tx);
    let cancel = CancellationToken::new();
    let stopper = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        stopper.cancel();
    });
    agent.run(vec![ChatMessage::user("go")], cancel).await;
    drop(agent);
    let outcomes: Vec<_> = calls_of(&mut rx).into_iter().map(|c| c.4).collect();
    assert_eq!(outcomes, [CallOutcome::Cancelled, CallOutcome::NotRun]);
}
