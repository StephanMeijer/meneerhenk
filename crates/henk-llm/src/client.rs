//! The [`ModelClient`] trait and how a configuration becomes a client.

use std::sync::Arc;
use std::time::Duration;

use secrecy::SecretString;
use serde::Deserialize;

use crate::anthropic::AnthropicClient;
use crate::error::LlmError;
use crate::http::RetryPolicy;
use crate::openai::OpenAiClient;
use crate::types::{Completion, CompletionRequest};

/// One model behind one endpoint.
#[async_trait::async_trait]
pub trait ModelClient: Send + Sync {
    /// The model name, for markers and logs.
    fn model(&self) -> &str;

    /// Runs one completion, with retries.
    async fn complete(&self, request: &CompletionRequest) -> Result<Completion, LlmError>;
}

/// Which wire format an endpoint speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    /// OpenAI chat completions, and everything compatible with it.
    OpenAi,
    /// Anthropic Messages.
    Anthropic,
}

/// Which parameter carries the output cap on OpenAI-compatible endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaxTokensParam {
    /// `max_tokens`: Ollama, most proxies, older OpenAI models.
    #[default]
    MaxTokens,
    /// `max_completion_tokens`: current OpenAI models.
    MaxCompletionTokens,
}

/// How hard a Claude model thinks: Anthropic's `output_config.effort`.
///
/// Current Claude models always think, adaptively; effort is the control.
/// Sending it also sends `thinking: {type: "adaptive"}`. Claude Opus 5.5
/// defaults to `medium` when it is not sent, other models to `high`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    /// Least thinking: simple, high-volume work.
    Low,
    /// The step down from `high` where quality holds.
    Medium,
    /// The usual choice for work where correctness matters.
    High,
    /// Between `high` and `max`.
    Xhigh,
    /// Most thinking, most cost.
    Max,
}

impl Effort {
    /// The value sent on the wire.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

/// Everything needed to build a client.
#[derive(Debug, Clone)]
pub struct ModelConfig {
    /// Wire format.
    pub provider: Provider,
    /// Base URL without a trailing slash. OpenAI style: ends in `/v1`.
    /// Anthropic style: the host only.
    pub base_url: String,
    /// The credential. Sent as `Authorization: Bearer` (OpenAI style) or
    /// `x-api-key` (Anthropic style).
    pub api_key: SecretString,
    /// The model name sent to the endpoint.
    pub model: String,
    /// Default output cap when a request sets none.
    pub max_tokens: u32,
    /// Per-attempt timeout.
    pub timeout: Duration,
    /// Retry policy.
    pub retry: RetryPolicy,
    /// OpenAI style only: which parameter carries the output cap.
    pub max_tokens_param: MaxTokensParam,
    /// Anthropic style only: thinking effort. `None` sends neither
    /// `thinking` nor `output_config`, leaving the model's default.
    pub effort: Option<Effort>,
    /// Anthropic style only: mark the system prompt and the end of the
    /// conversation for prompt caching. Off for a proxy that rejects
    /// `cache_control`.
    pub prompt_cache: bool,
}

impl ModelConfig {
    /// The base URL with any trailing slashes removed.
    #[must_use]
    pub fn base_url(&self) -> &str {
        self.base_url.trim_end_matches('/')
    }
}

/// Builds the client for a configuration.
///
/// # Errors
///
/// Returns [`LlmError::InvalidConfig`] for an empty model or base URL, or a
/// base URL that is not `http(s)://`.
pub fn client_for(config: ModelConfig) -> Result<Arc<dyn ModelClient>, LlmError> {
    if config.model.trim().is_empty() {
        return Err(LlmError::InvalidConfig("model name is empty".to_owned()));
    }
    let base = config.base_url();
    if !(base.starts_with("https://") || base.starts_with("http://")) {
        return Err(LlmError::InvalidConfig(format!(
            "base_url {base:?} must start with http(s)://"
        )));
    }
    if config.effort.is_some() && config.provider != Provider::Anthropic {
        return Err(LlmError::InvalidConfig(
            "effort applies to Anthropic-style endpoints only".to_owned(),
        ));
    }
    Ok(match config.provider {
        Provider::OpenAi => Arc::new(OpenAiClient::new(config)?),
        Provider::Anthropic => Arc::new(AnthropicClient::new(config)?),
    })
}
