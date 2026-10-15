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
    pub(crate) fn status(status: u16, body: &str, headers: &reqwest::header::HeaderMap) -> Self {
        Self::Status {
            status,
            body: truncate(body),
            retry_after: retry_after(headers),
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
            // A connection that dropped after it was made (reset, closed by
            // the peer mid-message) is a request or body error, not a
            // connect error.
            Self::Transport(error) => {
                error.is_timeout() || error.is_connect() || error.is_request() || error.is_body()
            }
            // GitLab, through its MCP server: the server reports GitLab's
            // own status in its error text, as a failed tool result or as
            // the call's error.
            Self::ToolFailed { message, .. } => gitlab_transient(message),
            Self::Mcp(henk_mcp::McpError::Timeout { .. }) => true,
            Self::Mcp(error @ henk_mcp::McpError::Service { .. }) => {
                gitlab_transient(&error.to_string())
            }
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

/// Whether the GitLab MCP server's error text reports a GitLab failure that
/// may pass: `GitLab API error: 502 Bad Gateway`, a 429, or its rate limit.
fn gitlab_transient(text: &str) -> bool {
    if text.contains("GitLab API Rate Limit Exceeded") {
        return true;
    }
    text.split("GitLab API error: ")
        .nth(1)
        .and_then(|rest| rest.get(..3))
        .and_then(|code| code.parse::<u16>().ok())
        .is_some_and(|status| status >= 500 || status == 429)
}

/// The body of a 2xx answer, or the status error with what the platform
/// said and how long it asked to wait.
pub(crate) async fn success_text(response: reqwest::Response) -> Result<String, PlatformError> {
    let status = response.status();
    let headers = response.headers().clone();
    let text = response.text().await?;
    if !status.is_success() {
        return Err(PlatformError::status(status.as_u16(), &text, &headers));
    }
    Ok(text)
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
    let end = body.floor_char_boundary(LIMIT);
    format!("{}...", body.get(..end).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::gitlab_transient;

    #[test]
    fn gitlab_failures_that_may_pass_are_told_from_the_rest() {
        for passing in [
            "GitLab API error: 502 Bad Gateway\n<html>",
            "MCP server gitlab-write: GitLab API error: 500 Internal Server Error\n",
            "GitLab API error: 429 Too Many Requests\n",
            "GitLab API Rate Limit Exceeded: {\"message\":\"User API Key Rate limit exceeded\"}",
        ] {
            assert!(gitlab_transient(passing), "{passing}");
        }
        for lasting in [
            "GitLab API error: 404 Not Found\n",
            "GitLab API error: 403 Forbidden\n",
            "GitLab API error: 422 Unprocessable Entity\n",
            "404 Not Found",
            "Invalid arguments: body: Required",
        ] {
            assert!(!gitlab_transient(lasting), "{lasting}");
        }
    }
}
