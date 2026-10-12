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
        /// How long the platform asked to wait (`Retry-After`), if it did.
        retry_after: Option<std::time::Duration>,
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
    /// No installation of the App Henk may act for reaches the repository.
    #[error("GitHub App cannot act on {repo}: {reason}")]
    NotInstalled {
        /// The repository, or the installation asked for.
        repo: String,
        /// Why not.
        reason: String,
    },
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
    /// A list went on past the safety cap. Reading part of it would give a
    /// count that silently leaves the rest out (#113), so it is an error.
    #[error(
        "{what} has more than {limit} entries; Henk stopped reading rather than count part of it"
    )]
    TooMany {
        /// The list.
        what: String,
        /// How many entries were read.
        limit: usize,
    },
}

impl PlatformError {
    /// A status error from `status`, `body` (truncated here) and the
    /// response's `Retry-After`.
    pub(crate) fn status(
        status: u16,
        body: &str,
        headers: Option<&reqwest::header::HeaderMap>,
    ) -> Self {
        Self::Status {
            status,
            body: truncate(body),
            retry_after: headers.and_then(retry_after),
        }
    }

    /// Whether trying the same request again may succeed (#338): the
    /// platform failed or was busy (5xx, 429), asked for a wait on a 403
    /// (GitHub's secondary rate limit), or the connection did.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Status {
                status,
                retry_after,
                ..
            } => *status >= 500 || *status == 429 || (*status == 403 && retry_after.is_some()),
            Self::Transport(error) => error.is_timeout() || error.is_connect(),
            _ => false,
        }
    }

    /// The wait the platform asked for, if it did.
    #[must_use]
    pub const fn retry_after(&self) -> Option<std::time::Duration> {
        match self {
            Self::Status { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// The HTTP status, for a status error.
    #[must_use]
    pub const fn http_status(&self) -> Option<u16> {
        match self {
            Self::Status { status, .. } => Some(*status),
            _ => None,
        }
    }
}

/// `Retry-After` in seconds; an HTTP date is ignored.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<std::time::Duration> {
    let value = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    value.parse().ok().map(std::time::Duration::from_secs)
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
