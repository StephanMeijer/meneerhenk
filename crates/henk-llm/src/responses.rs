//! The OpenAI Responses adapter: `POST {base_url}/responses` (#365).
//!
//! Stateless: every request sends `store: false` and the whole
//! conversation, and asks for the model's reasoning back encrypted
//! (`include: ["reasoning.encrypted_content"]`). Each turn's reasoning
//! items come back as [`Block::Opaque`] and are sent again, unchanged and
//! in their place, so a reasoning model keeps its chain of thought across
//! tool calls while nothing is kept at OpenAI.

use secrecy::ExposeSecret as _;
use serde_json::{Map, Value, json};
use tracing::instrument;

use crate::client::{ModelClient, ModelConfig, Provider};
use crate::error::{LlmError, truncate_body};
use crate::http::{build_endpoint_client, post_json, prepared, with_retry};
use crate::openai::{decode_usage, effort_rejected};
use crate::schema;
use crate::types::{
    Block, ChatMessage, Completion, CompletionRequest, Role, StopReason, ToolArguments, ToolCall,
    ToolChoice, Usage,
};

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
        body.insert(
            "max_output_tokens".into(),
            json!(request.max_tokens.unwrap_or(self.config.max_tokens)),
        );
        if let Some(effort) = self
            .config
            .effort
            .and_then(|effort| effort.wire(Provider::OpenAi))
        {
            // Reasoning models reject sampling parameters, so temperature
            // is never sent alongside effort.
            body.insert("reasoning".into(), json!({"effort": effort}));
        } else if let Some(temperature) = request.temperature {
            body.insert("temperature".into(), json!(temperature));
        }
        body.insert("store".into(), json!(false));
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
        let error = response.get("error");
        let code = error
            .and_then(|e| e.get("code"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let message = error
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("the response failed");
        let message = format!("{code}: {message}");
        // A failed response arrives with HTTP 200, so the status-based
        // mapping of `post_json` never sees it. The transient codes are
        // mapped to the errors a real 429 or 5xx would give, so they are
        // retried; any other code is the request's own fault.
        return Err(match code {
            "rate_limit_exceeded" => LlmError::RateLimited { retry_after: None },
            "server_error" | "vector_store_timeout" => LlmError::Server {
                status: 500,
                body: message,
            },
            _ => LlmError::BadRequest {
                status: 400,
                body: message,
            },
        });
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
            let builder = prepared(self.http.post(&self.url), &self.config)
                .bearer_auth(self.config.api_key.expose_secret());
            let response = post_json(builder, &body).await?;
            decode(&response)
        })
        .await
        .map_err(|error| effort_rejected(error, &self.config))
    }
}
