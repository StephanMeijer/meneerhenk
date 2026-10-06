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
    Block, ChatMessage, CompletionRequest, Effort, LlmError, MaxTokensParam, ModelConfig, Provider,
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
        effort: None,
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
fn openai_reads_usage_under_either_naming() {
    let response = |usage: Value| {
        json!({
            "choices": [{"finish_reason": "stop", "message": {"role": "assistant", "content": "hi"}}],
            "usage": usage,
        })
    };
    let openai_style = openai::decode(&response(
        json!({"prompt_tokens": 7, "completion_tokens": 3}),
    ))
    .unwrap();
    assert_eq!(
        (
            openai_style.usage.input_tokens,
            openai_style.usage.output_tokens
        ),
        (7, 3)
    );
    let passed_through =
        openai::decode(&response(json!({"input_tokens": 9, "output_tokens": 4}))).unwrap();
    assert_eq!(
        (
            passed_through.usage.input_tokens,
            passed_through.usage.output_tokens
        ),
        (9, 4)
    );
}

#[test]
fn openai_treats_empty_arguments_as_an_empty_object() {
    let response = json!({
        "choices": [{
            "finish_reason": "tool_calls",
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {"id": "c1", "type": "function", "function": {"name": "list_existing_findings", "arguments": ""}},
                    {"id": "c2", "type": "function", "function": {"name": "list_existing_findings", "arguments": "  \n"}}
                ]
            }
        }]
    });
    let completion = openai::decode(&response).unwrap();
    let calls: Vec<_> = completion.message.tool_calls().collect();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].arguments, ToolArguments::Parsed(json!({})));
    assert_eq!(calls[1].arguments, ToolArguments::Parsed(json!({})));
}

#[test]
fn openai_decode_errors_carry_the_body() {
    let error = openai::decode(&json!({"error": {"message": "model not found"}})).unwrap_err();
    assert!(matches!(error, LlmError::Decode(_)));
    assert!(error.to_string().contains("model not found"), "{error}");
    let error = openai::decode(&json!({"choices": [{"delta": {}}]})).unwrap_err();
    assert!(error.to_string().contains("delta"), "{error}");
}

#[test]
fn tool_descriptions_are_capped_for_both_adapters() {
    let mut long = tool();
    long.description = "x".repeat(3000);
    let request = CompletionRequest {
        tools: vec![long],
        ..Default::default()
    };
    let openai_body = openai::OpenAiClient::new(config(Provider::OpenAi, "http://x/v1"))
        .unwrap()
        .body(&request);
    let description = openai_body["tools"][0]["function"]["description"]
        .as_str()
        .unwrap();
    assert!(description.chars().count() <= 1024, "{}", description.len());
    assert!(description.ends_with("[...]"));
    let anthropic_body = anthropic::AnthropicClient::new(config(Provider::Anthropic, "http://x"))
        .unwrap()
        .body(&request);
    let description = anthropic_body["tools"][0]["description"].as_str().unwrap();
    assert!(description.chars().count() <= 1024);

    let short = openai::OpenAiClient::new(config(Provider::OpenAi, "http://x/v1"))
        .unwrap()
        .body(&CompletionRequest {
            tools: vec![tool()],
            ..Default::default()
        });
    assert_eq!(
        short["tools"][0]["function"]["description"],
        "Reads a pull request."
    );
}

#[test]
fn transcripts_serialise() {
    let text = serde_json::to_string(&conversation()).unwrap();
    assert!(text.contains("\"tool_call\""), "{text}");
    assert!(text.contains("\"parsed\""), "{text}");
    assert!(text.contains("\"tool_result\""), "{text}");
}

#[tokio::test]
async fn list_models_openai_and_anthropic() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer secret-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{"id": "fast-model", "object": "model"}, {"id": "big-model"}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let ids = henk_llm::list_models(&config(Provider::OpenAi, &format!("{}/v1", server.uri())))
        .await
        .unwrap();
    assert_eq!(ids, vec!["big-model", "fast-model"]);

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("x-api-key", "secret-key"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "claude-sonnet-5-5", "type": "model"}],
            "has_more": false
        })))
        .expect(1)
        .mount(&server)
        .await;
    let ids = henk_llm::list_models(&config(Provider::Anthropic, &server.uri()))
        .await
        .unwrap();
    assert_eq!(ids, vec!["claude-sonnet-5-5"]);

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401).set_body_string("no key"))
        .expect(1)
        .mount(&server)
        .await;
    let error = henk_llm::list_models(&config(Provider::OpenAi, &format!("{}/v1", server.uri())))
        .await
        .unwrap_err();
    assert!(matches!(error, LlmError::Unauthorized { status: 401, .. }));
}

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
fn anthropic_sends_adaptive_thinking_and_effort_and_no_temperature() {
    let mut cfg = config(Provider::Anthropic, "https://x.test");
    cfg.effort = Some(Effort::High);
    let client = anthropic::AnthropicClient::new(cfg).unwrap();
    let request = CompletionRequest {
        messages: vec![ChatMessage::user("a")],
        temperature: Some(0.2),
        ..Default::default()
    };
    let body = client.body(&request);
    assert_eq!(body["thinking"], json!({"type": "adaptive"}));
    assert_eq!(body["output_config"], json!({"effort": "high"}));
    assert!(body.get("temperature").is_none());
}

#[test]
fn anthropic_without_effort_sends_no_thinking_settings() {
    let client =
        anthropic::AnthropicClient::new(config(Provider::Anthropic, "https://x.test")).unwrap();
    let request = CompletionRequest {
        messages: vec![ChatMessage::user("a")],
        temperature: Some(0.2),
        ..Default::default()
    };
    let body = client.body(&request);
    assert!(body.get("thinking").is_none());
    assert!(body.get("output_config").is_none());
    assert_eq!(body["temperature"], 0.2);
}

#[test]
fn effort_is_refused_on_openai_style_endpoints() {
    let mut cfg = config(Provider::OpenAi, "https://x.test/v1");
    cfg.effort = Some(Effort::Low);
    assert!(matches!(client_for(cfg), Err(LlmError::InvalidConfig(_))));
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
    cfg.timeout = Duration::from_mins(2);
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

#[test]
fn a_refusal_or_content_filter_decodes_as_refused() {
    let anthropic = anthropic::decode(&json!({
        "content": [], "stop_reason": "refusal", "usage": {"input_tokens": 9, "output_tokens": 0}
    }))
    .unwrap();
    assert_eq!(anthropic.stop, StopReason::Refused("refusal".to_owned()));
    let openai = openai::decode(&json!({
        "choices": [{"finish_reason": "content_filter", "message": {"role": "assistant", "content": null}}],
        "usage": {"prompt_tokens": 9, "completion_tokens": 0}
    }))
    .unwrap();
    assert_eq!(
        openai.stop,
        StopReason::Refused("content_filter".to_owned())
    );
    let unknown = anthropic::decode(&json!({
        "content": [{"type": "text", "text": "..."}], "stop_reason": "pause_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    }))
    .unwrap();
    assert_eq!(
        unknown.stop,
        StopReason::Other("pause_turn".to_owned()),
        "unknown stays unknown"
    );
}
