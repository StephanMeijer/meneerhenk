//! Errors from talking to an MCP server.

use std::time::Duration;

/// Why an MCP operation failed.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// The server process could not be started.
    #[error("could not start MCP server {alias}: {source}")]
    Spawn {
        /// Configured alias.
        alias: String,
        /// The OS error.
        #[source]
        source: std::io::Error,
    },
    /// The MCP handshake failed.
    #[error("MCP server {alias} did not initialize: {detail}")]
    Initialize {
        /// Configured alias.
        alias: String,
        /// What went wrong.
        detail: String,
    },
    /// A request failed after the handshake.
    #[error("MCP server {alias}: {source}")]
    Service {
        /// Configured alias.
        alias: String,
        /// The rmcp error.
        #[source]
        source: rmcp::ServiceError,
    },
    /// A call took longer than the configured limit.
    #[error("MCP server {alias} did not answer {tool} within {timeout:?}")]
    Timeout {
        /// Configured alias.
        alias: String,
        /// The tool that was called.
        tool: String,
        /// The limit.
        timeout: Duration,
    },
    /// An environment variable the configuration names is not set.
    #[error("environment variable {0} is not set")]
    MissingEnv(String),
    /// The configuration is unusable.
    #[error("invalid MCP configuration for {alias}: {detail}")]
    InvalidConfig {
        /// Configured alias.
        alias: String,
        /// What is wrong.
        detail: String,
    },
}
