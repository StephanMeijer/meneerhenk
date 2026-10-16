//! The Responses adapter paid by a ChatGPT plan (#620): a streamed reply,
//! the parameters OpenAI does not take there, the plan's own errors, and a
//! bearer asked for on every attempt. Against wiremock streams.

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    missing_docs
)]

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use henk_llm::{
    Api, Block, ChatMessage, CompletionRequest, LlmError, MaxTokensParam, ModelConfig, Provider,
    RetryPolicy, StopReason, TokenSource, ToolChoice, ToolDef, ToolName, client_for,
};
use secrecy::{ExposeSecret as _, SecretString};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// A sign-in that hands out the tokens it was given, in order, and notes
/// which ones were refused. `rotate` moves to the next token at every
/// request (a renewal that happened in between); otherwise a refusal does.
struct Source {
    tokens: Vec<&'static str>,
    rotate: bool,
    state: Mutex<(usize, Vec<String>)>,
}

impl Source {
    fn new(tokens: &[&'static str], rotate: bool) -> Arc<Self> {
        Arc::new(Self {
            tokens: tokens.to_vec(),
            rotate,
            state: Mutex::new((0, Vec::new())),
        })
    }

    fn refused(&self) -> Vec<String> {
        self.state.lock().unwrap().1.clone()
    }
}

#[async_trait::async_trait]
impl TokenSource for Source {
    async fn bearer(&self) -> Result<SecretString, LlmError> {
        let mut state = self.state.lock().unwrap();
        let at = state.0.min(self.tokens.len() - 1);
        if self.rotate {
            state.0 += 1;
        }
        Ok(self.tokens[at].to_owned().into())
    }

    async fn rejected(&self, token: &SecretString) {
        let mut state = self.state.lock().unwrap();
        state.1.push(token.expose_secret().to_owned());
        if !self.rotate {
            state.0 += 1;
        }
    }
}

fn config(base_url: &str, source: &Arc<Source>) -> ModelConfig {
    ModelConfig {
        provider: Provider::OpenAi,
        api: Api::Responses,
        base_url: base_url.to_owned(),
        api_key: String::new().into(),
        token_source: Some(Arc::clone(source) as Arc<dyn TokenSource>),
        model: "gpt-6.1-sol".to_owned(),
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
        input_schema: json!({"type": "object", "properties": {"owner": {"type": "string"}}}),
    }
}

fn request() -> CompletionRequest {
    CompletionRequest {
        system: Some("Be terse.".to_owned()),
        messages: vec![ChatMessage::user("Review this.")],
        tools: vec![tool()],
        tool_choice: ToolChoice::Auto,
        max_tokens: Some(2048),
        temperature: Some(0.2),
        ..Default::default()
    }
}

fn fixture(name: &str) -> Value {
    let path = format!(
        "{}/tests/fixtures/openai_responses/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// A stream of these events, each as `event:` and `data:`.
fn stream(events: &[Value]) -> String {
    let mut out = String::new();
    for event in events {
        let kind = event["type"].as_str().unwrap();
        writeln!(out, "event: {kind}\ndata: {event}\n").unwrap();
    }
    out
}

fn sse(events: &[Value]) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(stream(events))
}

/// The events of a reply that goes well and ends in `response`.
fn completed(response: &Value) -> Vec<Value> {
    vec![
        json!({"type": "response.created", "response": {"id": "resp_1", "status": "in_progress"}}),
        json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "reasoning"}}),
        json!({"type": "response.reasoning_summary_text.delta", "delta": "thinking"}),
        json!({"type": "response.output_text.delta", "delta": "par"}),
        json!({"type": "response.completed", "response": response.clone()}),
    ]
}

fn failed(code: &str) -> Vec<Value> {
    vec![
        json!({"type": "response.created", "response": {"id": "resp_1", "status": "in_progress"}}),
        json!({"type": "response.output_text.delta", "delta": "par"}),
        json!({"type": "response.failed", "response": {
            "id": "resp_1",
            "status": "failed",
            "error": {"code": code, "message": "no"},
        }}),
    ]
}

async fn requests(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.unwrap()
}

