//! Errors from a platform.

/// Why a platform call failed.
#[derive(Debug, thiserror::Error)]
pub enum PlatformError {
    /// The connection failed.
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),
    /// The platform answered with an error status.
    #[error("platform returned {status}: {body}")]
    Status {
        /// HTTP status.
        status: u16,
        /// Body, truncated.
        body: String,
    },
    /// A 2xx body did not have the expected shape.
    #[error("unexpected response: {0}")]
    Decode(String),
    /// The tracker cannot do this, or not for this kind of issue.
    #[error("not supported: {0}")]
    Unsupported(String),
    /// Credentials could not be produced.
    #[error("authentication: {0}")]
    Auth(String),
    /// An MCP-backed write failed.
    #[error(transparent)]
    Mcp(#[from] henk_mcp::McpError),
    /// The MCP server answered a write with an error result.
    #[error("{tool} failed: {message}")]
    ToolFailed {
        /// Tool name.
        tool: String,
        /// The server's text.
        message: String,
    },
}

pub(crate) fn truncate(body: &str) -> String {
    const LIMIT: usize = 500;
    if body.len() <= LIMIT {
        return body.to_owned();
    }
    let mut end = LIMIT;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", body.get(..end).unwrap_or_default())
}
