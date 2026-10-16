//! The Responses adapter (#365) against fixtures and wiremock.

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    missing_docs
)]

use std::time::Duration;

use henk_llm::{
    Api, Block, ChatMessage, CompletionRequest, Effort, LlmError, MaxTokensParam, ModelConfig,
    Provider, RetryPolicy, Role, StopReason, ToolArguments, ToolChoice, ToolDef, ToolName,
    ToolResult, client_for, responses,
};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> Value {
    let path = format!(
        "{}/tests/fixtures/openai_responses/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    serde_json::from_str(&text).unwrap()
}

fn config(base_url: &str) -> ModelConfig {
    ModelConfig {
        provider: Provider::OpenAi,
        api: Api::Responses,
        base_url: base_url.to_owned(),
        api_key: "secret-key".to_owned().into(),
        token_source: None,
        model: "test-model".to_owned(),
        max_tokens: 1024,
        timeout: Duration::from_secs(5),
        retry: RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
        },
        max_tokens_param: MaxTokensParam::MaxTokens,
        effort: None,
        prompt_cache: true,
        headers: Vec::new(),
    }
}

fn tool() -> ToolDef {
    ToolDef {
        name: ToolName::parse("get_pull_request").unwrap(),
        description: "Reads a pull request.".to_owned(),
        input_schema: json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {"owner": {"type": "string"}},
            "required": ["owner"]
        }),
    }
}

fn client() -> responses::ResponsesClient {
    responses::ResponsesClient::new(config("https://api.test/v1")).unwrap()
}

fn result(call_id: &str, content: &str, is_error: bool) -> ChatMessage {
    ChatMessage::tool_results([ToolResult {
        call_id: call_id.to_owned(),
        content: content.to_owned(),
        is_error,
    }])
}

#[test]
fn the_body_is_stateless_and_names_the_function_tools() {
    let body = client().body(&CompletionRequest {
        system: Some("Be terse.".to_owned()),
        messages: vec![ChatMessage::user("Review this.")],
        tools: vec![tool()],
        tool_choice: ToolChoice::Required,
        max_tokens: Some(2048),
        ..Default::default()
    });
    assert_eq!(body["model"], "test-model");
    assert_eq!(body["instructions"], "Be terse.");
    assert_eq!(body["store"], false);
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(body["max_output_tokens"], 2048);
    assert!(body.get("max_tokens").is_none());
    assert_eq!(
        body["input"],
        json!([{"role": "user", "content": [{"type": "input_text", "text": "Review this."}]}])
    );
    assert_eq!(body["tool_choice"], "required");
    let sent = &body["tools"][0];
    assert_eq!(sent["type"], "function");
    assert_eq!(sent["name"], "get_pull_request");
    assert!(
        sent["parameters"].get("$schema").is_none(),
        "the schema is cleaned as for chat completions"
    );
    let named = client().body(&CompletionRequest {
        tools: vec![tool()],
        tool_choice: ToolChoice::Named(ToolName::parse("get_pull_request").unwrap()),
        ..Default::default()
    });
    assert_eq!(
        named["tool_choice"],
        json!({"type": "function", "name": "get_pull_request"})
    );
}

#[test]
fn effort_goes_as_reasoning_effort_and_no_temperature() {
    let mut cfg = config("https://x.test/v1");
    cfg.effort = Some(Effort::Low);
    let with_effort = responses::ResponsesClient::new(cfg).unwrap();
    let body = with_effort.body(&CompletionRequest {
        temperature: Some(0.2),
        ..Default::default()
    });
    assert_eq!(body["reasoning"], json!({"effort": "low"}));
    assert!(body.get("temperature").is_none());
    let body = client().body(&CompletionRequest {
        temperature: Some(0.2),
        ..Default::default()
    });
    assert!(
        body.get("reasoning").is_none(),
        "nothing sent without effort"
    );
    assert_eq!(body["temperature"], 0.2);
}

