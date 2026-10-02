//! The Anthropic Messages adapter.

use secrecy::ExposeSecret as _;
use serde_json::{Map, Value, json};
use tracing::instrument;

use crate::client::{ModelClient, ModelConfig};
use crate::error::LlmError;
use crate::http::{build_client, post_json, with_retry};
use crate::schema;
use crate::types::{
    Block, ChatMessage, Completion, CompletionRequest, Role, StopReason, ToolArguments, ToolCall,
    ToolChoice, Usage,
};

const API_VERSION: &str = "2023-06-01";

/// A client for one model on an Anthropic-style endpoint.
#[derive(Debug)]
pub struct AnthropicClient {
    config: ModelConfig,
    http: reqwest::Client,
    url: String,
}

impl AnthropicClient {
    /// Builds the client.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::InvalidConfig`] when the HTTP client cannot be built.
    pub fn new(config: ModelConfig) -> Result<Self, LlmError> {
        let http = build_client(config.timeout)?;
        let url = format!("{}/v1/messages", config.base_url());
        Ok(Self { config, http, url })
    }

    /// The request body for `request`. Public for fixture tests.
    #[must_use]
    pub fn body(&self, request: &CompletionRequest) -> Value {
        let mut body = Map::new();
        body.insert("model".into(), Value::String(self.config.model.clone()));
        body.insert(
            "max_tokens".into(),
            json!(request.max_tokens.unwrap_or(self.config.max_tokens)),
        );
        if let Some(system) = &request.system {
            body.insert("system".into(), json!(system));
        }
        if let Some(temperature) = request.temperature {
            body.insert("temperature".into(), json!(temperature));
        }
        body.insert(
            "messages".into(),
            Value::Array(encode_messages(&request.messages)),
        );
        if !request.tools.is_empty() {
            let tools: Vec<Value> = request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "name": tool.name.as_str(),
                        "description": tool.description,
                        "input_schema": schema::clean(&tool.input_schema),
                    })
                })
                .collect();
            body.insert("tools".into(), Value::Array(tools));
            body.insert(
                "tool_choice".into(),
                match &request.tool_choice {
                    ToolChoice::Auto => json!({"type": "auto"}),
                    ToolChoice::None => json!({"type": "none"}),
                    ToolChoice::Required => json!({"type": "any"}),
                    ToolChoice::Named(name) => json!({"type": "tool", "name": name.as_str()}),
                },
            );
        }
        Value::Object(body)
    }
}

/// Encodes messages, merging consecutive same-role messages because the
/// API requires strict alternation. Tool results go first in a user turn.
fn encode_messages(messages: &[ChatMessage]) -> Vec<Value> {
    let mut out: Vec<(Role, Vec<Value>)> = Vec::new();
    for message in messages {
        let content = encode_blocks(message);
        if content.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some((role, existing)) if *role == message.role => existing.extend(content),
            _ => out.push((message.role, content)),
        }
    }
    out.into_iter()
        .map(|(role, content)| {
            json!({
                "role": match role { Role::User => "user", Role::Assistant => "assistant" },
                "content": content,
            })
        })
        .collect()
}

fn encode_blocks(message: &ChatMessage) -> Vec<Value> {
    let mut results = Vec::new();
    let mut rest = Vec::new();
    for block in &message.blocks {
        match block {
            Block::ToolResult(result) => results.push(json!({
                "type": "tool_result",
                "tool_use_id": result.call_id,
                "content": result.content,
                "is_error": result.is_error,
            })),
            Block::Text(text) if !text.is_empty() => {
                rest.push(json!({"type": "text", "text": text}));
            }
            Block::Text(_) => {}
            Block::ToolCall(call) => {
                let input = match &call.arguments {
                    ToolArguments::Parsed(value) => value.clone(),
                    ToolArguments::Malformed(raw) => json!({"_raw": raw}),
                };
                rest.push(json!({
                    "type": "tool_use",
                    "id": call.id,
                    "name": call.name,
                    "input": input,
                }));
            }
            // Thinking blocks must come back exactly as received, and before
            // the tool_use blocks they preceded; we keep original order.
            Block::Opaque(value) => rest.push(value.clone()),
        }
    }
    results.extend(rest);
    results
}

/// Turns a Messages response into a [`Completion`]. Public for fixture tests.
///
/// # Errors
///
/// Returns [`LlmError::Decode`] when the response has no content array.
pub fn decode(response: &Value) -> Result<Completion, LlmError> {
    let content = response
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| LlmError::Decode("no content in response".to_owned()))?;
    let mut blocks = Vec::new();
    for item in content {
        match item.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    blocks.push(Block::Text(text.to_owned()));
                }
            }
            Some("tool_use") => {
                let id = item
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| LlmError::Decode("tool_use without id".to_owned()))?;
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| LlmError::Decode("tool_use without name".to_owned()))?;
                let input = item.get("input").cloned().unwrap_or_else(|| json!({}));
                blocks.push(Block::ToolCall(ToolCall {
                    id: id.to_owned(),
                    name: name.to_owned(),
                    arguments: ToolArguments::Parsed(input),
                }));
            }
            // thinking, redacted_thinking, and anything new: echo back verbatim.
            _ => blocks.push(Block::Opaque(item.clone())),
        }
    }
    let stop = match response.get("stop_reason").and_then(Value::as_str) {
        Some("end_turn") | None => StopReason::EndTurn,
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        Some(other) => StopReason::Other(other.to_owned()),
    };
    let usage = response
        .get("usage")
        .map_or_else(Usage::default, |usage| Usage {
            input_tokens: usage
                .get("input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            output_tokens: usage
                .get("output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        });
    Ok(Completion {
        message: ChatMessage {
            role: Role::Assistant,
            blocks,
        },
        stop,
        usage,
    })
}

#[async_trait::async_trait]
impl ModelClient for AnthropicClient {
    fn model(&self) -> &str {
        &self.config.model
    }

    #[instrument(skip_all, fields(model = %self.config.model))]
    async fn complete(&self, request: &CompletionRequest) -> Result<Completion, LlmError> {
        let body = self.body(request);
        with_retry(self.config.retry, |_| async {
            let builder = self
                .http
                .post(&self.url)
                .header("x-api-key", self.config.api_key.expose_secret())
                .header("anthropic-version", API_VERSION);
            let response = post_json(builder, &body).await?;
            decode(&response)
        })
        .await
    }
}
