//! Fixture and wiremock tests for both adapters.

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    missing_docs
)]

use std::time::Duration;

use henk_llm::{
    Block, ChatMessage, CompletionRequest, LlmError, MaxTokensParam, ModelConfig, Provider,
    RetryPolicy, Role, StopReason, ToolArguments, ToolChoice, ToolDef, ToolName, ToolResult,
    anthropic, client_for, openai,
};
use serde_json::{Value, json};
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    serde_json::from_str(&text).unwrap()
}

fn config(provider: Provider, base_url: &str) -> ModelConfig {
    ModelConfig {
        provider,
        base_url: base_url.to_owned(),
        api_key: "secret-key".to_owned().into(),
        model: "test-model".to_owned(),
        max_tokens: 1024,
        timeout: Duration::from_secs(5),
        retry: RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
        },
        max_tokens_param: MaxTokensParam::MaxTokens,
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

fn conversation() -> Vec<ChatMessage> {
    vec![
        ChatMessage::user("Review this."),
        ChatMessage {
            role: Role::Assistant,
            blocks: vec![
                Block::Opaque(json!({"type": "thinking", "thinking": "hm", "signature": "s"})),
                Block::ToolCall(henk_llm::ToolCall {
                    id: "call_1".to_owned(),
                    name: "get_pull_request".to_owned(),
                    arguments: ToolArguments::Parsed(json!({"owner": "o"})),
                }),
            ],
        },
        ChatMessage::tool_results([ToolResult {
            call_id: "call_1".to_owned(),
            content: "{\"title\": \"Fix\"}".to_owned(),
            is_error: false,
        }]),
    ]
}

// ---- OpenAI ----------------------------------------------------------------

#[test]
fn openai_decodes_tool_calls_including_malformed_arguments() {
    let completion = openai::decode(&fixture("openai/tool_call.json")).unwrap();
    assert_eq!(completion.stop, StopReason::ToolUse);
    assert_eq!(completion.usage.input_tokens, 120);
    let calls: Vec<_> = completion.message.tool_calls().collect();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].id, "call_abc");
    assert_eq!(
        calls[0].arguments,
        ToolArguments::Parsed(json!({"owner": "o", "repo": "r", "pullNumber": 7}))
    );
    assert_eq!(
        calls[1].arguments,
        ToolArguments::Malformed("{not json".to_owned())
    );
}

#[test]
fn openai_decodes_text() {
    let completion = openai::decode(&fixture("openai/text.json")).unwrap();
    assert_eq!(completion.stop, StopReason::EndTurn);
    assert_eq!(completion.message.text(), "Not bad.");
}

#[test]
fn openai_fills_in_missing_ids_and_object_arguments() {
    let completion = openai::decode(&fixture("openai/ollama_no_ids.json")).unwrap();
    assert_eq!(
        completion.stop,
        StopReason::ToolUse,
        "calls present means tool use"
    );
    let call = completion.message.tool_calls().next().unwrap();
    assert!(call.id.starts_with("call_0_"));
    assert_eq!(
        call.arguments,
        ToolArguments::Parsed(json!({"owner": "o", "repo": "r"}))
    );
}

#[test]
fn openai_encodes_the_conversation_in_wire_order() {
    let client = openai::OpenAiClient::new(config(Provider::OpenAi, "https://x.test/v1")).unwrap();
    let request = CompletionRequest {
        system: Some("You are Henk.".to_owned()),
        messages: conversation(),
        tools: vec![tool()],
        tool_choice: ToolChoice::Auto,
        max_tokens: None,
        temperature: Some(0.2),
    };
    let body = client.body(&request);
    assert_eq!(body["model"], "test-model");
    assert_eq!(body["max_tokens"], 1024);
    assert_eq!(body["temperature"], 0.2);
    assert_eq!(body["tool_choice"], "auto");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(
        messages[1],
        json!({"role": "user", "content": "Review this."})
    );
    assert_eq!(messages[2]["role"], "assistant");
    assert_eq!(messages[2]["content"], Value::Null);
    assert_eq!(
        messages[2]["tool_calls"][0]["function"]["arguments"],
        "{\"owner\":\"o\"}"
    );
    assert_eq!(
        messages[3],
        json!({"role": "tool", "tool_call_id": "call_1", "content": "{\"title\": \"Fix\"}"})
    );
    let parameters = &body["tools"][0]["function"]["parameters"];
    assert!(parameters.get("$schema").is_none(), "schema cleaned");
    assert_eq!(body["tools"][0]["function"]["name"], "get_pull_request");
}

