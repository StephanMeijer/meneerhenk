//! The OpenAI Responses adapter: `POST {base_url}/responses` (#365).
//!
//! Stateless: every request sends `store: false` and the whole
//! conversation, and asks for the model's reasoning back encrypted
//! (`include: ["reasoning.encrypted_content"]`). Each turn's reasoning
//! items come back as [`Block::Opaque`] and are sent again, unchanged and
//! in their place, so a reasoning model keeps its chain of thought across
//! tool calls while nothing is kept at OpenAI.
//!
//! With a [`TokenSource`] in the configuration the client is paid by a
//! ChatGPT plan instead of an API key (#620), and OpenAI's rules for that
//! differ: the reply is streamed (a request counts as done at
//! `response.completed`), there is no output cap or temperature to send, and
//! the plan's own limits come back as their own errors.

use secrecy::ExposeSecret as _;
use serde_json::{Map, Value, json};
use tracing::instrument;

use crate::client::{ModelClient, ModelConfig, Provider, TokenSource};
use crate::error::{LlmError, truncate_body};
use crate::http::{build_endpoint_client, error_for_status, post_json, prepared, with_retry};
use crate::openai::{decode_usage, effort_rejected};
use crate::schema;
use crate::sse::EventStream;
use crate::types::{
    Block, ChatMessage, Completion, CompletionRequest, Role, StopReason, ToolArguments, ToolCall,
    ToolChoice, Usage,
};

/// The code of a 429 or a failed response when the plan has no usage left.
const USAGE_LIMIT: &str = "subscription_sharing_usage_limit_exceeded";
/// The code of a failed response when OpenAI cannot tell the plan's usage.
const USAGE_UNAVAILABLE: &str = "subscription_sharing_usage_unavailable";
/// The code of a 403 when the account may not use its plan through an app.
const NOT_ELIGIBLE: &str = "subscription_sharing_user_not_eligible";
/// The code of a 401 when the sign-in names no valid user.
const INVALID_USER: &str = "subscription_sharing_invalid_user";

/// A client for one model over the Responses API.
#[derive(Debug)]
pub struct ResponsesClient {
    config: ModelConfig,
    http: reqwest::Client,
    url: String,
}

impl ResponsesClient {
    /// Builds the client.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::InvalidConfig`] when the HTTP client cannot be built.
    pub fn new(config: ModelConfig) -> Result<Self, LlmError> {
        Ok(Self::with_http(config, build_endpoint_client()?))
    }

    /// The client over `http`, the HTTP client its endpoint's models share.
    #[must_use]
    pub fn with_http(config: ModelConfig, http: reqwest::Client) -> Self {
        let url = format!("{}/responses", config.base_url());
        Self { config, http, url }
    }

    /// The request body for `request`. Public for fixture tests.
    #[must_use]
    pub fn body(&self, request: &CompletionRequest) -> Value {
        let mut input = Vec::new();
        for message in &request.messages {
            encode_message(message, &mut input);
        }
        let mut body = Map::new();
        body.insert("model".into(), Value::String(self.config.model.clone()));
        if let Some(system) = &request.system {
            body.insert("instructions".into(), Value::String(system.clone()));
        }
        body.insert("input".into(), Value::Array(input));
        // A ChatGPT plan takes no output cap and no sampling parameters:
        // OpenAI lists them as unsupported, and a request that sends one is
        // refused.
        let plan = self.config.token_source.is_some();
        if !plan {
            body.insert(
                "max_output_tokens".into(),
                json!(request.max_tokens.unwrap_or(self.config.max_tokens)),
            );
        }
        if let Some(effort) = self
            .config
            .effort
            .and_then(|effort| effort.wire(Provider::OpenAi))
        {
            // Reasoning models reject sampling parameters, so temperature
            // is never sent alongside effort.
            body.insert("reasoning".into(), json!({"effort": effort}));
        } else if let Some(temperature) = request.temperature
            && !plan
        {
            body.insert("temperature".into(), json!(temperature));
        }
        body.insert("store".into(), json!(false));
        if plan {
            body.insert("stream".into(), json!(true));
        }
        body.insert("include".into(), json!(["reasoning.encrypted_content"]));
        if !request.tools.is_empty() {
            let tools: Vec<Value> = request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "name": tool.name.as_str(),
                        "description": schema::cap_description(&tool.description),
                        "parameters": schema::clean(&tool.input_schema),
                    })
                })
                .collect();
            body.insert("tools".into(), Value::Array(tools));
            body.insert(
                "tool_choice".into(),
                match &request.tool_choice {
                    ToolChoice::Auto => json!("auto"),
                    ToolChoice::None => json!("none"),
                    ToolChoice::Required => json!("required"),
                    ToolChoice::Named(name) => json!({"type": "function", "name": name.as_str()}),
                },
            );
        }
        Value::Object(body)
    }
}

