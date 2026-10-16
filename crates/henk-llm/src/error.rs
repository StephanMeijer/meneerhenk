//! Errors a model call can produce.

use std::time::Duration;

/// Why a completion failed.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// HTTP 429. Retried, honouring `Retry-After` when present.
    #[error("rate limited by the model endpoint")]
    RateLimited {
        /// How long the endpoint asked us to wait, if it said.
        retry_after: Option<Duration>,
    },
    /// HTTP 529 (Anthropic) or 503 with an overload body. Retried.
    #[error("model endpoint overloaded")]
    Overloaded,
    /// Another 5xx or 408. Retried.
    #[error("model endpoint returned {status}: {body}")]
    Server {
        /// HTTP status.
        status: u16,
        /// Response body, truncated.
        body: String,
    },
    /// HTTP 401 or 403. Not retried.
    #[error("model endpoint rejected the credential ({status}): {body}")]
    Unauthorized {
        /// HTTP status.
        status: u16,
        /// Response body, truncated.
        body: String,
    },
    /// Any other 4xx. Not retried; the request itself is wrong.
    #[error("model endpoint rejected the request ({status}): {body}")]
    BadRequest {
        /// HTTP status.
        status: u16,
        /// Response body, truncated.
        body: String,
    },
    /// The connection failed or timed out. Retried.
    #[error("transport error talking to the model endpoint")]
    Transport(#[source] reqwest::Error),
    /// HTTP 400 naming the reasoning parameter, on a model with `effort`
    /// set: the endpoint does not take that effort (#366). Not retried.
    #[error(
        "the model endpoint refused effort {effort:?} (sent as {param}); remove the model's `effort` or set one this model accepts ({status}): {body}"
    )]
    EffortRejected {
        /// The effort as configured.
        effort: &'static str,
        /// The parameter it was sent as.
        param: &'static str,
        /// HTTP status.
        status: u16,
        /// Response body, truncated.
        body: String,
    },
    /// The endpoint answered 2xx with something we could not read.
    #[error("could not decode the model response: {0}")]
    Decode(String),
    /// Retries ran out. The last error is inside.
    #[error("gave up after {attempts} attempts; last error: {last}")]
    RetriesExhausted {
        /// Attempts made.
        attempts: u32,
        /// The error of the last attempt.
        #[source]
        last: Box<LlmError>,
    },
    /// The configuration cannot produce a client.
    #[error("invalid model configuration: {0}")]
    InvalidConfig(String),
    /// The ChatGPT sign-in of an endpoint is gone: OpenAI will not renew it,
    /// or there is none yet (#620). Not retried; a new sign-in fixes it.
    #[error(
        "the ChatGPT sign-in of endpoint {endpoint} cannot be used ({reason}); sign in again with `henk auth openai`"
    )]
    SignedOut {
        /// The endpoint, as configuration names it.
        endpoint: String,
        /// A short word for why: OpenAI's error code, or that there is no
        /// sign-in file yet. Never a token.
        reason: String,
    },
    /// The file that holds an endpoint's ChatGPT sign-in cannot be read or
    /// written (#620). Not retried.
    #[error("the sign-in file of endpoint {endpoint} cannot be used: {problem}")]
    CredentialFile {
        /// The endpoint, as configuration names it.
        endpoint: String,
        /// What is wrong, never including a token.
        problem: String,
    },
}

impl LlmError {
    /// Whether a fresh attempt could succeed.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::RateLimited { .. } | Self::Overloaded | Self::Server { .. } => true,
            Self::Transport(error) => {
                error.is_timeout() || error.is_connect() || error.is_request()
            }
            Self::Unauthorized { .. }
            | Self::BadRequest { .. }
            | Self::EffortRejected { .. }
            | Self::Decode(_)
            | Self::RetriesExhausted { .. }
            | Self::InvalidConfig(_)
            | Self::SignedOut { .. }
            | Self::CredentialFile { .. } => false,
        }
    }

    /// The wait the endpoint asked for, if any.
    #[must_use]
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after } => *retry_after,
            _ => None,
        }
    }
}

/// Cuts a response body down to something that fits in a log line.
pub(crate) fn truncate_body(body: &str) -> String {
    const LIMIT: usize = 600;
    if body.len() <= LIMIT {
        return body.to_owned();
    }
    let end = body.floor_char_boundary(LIMIT);
    format!("{}...", body.get(..end).unwrap_or_default())
}