fn bearer_of(request: &Request) -> String {
    request
        .headers
        .get("authorization")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn a_turn_is_streamed_and_sends_none_of_the_parameters_a_plan_refuses() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer plan-token"))
        .respond_with(sse(&completed(&fixture("reasoning_tool_call.json"))))
        .mount(&server)
        .await;
    let source = Source::new(&["plan-token"], false);
    let client = client_for(config(&format!("{}/v1", server.uri()), &source)).unwrap();

    let completion = client.complete(&request()).await.unwrap();

    assert_eq!(completion.stop, StopReason::ToolUse);
    assert!(matches!(completion.message.blocks[0], Block::Opaque(_)));
    assert!(
        matches!(&completion.message.blocks[1], Block::ToolCall(c) if c.name == "get_pull_request")
    );
    assert_eq!(completion.usage.input_tokens, 300);
    assert_eq!(completion.usage.output_tokens, 90);
    assert_eq!(completion.usage.reasoning_tokens, 64);

    let sent: Value = serde_json::from_slice(&requests(&server).await[0].body).unwrap();
    assert_eq!(sent["stream"], true);
    assert_eq!(sent["store"], false);
    assert_eq!(sent["model"], "gpt-6.1-sol");
    assert_eq!(sent["instructions"], "Be terse.");
    assert_eq!(sent["include"], json!(["reasoning.encrypted_content"]));
    // What OpenAI lists as unsupported with a plan.
    for param in [
        "max_output_tokens",
        "temperature",
        "previous_response_id",
        "user",
        "metadata",
        "background",
        "conversation",
        "max_tool_calls",
        "top_p",
        "truncation",
        "safety_identifier",
    ] {
        assert!(
            sent.get(param).is_none(),
            "{param} must not be sent: {sent}"
        );
    }
    assert!(
        sent["input"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["role"] != "system"),
        "the system prompt goes as instructions"
    );
    assert_eq!(sent["tools"][0]["type"], "function");
}

#[tokio::test]
async fn an_endpoint_with_a_key_still_sends_the_cap_and_does_not_stream() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer secret-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture("text.json")))
        .mount(&server)
        .await;
    let source = Source::new(&["unused"], false);
    let mut keyed = config(&format!("{}/v1", server.uri()), &source);
    keyed.token_source = None;
    keyed.api_key = "secret-key".to_owned().into();

    client_for(keyed)
        .unwrap()
        .complete(&request())
        .await
        .unwrap();

    let sent: Value = serde_json::from_slice(&requests(&server).await[0].body).unwrap();
    assert_eq!(sent["max_output_tokens"], 2048);
    assert!(sent.get("stream").is_none());
}

#[tokio::test]
async fn a_sign_in_is_for_the_responses_api_only() {
    let source = Source::new(&["t"], false);
    for (provider, api) in [
        (Provider::OpenAi, Api::ChatCompletions),
        (Provider::Anthropic, Api::ChatCompletions),
    ] {
        let mut config = config("https://api.test/v1", &source);
        config.provider = provider;
        config.api = api;
        assert!(
            matches!(client_for(config), Err(LlmError::InvalidConfig(_))),
            "{provider:?} over {api:?}"
        );
    }
}

#[tokio::test]
async fn the_usage_limit_in_a_429_stops_at_once_and_invents_no_wait() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "error": {"code": "subscription_sharing_usage_limit_exceeded", "message": "limit"}
        })))
        .mount(&server)
        .await;
    let source = Source::new(&["t"], false);
    let client = client_for(config(&format!("{}/v1", server.uri()), &source)).unwrap();

    let error = client.complete(&request()).await.unwrap_err();

    assert!(matches!(error, LlmError::PlanUsageExhausted), "{error:?}");
    assert!(!error.is_retryable());
    assert_eq!(requests(&server).await.len(), 1, "not asked again");
    assert!(source.refused().is_empty());
}

#[tokio::test]
async fn any_other_429_is_still_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(429)
                .set_body_json(json!({"error": {"code": "rate_limit_exceeded"}})),
        )
        .mount(&server)
        .await;
    let source = Source::new(&["t"], false);
    let client = client_for(config(&format!("{}/v1", server.uri()), &source)).unwrap();

    let error = client.complete(&request()).await.unwrap_err();

    assert!(
        matches!(&error, LlmError::RetriesExhausted { attempts: 3, last } if matches!(**last, LlmError::RateLimited { .. })),
        "{error:?}"
    );
    assert_eq!(requests(&server).await.len(), 3);
}