/// Whether an opaque block is one of this API's reasoning items. Other
/// providers' opaque content (a chat completion's `reasoning_content`,
/// Anthropic thinking) means nothing here and is not sent.
fn is_reasoning_item(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("reasoning")
}

/// One message as input items, in block order.
fn encode_message(message: &ChatMessage, out: &mut Vec<Value>) {
    match message.role {
        Role::User => {
            let mut text = String::new();
            for block in &message.blocks {
                match block {
                    Block::ToolResult(result) => {
                        let output = if result.is_error {
                            format!("Error: {}", result.content)
                        } else {
                            result.content.clone()
                        };
                        out.push(json!({
                            "type": "function_call_output",
                            "call_id": result.call_id,
                            "output": output,
                        }));
                    }
                    Block::Text(part) => {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(part);
                    }
                    Block::ToolCall(_) | Block::Opaque(_) => {}
                }
            }
            if !text.is_empty() {
                out.push(json!({
                    "role": "user",
                    "content": [{"type": "input_text", "text": text}],
                }));
            }
        }
        Role::Assistant => {
            let start = out.len();
            for block in &message.blocks {
                match block {
                    Block::Opaque(item) if is_reasoning_item(item) => out.push(item.clone()),
                    Block::Text(text) if !text.is_empty() => out.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": text}],
                    })),
                    Block::ToolCall(call) => {
                        out.push(json!({
                            "type": "function_call",
                            "call_id": call.id,
                            "name": call.name,
                            "arguments": call.arguments.text(),
                        }));
                    }
                    Block::Opaque(_) | Block::Text(_) | Block::ToolResult(_) => {}
                }
            }
            // A reasoning item must be followed by the item it produced;
            // the API refuses one that is not. A turn cut at the output cap
            // while thinking holds only reasoning, and a reasoning item
            // with nothing after it is of no use to the model anyway.
            while out.len() > start && out.last().is_some_and(is_reasoning_item) {
                out.pop();
            }
        }
    }
}