#[tokio::test]
async fn a_400_about_reasoning_names_the_effort_setting() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"error":{"message":"Unsupported parameter: 'reasoning.effort' is not supported with this model.","param":"reasoning.effort"}}"#,
        ))
        .expect(1)
        .mount(&server)
        .await;
    let mut cfg = config(&format!("{}/v1", server.uri()));
    cfg.effort = Some(Effort::Xhigh);
    let error = client_for(cfg)
        .unwrap()
        .complete(&CompletionRequest::default())
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            LlmError::EffortRejected {
                effort: "xhigh",
                param: "reasoning.effort",
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn a_conversation_goes_as_items_with_its_reasoning_in_place() {
    let reasoning =
        json!({"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "enc"});
    let messages = vec![
        ChatMessage::user("Review this."),
        ChatMessage {
            role: Role::Assistant,
            blocks: vec![
                Block::Opaque(reasoning.clone()),
                Block::Text("Reading it.".to_owned()),
                Block::ToolCall(henk_llm::ToolCall {
                    id: "call_1".to_owned(),
                    name: "get_pull_request".to_owned(),
                    arguments: ToolArguments::Parsed(json!({"owner": "o"})),
                }),
                Block::ToolCall(henk_llm::ToolCall {
                    id: "call_2".to_owned(),
                    name: "get_pull_request".to_owned(),
                    arguments: ToolArguments::Malformed("{\"owner\":".to_owned()),
                }),
            ],
        },
        ChatMessage::tool_results([
            ToolResult {
                call_id: "call_1".to_owned(),
                content: "{\"title\": \"Fix\"}".to_owned(),
                is_error: false,
            },
            ToolResult {
                call_id: "call_2".to_owned(),
                content: "bad arguments".to_owned(),
                is_error: true,
            },
        ]),
    ];
    let body = client().body(&CompletionRequest {
        messages,
        ..Default::default()
    });
    assert_eq!(
        body["input"],
        json!([
            {"role": "user", "content": [{"type": "input_text", "text": "Review this."}]},
            reasoning,
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Reading it."}]},
            {"type": "function_call", "call_id": "call_1", "name": "get_pull_request", "arguments": "{\"owner\":\"o\"}"},
            {"type": "function_call", "call_id": "call_2", "name": "get_pull_request", "arguments": "{\"owner\":"},
            {"type": "function_call_output", "call_id": "call_1", "output": "{\"title\": \"Fix\"}"},
            {"type": "function_call_output", "call_id": "call_2", "output": "Error: bad arguments"},
        ])
    );
}

#[test]
fn other_providers_opaque_content_is_not_sent() {
    let body = client().body(&CompletionRequest {
        messages: vec![ChatMessage {
            role: Role::Assistant,
            blocks: vec![
                Block::Opaque(json!({"type": "thinking", "thinking": "hm", "signature": "s"})),
                Block::Opaque(json!({"reasoning_content": "from a chat completion"})),
                Block::Text("Done.".to_owned()),
            ],
        }],
        ..Default::default()
    });
    assert_eq!(body["input"].as_array().unwrap().len(), 1, "{body}");
    assert_eq!(body["input"][0]["type"], "message");
}

#[test]
fn a_reasoning_item_with_nothing_after_it_is_not_sent() {
    let reasoning =
        json!({"type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "enc"});
    let alone = client().body(&CompletionRequest {
        messages: vec![ChatMessage {
            role: Role::Assistant,
            blocks: vec![
                Block::Opaque(reasoning.clone()),
                Block::Opaque(reasoning.clone()),
                Block::Text(String::new()),
            ],
        }],
        ..Default::default()
    });
    assert_eq!(alone["input"], json!([]), "{alone}");

    let before_text = client().body(&CompletionRequest {
        messages: vec![ChatMessage {
            role: Role::Assistant,
            blocks: vec![
                Block::Opaque(reasoning.clone()),
                Block::Text("Done.".to_owned()),
                Block::Opaque(json!({"type": "reasoning", "id": "rs_2"})),
            ],
        }],
        ..Default::default()
    });
    assert_eq!(
        before_text["input"],
        json!([
            reasoning,
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Done."}]},
        ]),
        "the one with an item after it stays, the trailing one goes"
    );
}

#[test]
fn a_turn_cut_while_thinking_goes_back_without_its_reasoning() {
    let cut = responses::decode(&fixture("incomplete.json")).unwrap();
    assert_eq!(cut.stop, StopReason::MaxTokens);
    assert!(matches!(cut.message.blocks.as_slice(), [Block::Opaque(_)]));
    let body = client().body(&CompletionRequest {
        messages: vec![
            ChatMessage::user("Review this."),
            cut.message,
            ChatMessage::user("You ran out of tokens. Continue."),
        ],
        ..Default::default()
    });
    assert_eq!(
        body["input"],
        json!([
            {"role": "user", "content": [{"type": "input_text", "text": "Review this."}]},
            {"role": "user", "content": [{"type": "input_text", "text": "You ran out of tokens. Continue."}]},
        ]),
        "no reasoning item is left dangling before the nudge"
    );
}

#[test]
fn text_decodes_with_its_cached_tokens() {
    let completion = responses::decode(&fixture("text.json")).unwrap();
    assert_eq!(completion.message.text(), "Looks good.");
    assert_eq!(completion.stop, StopReason::EndTurn);
    assert_eq!(
        (
            completion.usage.input_tokens,
            completion.usage.cache_read_tokens,
            completion.usage.output_tokens
        ),
        (20, 100, 8)
    );
}

#[test]
fn a_function_call_decodes_with_its_reasoning_kept_whole() {
    let response = fixture("reasoning_tool_call.json");
    let completion = responses::decode(&response).unwrap();
    assert_eq!(completion.stop, StopReason::ToolUse);
    assert_eq!(
        completion.message.blocks[0],
        Block::Opaque(response["output"][0].clone()),
        "the reasoning item, encrypted content and all"
    );
    let call = completion.message.tool_calls().next().unwrap();
    assert_eq!(call.id, "call_1", "the call_id, which the output answers");
    assert_eq!(call.arguments, ToolArguments::Parsed(json!({"owner": "o"})));
    assert_eq!(
        (
            completion.usage.output_tokens,
            completion.usage.reasoning_tokens
        ),
        (90, 64)
    );
}

#[test]
fn an_incomplete_response_stops_at_the_cap() {
    let completion = responses::decode(&fixture("incomplete.json")).unwrap();
    assert_eq!(completion.stop, StopReason::MaxTokens);
    let filtered = responses::decode(&json!({
        "status": "incomplete",
        "incomplete_details": {"reason": "content_filter"},
        "output": [],
    }))
    .unwrap();
    assert_eq!(
        filtered.stop,
        StopReason::Refused("content_filter".to_owned())
    );
}

#[test]
fn a_refusal_decodes_as_refused() {
    let completion = responses::decode(&fixture("refusal.json")).unwrap();
    assert_eq!(
        completion.stop,
        StopReason::Refused("I can't help with that.".to_owned())
    );
}

#[test]
fn a_failed_response_is_an_error() {
    assert!(matches!(
        responses::decode(&fixture("failed.json")),
        Err(LlmError::Server { .. })
    ));
    assert!(matches!(
        responses::decode(&json!({
            "status": "failed",
            "error": {"code": "invalid_prompt", "message": "no"},
        })),
        Err(LlmError::BadRequest { .. })
    ));
    assert!(matches!(
        responses::decode(&json!({
            "status": "failed",
            "error": {"code": "rate_limit_exceeded", "message": "slow down"},
        })),
        Err(LlmError::RateLimited { retry_after: None })
    ));
    assert!(matches!(
        responses::decode(&json!({
            "status": "failed",
            "error": {"code": "vector_store_timeout", "message": "too slow"},
        })),
        Err(LlmError::Server { .. })
    ));
    assert!(matches!(
        responses::decode(&json!({"error": "a proxy's own"})),
        Err(LlmError::Decode(why)) if why.contains("a proxy's own")
    ));
}

/// A request whose input is exactly `items`.
fn input_is(items: Value) -> impl wiremock::Match {
    move |request: &wiremock::Request| {
        serde_json::from_slice::<Value>(&request.body).is_ok_and(|body| body["input"] == items)
    }
}

#[tokio::test]
async fn reasoning_is_carried_into_the_next_turn() {
    let server = MockServer::start().await;
    let turn_one = fixture("reasoning_tool_call.json");
    let question =
        json!({"role": "user", "content": [{"type": "input_text", "text": "Title of #7?"}]});
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer secret-key"))
        .and(input_is(json!([question])))
        .respond_with(ResponseTemplate::new(200).set_body_json(&turn_one))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(input_is(json!([
            question,
            turn_one["output"][0],
            {"type": "function_call", "call_id": "call_1", "name": "get_pull_request", "arguments": "{\"owner\":\"o\"}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "{\"title\": \"Fix\"}"},
        ])))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("text.json")))
        .expect(1)
        .mount(&server)
        .await;

    let client = client_for(config(&format!("{}/v1", server.uri()))).unwrap();
    let mut request = CompletionRequest {
        messages: vec![ChatMessage::user("Title of #7?")],
        tools: vec![tool()],
        ..Default::default()
    };
    let first = client.complete(&request).await.unwrap();
    assert_eq!(first.stop, StopReason::ToolUse);
    request.messages.push(first.message);
    request
        .messages
        .push(result("call_1", "{\"title\": \"Fix\"}", false));
    let second = client.complete(&request).await.unwrap();
    assert_eq!(second.message.text(), "Looks good.");
}

#[tokio::test]
async fn a_rate_limit_is_retried_and_a_bad_request_is_not() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("text.json")))
        .mount(&server)
        .await;
    let client = client_for(config(&format!("{}/v1", server.uri()))).unwrap();
    let request = CompletionRequest {
        messages: vec![ChatMessage::user("hi")],
        ..Default::default()
    };
    assert_eq!(
        client.complete(&request).await.unwrap().message.text(),
        "Looks good."
    );

    let refusing = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(400).set_body_string("{\"error\": \"bad\"}"))
        .expect(1)
        .mount(&refusing)
        .await;
    let client = client_for(config(&format!("{}/v1", refusing.uri()))).unwrap();
    assert!(matches!(
        client.complete(&request).await,
        Err(LlmError::BadRequest { status: 400, .. })
    ));
}

#[tokio::test]
async fn a_rate_limit_in_the_body_of_a_200_is_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "failed",
            "error": {"code": "rate_limit_exceeded", "message": "slow down"},
            "output": [],
        })))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("text.json")))
        .expect(1)
        .mount(&server)
        .await;
    let client = client_for(config(&format!("{}/v1", server.uri()))).unwrap();
    let completion = client
        .complete(&CompletionRequest {
            messages: vec![ChatMessage::user("hi")],
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(completion.message.text(), "Looks good.");
}

#[test]
fn the_responses_api_is_openai_s_only() {
    let mut anthropic = config("https://api.test");
    anthropic.provider = Provider::Anthropic;
    assert!(matches!(
        client_for(anthropic),
        Err(LlmError::InvalidConfig(_))
    ));
}
