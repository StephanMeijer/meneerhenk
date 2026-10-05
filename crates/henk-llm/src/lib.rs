//! Provider-neutral chat completions.
//!
//! A [`ModelClient`] turns a [`CompletionRequest`] (system prompt, messages,
//! tool definitions) into one [`Completion`]. Two adapters exist:
//!
//! - [`openai`]: the OpenAI-compatible chat completions API, which also covers
//!   the local proxy, Ollama and OpenRouter;
//! - [`anthropic`]: the Anthropic Messages API.
//!
//! Both are non-streaming. Retries with backoff live in [`http`] and apply to
//! both. Nothing in this crate knows about MCP, reviews or Henk; it is a
//! small, testable HTTP client.

pub mod anthropic;
pub mod client;
pub mod error;
pub mod http;
pub mod models;
pub mod openai;
pub mod schema;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod types;

pub use client::{Effort, MaxTokensParam, ModelClient, ModelConfig, Provider, client_for};
pub use error::LlmError;
pub use http::{RetryPolicy, ensure_tls_provider};
pub use models::list_models;
pub use types::{
    Block, ChatMessage, Completion, CompletionRequest, Role, StopReason, ToolArguments, ToolCall,
    ToolChoice, ToolDef, ToolName, ToolResult, Usage,
};