#[tokio::test]
async fn openai_sends_bearer_and_retries_on_429_with_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer secret-key"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({"model": "test-model"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("openai/text.json")))
        .expect(1)
        .mount(&server)
        .await;

    let client = client_for(config(Provider::OpenAi, &format!("{}/v1/", server.uri()))).unwrap();
    let completion = client
        .complete(&CompletionRequest {
            messages: vec![ChatMessage::user("hi")],
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(completion.message.text(), "Not bad.");
}

#[tokio::test]
async fn openai_does_not_retry_400() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad tool schema"))
        .expect(1)
        .mount(&server)
        .await;
    let client = client_for(config(Provider::OpenAi, &format!("{}/v1", server.uri()))).unwrap();
    let error = client
        .complete(&CompletionRequest::default())
        .await
        .unwrap_err();
    assert!(
        matches!(error, LlmError::BadRequest { status: 400, ref body } if body == "bad tool schema")
    );
}

#[tokio::test]
async fn openai_gives_up_after_repeated_500() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(3)
        .mount(&server)
        .await;
    let client = client_for(config(Provider::OpenAi, &format!("{}/v1", server.uri()))).unwrap();
    let error = client
        .complete(&CompletionRequest::default())
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        LlmError::RetriesExhausted { attempts: 3, .. }
    ));
}

#[tokio::test]
async fn openai_401_is_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    let client = client_for(config(Provider::OpenAi, &format!("{}/v1", server.uri()))).unwrap();
    let error = client
        .complete(&CompletionRequest::default())
        .await
        .unwrap_err();
    assert!(matches!(error, LlmError::Unauthorized { status: 401, .. }));
}

// ---- Anthropic -------------------------------------------------------------

#[test]
fn anthropic_decodes_thinking_text_and_tool_use() {
    let completion = anthropic::decode(&fixture("anthropic/tool_use.json")).unwrap();
    assert_eq!(completion.stop, StopReason::ToolUse);
    assert_eq!(completion.usage.output_tokens, 40);
    assert!(matches!(&completion.message.blocks[0], Block::Opaque(v) if v["type"] == "thinking"));
    assert_eq!(completion.message.text(), "I will read the pull request.");
    let call = completion.message.tool_calls().next().unwrap();
    assert_eq!(call.id, "toolu_1");
    assert_eq!(
        call.arguments,
        ToolArguments::Parsed(json!({"owner": "o", "repo": "r", "pullNumber": 7}))
    );
}

#[test]
fn anthropic_encodes_system_top_level_and_echoes_thinking() {
    let client =
        anthropic::AnthropicClient::new(config(Provider::Anthropic, "https://x.test")).unwrap();
    let request = CompletionRequest {
        system: Some("You are Henk.".to_owned()),
        messages: conversation(),
        tools: vec![tool()],
        tool_choice: ToolChoice::Required,
        max_tokens: Some(256),
        temperature: None,
    };
    let body = client.body(&request);
    assert_eq!(body["system"], "You are Henk.");
    assert_eq!(body["max_tokens"], 256);
    assert_eq!(body["tool_choice"], json!({"type": "any"}));
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3, "user, assistant, user");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(
        messages[1]["content"][0]["type"], "thinking",
        "opaque block echoed first"
    );
    assert_eq!(messages[1]["content"][1]["type"], "tool_use");
    assert_eq!(messages[1]["content"][1]["input"], json!({"owner": "o"}));
    assert_eq!(messages[2]["content"][0]["type"], "tool_result");
    assert_eq!(messages[2]["content"][0]["tool_use_id"], "call_1");
    assert_eq!(messages[2]["content"][0]["is_error"], false);
    assert!(body["tools"][0]["input_schema"].get("$schema").is_none());
}

