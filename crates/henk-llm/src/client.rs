//! The [`ModelClient`] trait and how a configuration becomes a client.

use std::sync::Arc;
use std::time::Duration;

use secrecy::SecretString;
use serde::Deserialize;

use crate::anthropic::AnthropicClient;
use crate::error::LlmError;
use crate::http::RetryPolicy;
use crate::openai::OpenAiClient;
use crate::responses::ResponsesClient;
use crate::types::{Completion, CompletionRequest};

/// One model behind one endpoint.
#[async_trait::async_trait]
pub trait ModelClient: Send + Sync {
    /// The model name, for markers and logs.
    fn model(&self) -> &str;

    /// Runs one completion, with retries.
    async fn complete(&self, request: &CompletionRequest) -> Result<Completion, LlmError>;
}

/// Where a model client gets its bearer when the credential is not a fixed
/// key (#620): a sign-in that renews itself.
#[async_trait::async_trait]
pub trait TokenSource: Send + Sync {
    /// The bearer to send now, renewed first when it is about to expire.
    ///
    /// # Errors
    ///
    /// Returns why no bearer can be had: the sign-in is gone, its file is
    /// unusable, or the endpoint that renews it could not be reached.
    async fn bearer(&self) -> Result<SecretString, LlmError>;

    /// The endpoint refused `token`: renew rather than send it again.
    async fn rejected(&self, token: &SecretString);
}

/// Which wire format an endpoint speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    /// OpenAI chat completions, and everything compatible with it. Spelled
    /// `openai` in configuration, as everyone writes it (#364).
    #[serde(rename = "openai")]
    OpenAi,
    /// Anthropic Messages.
    Anthropic,
}

impl Provider {
    /// The name configuration spells it with: `openai` or `anthropic`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
        }
    }
}

/// Which OpenAI API an `openai` endpoint is called through (#365).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Api {
    /// `POST /chat/completions`, which every OpenAI-compatible server
    /// speaks.
    #[default]
    ChatCompletions,
    /// `POST /responses`: OpenAI's current API, where a reasoning model's
    /// reasoning is carried from turn to turn.
    Responses,
}

impl Api {
    /// The name configuration spells it with.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::Responses => "responses",
        }
    }
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

/// How hard a model thinks (#366).
///
/// Anthropic: sent as `output_config.effort`, with `thinking: {type:
/// "adaptive"}`. Current Claude models always think, adaptively; effort is
/// the control. Claude Opus 5.5 defaults to `medium` when it is not sent,
/// other models to `high`.
///
/// OpenAI: sent as `reasoning_effort` (Chat Completions) or
/// `reasoning.effort` (Responses). Which values a model takes depends on
/// the model; `none` and `minimal` exist on OpenAI only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    /// OpenAI only: no reasoning at all.
    None,
    /// OpenAI only: the least reasoning short of none.
    Minimal,
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
    /// The name configuration spells it with.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// The levels `provider` takes.
    #[must_use]
    pub fn accepted(provider: Provider) -> &'static [Self] {
        match provider {
            Provider::Anthropic => &[Self::Low, Self::Medium, Self::High, Self::Xhigh, Self::Max],
            Provider::OpenAi => &[
                Self::None,
                Self::Minimal,
                Self::Low,
                Self::Medium,
                Self::High,
                Self::Xhigh,
            ],
        }
    }

    /// The value sent to `provider`, or `None` when it takes no such level.
    /// Both providers spell the levels they take as Henk does today (OpenAI
    /// has no `max`); a provider that spelled one differently would be
    /// mapped here.
    #[must_use]
    pub fn wire(self, provider: Provider) -> Option<&'static str> {
        Self::accepted(provider)
            .contains(&self)
            .then(|| self.as_str())
    }

    /// The levels `provider` takes, as a refusal lists them.
    #[must_use]
    pub fn accepted_list(provider: Provider) -> String {
        Self::accepted(provider)
            .iter()
            .map(|effort| effort.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The request parameter that carries effort on `provider` over `api`.
#[must_use]
pub fn effort_param(provider: Provider, api: Api) -> &'static str {
    match (provider, api) {
        (Provider::Anthropic, _) => "output_config.effort",
        (Provider::OpenAi, Api::ChatCompletions) => "reasoning_effort",
        (Provider::OpenAi, Api::Responses) => "reasoning.effort",
    }
}

/// Everything needed to build a client.
#[derive(Debug, Clone)]
pub struct ModelConfig {
    /// Wire format.
    pub provider: Provider,
    /// OpenAI style only: which API to call.
    pub api: Api,
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
    /// OpenAI Chat Completions only: which parameter carries the output
    /// cap. The Responses API always sends `max_output_tokens`.
    pub max_tokens_param: MaxTokensParam,
    /// Thinking effort, sent as [`effort_param`] says. `None` sends
    /// nothing, leaving the model's default.
    pub effort: Option<Effort>,
    /// Anthropic style only: mark the system prompt and the end of the
    /// conversation for prompt caching. Off for a proxy that rejects
    /// `cache_control`.
    pub prompt_cache: bool,
    /// Extra headers the endpoint wants on every request (#368).
    pub headers: Vec<(String, String)>,
}

impl ModelConfig {
    /// The base URL with any trailing slashes removed.
    #[must_use]
    pub fn base_url(&self) -> &str {
        self.base_url.trim_end_matches('/')
    }
}

/// Builds the client for a configuration, with an HTTP client of its own.
///
/// # Errors
///
/// As [`client_with`], and [`LlmError::InvalidConfig`] when no HTTP client
/// can be built.
pub fn client_for(config: ModelConfig) -> Result<Arc<dyn ModelClient>, LlmError> {
    client_with(config, crate::http::build_endpoint_client()?)
}

/// Builds the client for a configuration over `http`, the HTTP client the
/// models of one endpoint share (#368).
///
/// # Errors
///
/// Returns [`LlmError::InvalidConfig`] for an empty model or base URL, or a
/// base URL that is not `http(s)://`.
pub fn client_with(
    config: ModelConfig,
    http: reqwest::Client,
) -> Result<Arc<dyn ModelClient>, LlmError> {
    if config.model.trim().is_empty() {
        return Err(LlmError::InvalidConfig("model name is empty".to_owned()));
    }
    let base = config.base_url();
    if !(base.starts_with("https://") || base.starts_with("http://")) {
        return Err(LlmError::InvalidConfig(format!(
            "base_url {base:?} must start with http(s)://"
        )));
    }
    if config.api == Api::Responses && config.provider != Provider::OpenAi {
        return Err(LlmError::InvalidConfig(
            "the Responses API is OpenAI's; an Anthropic endpoint has none".to_owned(),
        ));
    }
    if let Some(effort) = config.effort
        && effort.wire(config.provider).is_none()
    {
        return Err(LlmError::InvalidConfig(format!(
            "effort {:?} is not accepted by provider {:?}; accepted: {}",
            effort.as_str(),
            config.provider.as_str(),
            Effort::accepted_list(config.provider)
        )));
    }
    Ok(match (config.provider, config.api) {
        (Provider::OpenAi, Api::Responses) => Arc::new(ResponsesClient::with_http(config, http)),
        (Provider::OpenAi, Api::ChatCompletions) => Arc::new(OpenAiClient::with_http(config, http)),
        (Provider::Anthropic, _) => Arc::new(AnthropicClient::with_http(config, http)),
    })
}