/// The error of a failed response, from its `error` object.
///
/// A failed response arrives with HTTP 200, so the status-based mapping of
/// `post_json` never sees it. The transient codes are mapped to the errors a
/// real 429 or 5xx would give, so they are retried; the plan's own codes
/// (#620) to the errors a plan gives over HTTP; any other code is the
/// request's own fault.
fn failure(error: Option<&Value>) -> LlmError {
    let code = error
        .and_then(|e| e.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let message = error
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("the response failed");
    let message = format!("{code}: {message}");
    match code {
        USAGE_LIMIT => LlmError::PlanUsageExhausted,
        NOT_ELIGIBLE => LlmError::PlanNotEligible,
        INVALID_USER => LlmError::Unauthorized {
            status: 401,
            body: message,
        },
        "rate_limit_exceeded" => LlmError::RateLimited { retry_after: None },
        "server_error" | "vector_store_timeout" | USAGE_UNAVAILABLE => LlmError::Server {
            status: 500,
            body: message,
        },
        _ => LlmError::BadRequest {
            status: 400,
            body: message,
        },
    }
}

/// Turns a Responses API response into a [`Completion`]. Public for
/// fixture tests.
///
/// # Errors
///
/// Returns [`LlmError::RateLimited`], [`LlmError::Server`] or
/// [`LlmError::BadRequest`] for a response whose `status` is `failed`
/// (the first two are retried), and [`LlmError::Decode`] when it has no
/// `output` or an unreadable item.
pub fn decode(response: &Value) -> Result<Completion, LlmError> {
    let status = response.get("status").and_then(Value::as_str);
    if status == Some("failed") {
        return Err(failure(response.get("error")));
    }
    let output = response
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            LlmError::Decode(format!(
                "no output in response: {}",
                truncate_body(&response.to_string())
            ))
        })?;

    let mut blocks = Vec::new();
    let mut refusal = None;
    for item in output {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                for part in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    match part.get("type").and_then(Value::as_str) {
                        Some("output_text") => {
                            if let Some(text) = part.get("text").and_then(Value::as_str)
                                && !text.is_empty()
                            {
                                blocks.push(Block::Text(text.to_owned()));
                            }
                        }
                        Some("refusal") => {
                            refusal = Some(
                                part.get("refusal")
                                    .and_then(Value::as_str)
                                    .unwrap_or("refusal")
                                    .to_owned(),
                            );
                        }
                        _ => {}
                    }
                }
            }
            Some("function_call") => blocks.push(Block::ToolCall(decode_call(item)?)),
            // Reasoning, and anything new: echoed back as it came.
            _ => blocks.push(Block::Opaque(item.clone())),
        }
    }

    let had_calls = blocks.iter().any(|b| matches!(b, Block::ToolCall(_)));
    let incomplete = response
        .pointer("/incomplete_details/reason")
        .and_then(Value::as_str);
    let stop = match (refusal, status, incomplete) {
        (Some(why), _, _) => StopReason::Refused(why),
        (None, Some("incomplete"), Some("max_output_tokens")) => StopReason::MaxTokens,
        (None, Some("incomplete"), Some("content_filter")) => {
            StopReason::Refused("content_filter".to_owned())
        }
        (None, Some("incomplete"), other) => {
            StopReason::Other(format!("incomplete: {}", other.unwrap_or("unknown")))
        }
        (None, _, _) if had_calls => StopReason::ToolUse,
        (None, _, _) => StopReason::EndTurn,
    };
    let usage = response.get("usage").map_or_else(
        || {
            tracing::debug!("response carries no usage; counting zero tokens");
            Usage::default()
        },
        decode_usage,
    );
    Ok(Completion {
        message: ChatMessage {
            role: Role::Assistant,
            blocks,
        },
        stop,
        usage,
    })
}

fn decode_call(item: &Value) -> Result<ToolCall, LlmError> {
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| LlmError::Decode("function call without name".to_owned()))?
        .to_owned();
    let id = item
        .get("call_id")
        .and_then(Value::as_str)
        .ok_or_else(|| LlmError::Decode(format!("function call {name} without call_id")))?
        .to_owned();
    let arguments = ToolArguments::from_wire(item.get("arguments"));
    Ok(ToolCall {
        id,
        name,
        arguments,
    })
}

#[async_trait::async_trait]
impl ModelClient for ResponsesClient {
    fn model(&self) -> &str {
        &self.config.model
    }

    #[instrument(skip_all, fields(model = %self.config.model))]
    async fn complete(&self, request: &CompletionRequest) -> Result<Completion, LlmError> {
        let body = self.body(request);
        with_retry(self.config.retry, |_| async {
            if let Some(source) = &self.config.token_source {
                return self.complete_on_plan(source.as_ref(), &body).await;
            }
            let builder = prepared(self.http.post(&self.url), &self.config)
                .bearer_auth(self.config.api_key.expose_secret());
            let response = post_json(builder, &body).await?;
            decode(&response)
        })
        .await
        .map_err(|error| effort_rejected(error, &self.config))
    }
}

