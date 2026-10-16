//! Provider-neutral chat completions.
//!
//! A [`ModelClient`] turns a [`CompletionRequest`] (system prompt, messages,
//! tool definitions) into one [`Completion`]. Three adapters exist:
//!
//! - [`openai`]: the OpenAI-compatible chat completions API, which also covers
//!   the local proxy, Ollama and OpenRouter;
//! - [`responses`]: OpenAI's Responses API, which carries a reasoning
//!   model's reasoning from turn to turn;
//! - [`anthropic`]: the Anthropic Messages API.
//!
//! All are non-streaming, except the Responses adapter when a ChatGPT
//! sign-in pays for it (#620). Retries with backoff live in [`http`] and
//! apply to all. Nothing in this crate knows about MCP, reviews or Henk; it
//! is a small, testable HTTP client.

pub mod anthropic;
pub mod chatgpt;
pub mod client;
pub mod error;
pub mod http;
pub mod models;
pub mod openai;
pub mod responses;
pub mod schema;
mod sse;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod types;

pub use client::{
    Api, Effort, MaxTokensParam, ModelClient, ModelConfig, Provider, TokenSource, client_for,
    client_with, effort_param,
};
pub use error::LlmError;
pub use http::{RetryPolicy, build_endpoint_client, ensure_tls_provider};
pub use models::list_models;
pub use types::{
    Block, CacheTtl, ChatMessage, Completion, CompletionRequest, Role, StopReason, ToolArguments,
    ToolCall, ToolChoice, ToolDef, ToolName, ToolResult, Usage,
};
