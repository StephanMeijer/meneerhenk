//! The OpenAI-compatible chat completions adapter.
//!
//! Covers OpenAI itself, the local proxy, Ollama's `/v1` and OpenRouter.

use secrecy::ExposeSecret as _;
use serde_json::{Map, Value, json};
use tracing::instrument;

use crate::client::{MaxTokensParam, ModelClient, ModelConfig};
use crate::error::LlmError;
use crate::http::{build_client, post_json, with_retry};
use crate::schema;
use crate::types::{
    Block, ChatMessage, Completion, CompletionRequest, Role, StopReason, ToolArguments, ToolCall,
    ToolChoice, Usage,
};

/// A client for one model on an OpenAI-compatible endpoint.
#[derive(Debug)]
pub struct OpenAiClient {
    config: ModelConfig,
    http: reqwest::Client,
    url: String,
}

impl OpenAiClient {
    /// Builds the client.
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::InvalidConfig`] when the HTTP client cannot be built.
    pub fn new(config: ModelConfig) -> Result<Self, LlmError> {
        let http = build_client(config.timeout)?;
        let url = format!("{}/chat/completions", config.base_url());
        Ok(Self { config, http, url })
    }

    /// The request body for `request`. Public for fixture tests.
    #[must_use]
    pub fn body(&self, request: &CompletionRequest) -> Value {
        let mut messages = Vec::new();
        if let Some(system) = &request.system {
            messages.push(json!({"role": "system", "content": system}));
        }
        for message in &request.messages {
            encode_message(message, &mut messages);
        }

        let mut body = Map::new();
        body.insert("model".into(), Value::String(self.config.model.clone()));
        body.insert("messages".into(), Value::Array(messages));
        let max_tokens = request.max_tokens.unwrap_or(self.config.max_tokens);
        let key = match self.config.max_tokens_param {
            MaxTokensParam::MaxTokens => "max_tokens",
            MaxTokensParam::MaxCompletionTokens => "max_completion_tokens",
        };
        body.insert(key.into(), json!(max_tokens));
        if let Some(temperature) = request.temperature {
            body.insert("temperature".into(), json!(temperature));
        }
        if !request.tools.is_empty() {
            let tools: Vec<Value> = request
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": tool.name.as_str(),
                            "description": tool.description,
                            "parameters": schema::clean(&tool.input_schema),
                        }
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
                    ToolChoice::Named(name) => {
                        json!({"type": "function", "function": {"name": name.as_str()}})
                    }
                },
            );
        }
        Value::Object(body)
    }
}

fn encode_message(message: &ChatMessage, out: &mut Vec<Value>) {
    match message.role {
        Role::User => {
            // Tool results must directly follow the assistant's tool calls,
            // so they go first, then any text as a user message.
            let mut text = Vec::new();
            for block in &message.blocks {
                match block {
                    Block::ToolResult(result) => {
                        let content = if result.is_error {
                            format!("Error: {}", result.content)
                        } else {
                            result.content.clone()
                        };
                        out.push(json!({
                            "role": "tool",
                            "tool_call_id": result.call_id,
                            "content": content,
                        }));
                    }
                    Block::Text(t) => text.push(t.as_str()),
                    Block::ToolCall(_) | Block::Opaque(_) => {}
                }
            }
            if !text.is_empty() {
                out.push(json!({"role": "user", "content": text.join("\n")}));
            }
        }
        Role::Assistant => {
            let text = message.text();
            let calls: Vec<Value> = message
                .tool_calls()
                .map(|call| {
                    let arguments = match &call.arguments {
                        ToolArguments::Parsed(value) => value.to_string(),
                        ToolArguments::Malformed(raw) => raw.clone(),
                    };
                    json!({
                        "id": call.id,
                        "type": "function",
                        "function": {"name": call.name, "arguments": arguments},
                    })
                })
                .collect();
            let mut entry = Map::new();
            entry.insert("role".into(), json!("assistant"));
            entry.insert(
                "content".into(),
                if text.is_empty() {
                    Value::Null
                } else {
                    json!(text)
                },
            );
            if !calls.is_empty() {
                entry.insert("tool_calls".into(), Value::Array(calls));
            }
            out.push(Value::Object(entry));
        }
    }
}

/// Turns a chat completions response into a [`Completion`]. Public for fixture tests.
///
/// # Errors
///
/// Returns [`LlmError::Decode`] when the response has no choice or an
/// unreadable message.
pub fn decode(response: &Value) -> Result<Completion, LlmError> {
    let choice = response
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| LlmError::Decode("no choices in response".to_owned()))?;
    let message = choice
        .get("message")
        .ok_or_else(|| LlmError::Decode("choice without message".to_owned()))?;

    let mut blocks = Vec::new();
    match message.get("content") {
        Some(Value::String(text)) if !text.is_empty() => blocks.push(Block::Text(text.clone())),
        Some(Value::Array(parts)) => {
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    blocks.push(Block::Text(text.to_owned()));
                }
            }
        }
        _ => {}
    }
    if let Some(reasoning) = message.get("reasoning_content").filter(|v| !v.is_null()) {
        blocks.push(Block::Opaque(json!({"reasoning_content": reasoning})));
    }

    let mut had_calls = false;
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for (index, call) in calls.iter().enumerate() {
            let function = call.get("function").unwrap_or(call);
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| LlmError::Decode("tool call without name".to_owned()))?
                .to_owned();
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .map_or_else(|| synthetic_id(index), str::to_owned);
            let arguments = match function.get("arguments") {
                Some(Value::String(raw)) => serde_json::from_str::<Value>(raw).map_or_else(
                    |_| ToolArguments::Malformed(raw.clone()),
                    ToolArguments::Parsed,
                ),
                Some(Value::Object(map)) => ToolArguments::Parsed(Value::Object(map.clone())),
                Some(Value::Null) | None => ToolArguments::Parsed(json!({})),
                Some(other) => ToolArguments::Malformed(other.to_string()),
            };
            blocks.push(Block::ToolCall(ToolCall {
                id,
                name,
                arguments,
            }));
            had_calls = true;
        }
    }

    let stop = match choice.get("finish_reason").and_then(Value::as_str) {
        Some("tool_calls" | "function_call") => StopReason::ToolUse,
        Some("stop") if had_calls => StopReason::ToolUse,
        Some("stop") | None => StopReason::EndTurn,
        Some("length") => StopReason::MaxTokens,
        Some(other) => StopReason::Other(other.to_owned()),
    };
    let usage = response
        .get("usage")
        .map_or_else(Usage::default, |usage| Usage {
            input_tokens: usage
                .get("prompt_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            output_tokens: usage
                .get("completion_tokens")
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

/// Ollama's compatibility layer can omit ids. Results need one to refer to.
fn synthetic_id(index: usize) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("call_{index}_{nanos:08x}")
}

#[async_trait::async_trait]
impl ModelClient for OpenAiClient {
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
                .bearer_auth(self.config.api_key.expose_secret());
            let response = post_json(builder, &body).await?;
            decode(&response)
        })
        .await
    }
}