#[test]
fn anthropic_merges_consecutive_user_messages() {
    let client =
        anthropic::AnthropicClient::new(config(Provider::Anthropic, "https://x.test")).unwrap();
    let request = CompletionRequest {
        messages: vec![ChatMessage::user("a"), ChatMessage::user("b")],
        ..Default::default()
    };
    let body = client.body(&request);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["content"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn anthropic_sends_api_key_header_and_retries_529() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "secret-key"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(ResponseTemplate::new(529).set_body_string("{\"type\":\"error\"}"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("anthropic/text.json")))
        .expect(1)
        .mount(&server)
        .await;
    let client = client_for(config(Provider::Anthropic, &server.uri())).unwrap();
    let completion = client
        .complete(&CompletionRequest {
            messages: vec![ChatMessage::user("hi")],
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(completion.message.text(), "No issues found.");
    assert_eq!(completion.stop, StopReason::EndTurn);
}

#[test]
fn client_for_rejects_bad_config() {
    let mut bad = config(Provider::OpenAi, "x.test");
    assert!(matches!(
        client_for(bad.clone()),
        Err(LlmError::InvalidConfig(_))
    ));
    bad.base_url = "https://x.test".to_owned();
    bad.model = String::new();
    assert!(matches!(client_for(bad), Err(LlmError::InvalidConfig(_))));
}

/// Live smoke test. Set `LLM_SMOKE_BASE_URL`, `LLM_SMOKE_API_KEY`, `LLM_SMOKE_MODEL`
/// and optionally `LLM_SMOKE_PROVIDER=anthropic`, then run with `--ignored`.
#[tokio::test]
#[ignore = "needs a live endpoint"]
async fn live_smoke() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let base_url = std::env::var("LLM_SMOKE_BASE_URL").expect("LLM_SMOKE_BASE_URL");
    let api_key = std::env::var("LLM_SMOKE_API_KEY").expect("LLM_SMOKE_API_KEY");
    let model = std::env::var("LLM_SMOKE_MODEL").expect("LLM_SMOKE_MODEL");
    let provider = match std::env::var("LLM_SMOKE_PROVIDER").as_deref() {
        Ok("anthropic") => Provider::Anthropic,
        _ => Provider::OpenAi,
    };
    let mut cfg = config(provider, &base_url);
    cfg.api_key = api_key.into();
    cfg.model = model;
    cfg.timeout = Duration::from_secs(120);
    let client = client_for(cfg).unwrap();
    let request = CompletionRequest {
        system: Some(
            "You are a terse assistant. Use the tool when asked about a pull request.".to_owned(),
        ),
        messages: vec![ChatMessage::user(
            "What is the title of pull request 7 in o/r? Use the tool.",
        )],
        tools: vec![tool()],
        ..Default::default()
    };
    let first = client.complete(&request).await.unwrap();
    assert_eq!(first.stop, StopReason::ToolUse, "{first:?}");
    let call = first.message.tool_calls().next().unwrap().clone();
    let mut messages = request.messages.clone();
    messages.push(first.message.clone());
    messages.push(ChatMessage::tool_results([ToolResult {
        call_id: call.id,
        content: "{\"title\": \"Fix the thing\"}".to_owned(),
        is_error: false,
    }]));
    let second = client
        .complete(&CompletionRequest {
            messages,
            ..request
        })
        .await
        .unwrap();
    assert!(
        second.message.text().contains("Fix the thing"),
        "{second:?}"
    );
}