#[tokio::test]
async fn an_account_that_may_not_use_its_plan_is_told_so_and_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "error": {"code": "subscription_sharing_user_not_eligible", "message": "no"}
        })))
        .mount(&server)
        .await;
    let source = Source::new(&["t"], false);
    let client = client_for(config(&format!("{}/v1", server.uri()), &source)).unwrap();

    let error = client.complete(&request()).await.unwrap_err();

    assert!(matches!(error, LlmError::PlanNotEligible), "{error:?}");
    assert_eq!(requests(&server).await.len(), 1);
    assert!(source.refused().is_empty(), "a new sign-in would not help");
}

#[tokio::test]
async fn another_403_is_an_unauthorized_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
        .mount(&server)
        .await;
    let source = Source::new(&["t"], false);
    let client = client_for(config(&format!("{}/v1", server.uri()), &source)).unwrap();

    let error = client.complete(&request()).await.unwrap_err();

    assert!(
        matches!(error, LlmError::Unauthorized { status: 403, .. }),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_refused_token_is_renewed_and_the_turn_sent_again_once() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer old"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": {"code": "subscription_sharing_invalid_user"}
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer new"))
        .respond_with(sse(&completed(&fixture("text.json"))))
        .mount(&server)
        .await;
    let source = Source::new(&["old", "new"], false);
    let client = client_for(config(&format!("{}/v1", server.uri()), &source)).unwrap();

    client.complete(&request()).await.unwrap();

    let sent: Vec<String> = requests(&server).await.iter().map(bearer_of).collect();
    assert_eq!(sent, ["Bearer old", "Bearer new"]);
    assert_eq!(source.refused(), ["old"]);
}

#[tokio::test]
async fn a_token_refused_twice_fails_after_one_renewal() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_string("no"))
        .mount(&server)
        .await;
    let source = Source::new(&["old", "new"], false);
    let client = client_for(config(&format!("{}/v1", server.uri()), &source)).unwrap();

    let error = client.complete(&request()).await.unwrap_err();

    assert!(
        matches!(error, LlmError::Unauthorized { status: 401, .. }),
        "{error:?}"
    );
    assert_eq!(requests(&server).await.len(), 2);
    assert_eq!(source.refused(), ["old"]);
}

#[tokio::test]
async fn the_bearer_is_asked_for_on_every_attempt() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer first"))
        .respond_with(ResponseTemplate::new(503).set_body_string("busy"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(header("authorization", "Bearer second"))
        .respond_with(sse(&completed(&fixture("text.json"))))
        .mount(&server)
        .await;
    let source = Source::new(&["first", "second"], true);
    let client = client_for(config(&format!("{}/v1", server.uri()), &source)).unwrap();

    client.complete(&request()).await.unwrap();

    let sent: Vec<String> = requests(&server).await.iter().map(bearer_of).collect();
    assert_eq!(sent, ["Bearer first", "Bearer second"]);
}

#[tokio::test]
async fn a_sign_in_that_is_gone_fails_the_call_without_calling_openai() {
    struct Gone;
    #[async_trait::async_trait]
    impl TokenSource for Gone {
        async fn bearer(&self) -> Result<SecretString, LlmError> {
            Err(LlmError::SignedOut {
                endpoint: "chatgpt".to_owned(),
                reason: "invalid_grant".to_owned(),
            })
        }
        async fn rejected(&self, _: &SecretString) {}
    }
    let server = MockServer::start().await;
    let source = Source::new(&["t"], false);
    let mut gone = config(&format!("{}/v1", server.uri()), &source);
    gone.token_source = Some(Arc::new(Gone));

    let error = client_for(gone)
        .unwrap()
        .complete(&request())
        .await
        .unwrap_err();

    assert!(matches!(error, LlmError::SignedOut { .. }), "{error:?}");
    assert!(requests(&server).await.is_empty());
}

#[tokio::test]
async fn the_plan_running_out_mid_stream_is_the_same_error_and_is_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse(&failed("subscription_sharing_usage_limit_exceeded")))
        .mount(&server)
        .await;
    let source = Source::new(&["t"], false);
    let client = client_for(config(&format!("{}/v1", server.uri()), &source)).unwrap();

    let error = client.complete(&request()).await.unwrap_err();

    assert!(matches!(error, LlmError::PlanUsageExhausted), "{error:?}");
    assert_eq!(requests(&server).await.len(), 1);
}

