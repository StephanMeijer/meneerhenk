//! A lane on the Responses API (#365): the loop with a real Responses
//! client against a recorded-shape server, through three turns, with the
//! model's reasoning carried from each turn into the next.

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::too_many_lines,
    missing_docs
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use henk_agent::{Agent, AgentConfig, StopCause, Tool, ToolOutput, ToolSet};
use henk_llm::{
    Api, ChatMessage, MaxTokensParam, ModelConfig, Provider, RetryPolicy, ToolDef, ToolName,
    client_for,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Counts its calls and answers with the file asked for.
struct ReadFile(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl Tool for ReadFile {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: ToolName::parse("read_file").unwrap(),
            description: "Reads a file.".to_owned(),
            input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        }
    }

    async fn call(&self, arguments: Value) -> ToolOutput {
        self.0.fetch_add(1, Ordering::SeqCst);
        ToolOutput::ok(format!(
            "contents of {}",
            arguments["path"].as_str().unwrap_or("?")
        ))
    }
}

fn reasoning(id: &str) -> Value {
    json!({"type": "reasoning", "id": id, "summary": [], "encrypted_content": format!("enc-{id}")})
}

fn reading(turn: usize, path: &str) -> Value {
    json!({
        "id": format!("resp_{turn}"),
        "status": "completed",
        "output": [
            reasoning(&format!("rs_{turn}")),
            {"type": "function_call", "id": format!("fc_{turn}"), "call_id": format!("call_{turn}"),
             "name": "read_file", "arguments": json!({"path": path}).to_string()},
        ],
        "usage": {"input_tokens": 100 * turn, "output_tokens": 40,
                  "output_tokens_details": {"reasoning_tokens": 30}},
    })
}

#[tokio::test]
async fn a_lane_runs_turns_on_the_responses_api_with_its_reasoning_carried() {
    let server = MockServer::start().await;
    let answers = [
        reading(1, "src/lib.rs"),
        reading(2, "src/main.rs"),
        json!({
            "id": "resp_3",
            "status": "completed",
            "output": [
                reasoning("rs_3"),
                {"type": "message", "role": "assistant",
                 "content": [{"type": "output_text", "text": "No findings."}]},
            ],
            "usage": {"input_tokens": 300, "output_tokens": 20,
                      "output_tokens_details": {"reasoning_tokens": 12}},
        }),
    ];
    for answer in answers {
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(answer))
            .up_to_n_times(1)
            .mount(&server)
            .await;
    }
    let model = client_for(ModelConfig {
        provider: Provider::OpenAi,
        api: Api::Responses,
        base_url: format!("{}/v1", server.uri()),
        api_key: "k".to_owned().into(),
        token_source: None,
        model: "test-model".to_owned(),
        max_tokens: 1024,
        timeout: Duration::from_secs(5),
        retry: RetryPolicy::default(),
        max_tokens_param: MaxTokensParam::MaxTokens,
        effort: None,
        prompt_cache: true,
        headers: Vec::new(),
    })
    .unwrap();
    let reads = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolSet::new();
    tools.add(ReadFile(Arc::clone(&reads)));
    let agent = Agent::new(
        model,
        tools,
        "You review.",
        AgentConfig {
            max_turns: 5,
            timeout: Duration::from_secs(10),
            max_tool_output_chars: 1_000,
            max_conversation_chars: 100_000,
            keep_recent_turns: 4,
            max_repeated_calls: 3,
            record_argument_bytes: 4096,
            cache_ttl: henk_llm::CacheTtl::Short,
        },
    );
    let outcome = agent
        .run(
            vec![ChatMessage::user("Review it.")],
            CancellationToken::new(),
        )
        .await;

    assert!(
        matches!(outcome.stop, StopCause::EndTurn),
        "{:?}",
        outcome.stop
    );
    assert_eq!(outcome.final_text, "No findings.");
    assert_eq!(outcome.turns, 3);
    assert_eq!(reads.load(Ordering::SeqCst), 2, "both reads ran");
    assert_eq!(outcome.usage.reasoning_tokens, 72);
    assert_eq!(outcome.usage.output_tokens, 100);

    let inputs: Vec<Vec<Value>> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["store"], false);
            assert_eq!(body["instructions"], "You review.");
            body["input"].as_array().unwrap().clone()
        })
        .collect();
    assert_eq!(inputs.len(), 3);
    let kinds = |items: &[Value]| -> Vec<String> {
        items
            .iter()
            .map(|item| {
                item["type"].as_str().map_or_else(
                    || format!("{}", item["role"]),
                    |kind| {
                        item["id"]
                            .as_str()
                            .map_or_else(|| kind.to_owned(), |id| format!("{kind}:{id}"))
                    },
                )
            })
            .collect()
    };
    assert_eq!(
        kinds(&inputs[2]),
        [
            "\"user\"",
            "reasoning:rs_1",
            "function_call",
            "function_call_output",
            "reasoning:rs_2",
            "function_call",
            "function_call_output",
        ],
        "every turn's reasoning goes back, in its place"
    );
    assert_eq!(inputs[2][1], reasoning("rs_1"), "unchanged");
    assert_eq!(inputs[2][3]["output"], "contents of src/lib.rs");
    assert_eq!(
        inputs[1].len(),
        4,
        "the second turn saw the first's reasoning and result"
    );
}