impl ResponsesClient {
    /// One attempt paid by a ChatGPT plan. The bearer is asked for each
    /// time, so a renewal in between is used; a 401 gets the token renewed
    /// and one more try straight away, then fails.
    async fn complete_on_plan(
        &self,
        source: &dyn TokenSource,
        body: &Value,
    ) -> Result<Completion, LlmError> {
        let mut renewed = false;
        loop {
            let token = source.bearer().await?;
            let builder = prepared(self.http.post(&self.url), &self.config)
                .bearer_auth(token.expose_secret());
            match plan_exchange(builder, body).await {
                Err(LlmError::Unauthorized { status: 401, .. }) if !renewed => {
                    source.rejected(&token).await;
                    renewed = true;
                }
                other => return other,
            }
        }
    }
}

/// Sends `body` and reads the streamed reply to its end.
async fn plan_exchange(
    builder: reqwest::RequestBuilder,
    body: &Value,
) -> Result<Completion, LlmError> {
    let response = builder
        .json(body)
        .send()
        .await
        .map_err(LlmError::Transport)?;
    let status = response.status();
    if !status.is_success() {
        let headers = response.headers().clone();
        let text = response.text().await.map_err(LlmError::Transport)?;
        return Err(plan_error_for_status(status, &headers, &text));
    }
    decode(&read_completed(response).await?)
}

/// A non-2xx answer from a plan: OpenAI's own codes first, then the usual
/// mapping.
fn plan_error_for_status(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    body: &str,
) -> LlmError {
    match status.as_u16() {
        429 if body.contains(USAGE_LIMIT) => LlmError::PlanUsageExhausted,
        403 if body.contains(NOT_ELIGIBLE) => LlmError::PlanNotEligible,
        _ => error_for_status(status, headers, body),
    }
}

/// Reads events until the reply says how it ended and returns the response
/// object it carries: `response.completed`, or `response.incomplete`, whose
/// stop reason [`decode`] reads. A failure event, or a stream that ends
/// first, is an error.
async fn read_completed(mut response: reqwest::Response) -> Result<Value, LlmError> {
    let mut events = EventStream::default();
    let mut seen = 0_usize;
    loop {
        while let Some(data) = events.next_data() {
            seen += 1;
            if let Some(done) = terminal(&data)? {
                return Ok(done);
            }
        }
        match response.chunk().await {
            Ok(Some(chunk)) => events.push(&chunk),
            Ok(None) => break,
            Err(error) if error.is_timeout() => return Err(LlmError::Transport(error)),
            Err(error) => return Err(LlmError::StreamInterrupted(error.to_string())),
        }
    }
    if seen == 0 && !events.rest().is_empty() {
        return Err(LlmError::Decode(format!(
            "not an event stream: {}",
            truncate_body(&String::from_utf8_lossy(events.rest()))
        )));
    }
    Err(LlmError::StreamInterrupted(format!(
        "{seen} events, none of them response.completed"
    )))
}

/// What one event of the stream ends, if it ends the reply: the final
/// response object, or the error it failed with. Progress events, and data
/// that is not an event at all, end nothing.
fn terminal(data: &str) -> Result<Option<Value>, LlmError> {
    let Ok(event) = serde_json::from_str::<Value>(data) else {
        tracing::debug!(
            bytes = data.len(),
            "skipping a stream event that is not JSON"
        );
        return Ok(None);
    };
    match event.get("type").and_then(Value::as_str) {
        Some("response.completed" | "response.incomplete") => {
            event.get("response").cloned().map(Some).ok_or_else(|| {
                LlmError::Decode("a final stream event without a response".to_owned())
            })
        }
        Some("response.failed") => Err(failure(
            event
                .pointer("/response/error")
                .or_else(|| event.get("error")),
        )),
        Some("error") => Err(failure(Some(event.get("error").unwrap_or(&event)))),
        _ => Ok(None),
    }
}