#[tokio::test]
async fn usage_that_cannot_be_told_mid_stream_is_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse(&failed("subscription_sharing_usage_unavailable")))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(sse(&completed(&fixture("text.json"))))
        .mount(&server)
        .await;
    let source = Source::new(&["t"], false);
    let client = client_for(config(&format!("{}/v1", server.uri()), &source)).unwrap();

    client.complete(&request()).await.unwrap();

    assert_eq!(requests(&server).await.len(), 2);
}

#[tokio::test]
async fn a_request_the_plan_refuses_mid_stream_is_the_requests_fault() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse(&failed("invalid_prompt")))
        .mount(&server)
        .await;
    let source = Source::new(&["t"], false);
    let client = client_for(config(&format!("{}/v1", server.uri()), &source)).unwrap();

    let error = client.complete(&request()).await.unwrap_err();

    assert!(matches!(error, LlmError::BadRequest { .. }), "{error:?}");
    assert_eq!(requests(&server).await.len(), 1);
}

#[tokio::test]
async fn an_error_event_ends_the_turn() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse(&[
            json!({"type": "response.created", "response": {"id": "r"}}),
            json!({"type": "error", "code": "server_error", "message": "boom"}),
        ]))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(sse(&completed(&fixture("text.json"))))
        .mount(&server)
        .await;
    let source = Source::new(&["t"], false);
    let client = client_for(config(&format!("{}/v1", server.uri()), &source)).unwrap();

    client.complete(&request()).await.unwrap();

    assert_eq!(requests(&server).await.len(), 2, "server_error is retried");
}

#[tokio::test]
async fn a_stream_cut_before_completion_is_sent_again_and_then_given_up_on() {
    let server = MockServer::start().await;
    let cut = completed(&fixture("text.json"))[..4].to_vec();
    Mock::given(method("POST"))
        .respond_with(sse(&cut))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(sse(&completed(&fixture("text.json"))))
        .mount(&server)
        .await;
    let source = Source::new(&["t"], false);
    let client = client_for(config(&format!("{}/v1", server.uri()), &source)).unwrap();
    client.complete(&request()).await.unwrap();
    assert_eq!(requests(&server).await.len(), 2);

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse(&cut))
        .mount(&server)
        .await;
    let client = client_for(config(&format!("{}/v1", server.uri()), &source)).unwrap();
    let error = client.complete(&request()).await.unwrap_err();
    assert!(
        matches!(&error, LlmError::RetriesExhausted { attempts: 3, last } if matches!(**last, LlmError::StreamInterrupted(_))),
        "{error:?}"
    );
}

#[tokio::test]
async fn an_incomplete_response_carries_its_stop_reason() {
    let server = MockServer::start().await;
    let mut cut = fixture("incomplete.json");
    cut["incomplete_details"] = json!({"reason": "content_filter"});
    Mock::given(method("POST"))
        .respond_with(sse(&[
            json!({"type": "response.created", "response": {"id": "r"}}),
            json!({"type": "response.incomplete", "response": cut}),
        ]))
        .mount(&server)
        .await;
    let source = Source::new(&["t"], false);
    let client = client_for(config(&format!("{}/v1", server.uri()), &source)).unwrap();

    let completion = client.complete(&request()).await.unwrap();

    assert!(
        matches!(&completion.stop, StopReason::Refused(why) if why == "content_filter"),
        "{:?}",
        completion.stop
    );
}

#[tokio::test]
async fn an_answer_that_is_not_an_event_stream_is_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>a proxy's page</html>"))
        .mount(&server)
        .await;
    let source = Source::new(&["t"], false);
    let client = client_for(config(&format!("{}/v1", server.uri()), &source)).unwrap();

    let error = client.complete(&request()).await.unwrap_err();

    assert!(
        matches!(&error, LlmError::Decode(why) if why.contains("a proxy's page")),
        "{error:?}"
    );
    assert_eq!(requests(&server).await.len(), 1);
}
