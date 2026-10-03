//! Shared HTTP plumbing: the client, status mapping and retries.

use std::future::Future;
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::{HeaderMap, RETRY_AFTER};
use tracing::{debug, warn};

use crate::error::{LlmError, truncate_body};

/// How many times to try and how long to wait between tries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Total attempts including the first. At least 1.
    pub max_attempts: u32,
    /// Delay before the second attempt; doubles each time.
    pub base_delay: Duration,
    /// Cap on the delay, also when `Retry-After` asks for more.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(30),
        }
    }
}

impl RetryPolicy {
    /// Delay before attempt number `next_attempt` (1-based; the first retry
    /// is attempt 2), unless the endpoint asked for a specific wait.
    #[must_use]
    pub fn delay_before(&self, next_attempt: u32, asked: Option<Duration>) -> Duration {
        let wait = asked.unwrap_or_else(|| {
            let doublings = next_attempt.saturating_sub(2).min(16);
            let scaled = self.base_delay.saturating_mul(1_u32 << doublings);
            scaled.saturating_add(jitter(scaled))
        });
        wait.min(self.max_delay)
    }
}

/// Up to a quarter of the delay, derived from the clock so no RNG is needed.
fn jitter(base: Duration) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let quarter = base / 4;
    quarter.mul_f64(f64::from(nanos % 1000) / 1000.0)
}

/// Runs `attempt` until it succeeds, fails with a non-retryable error, or the
/// policy is exhausted. The closure receives the 1-based attempt number.
///
/// # Errors
///
/// Returns the first non-retryable error unchanged, or
/// [`LlmError::RetriesExhausted`] wrapping the last retryable one.
pub async fn with_retry<T, F, Fut>(policy: RetryPolicy, mut attempt: F) -> Result<T, LlmError>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T, LlmError>>,
{
    let max_attempts = policy.max_attempts.max(1);
    let mut number = 1;
    loop {
        match attempt(number).await {
            Ok(value) => return Ok(value),
            Err(error) if error.is_retryable() && number < max_attempts => {
                let wait = policy.delay_before(number + 1, error.retry_after());
                warn!(attempt = number, wait_ms = wait.as_millis(), %error, "retrying model call");
                tokio::time::sleep(wait).await;
                number += 1;
            }
            Err(error) if error.is_retryable() => {
                return Err(LlmError::RetriesExhausted {
                    attempts: number,
                    last: Box::new(error),
                });
            }
            Err(error) => return Err(error),
        }
    }
}

/// Builds the shared HTTP client.
///
/// # Errors
///
/// Returns [`LlmError::InvalidConfig`] when reqwest cannot build a client,
/// which only happens when no TLS provider is installed.
pub fn build_client(timeout: Duration) -> Result<reqwest::Client, LlmError> {
    ensure_tls_provider();
    reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|error| LlmError::InvalidConfig(format!("http client: {error}")))
}

/// Installs rustls's ring provider if the process has none yet.
///
/// reqwest is built without a default provider so that the licence policy
/// can forbid aws-lc. Every crate that builds an HTTP client calls this first.
pub fn ensure_tls_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        // A race with another installer only yields "already installed".
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

/// Maps a non-2xx response to an error.
#[must_use]
pub fn error_for_status(status: StatusCode, headers: &HeaderMap, body: &str) -> LlmError {
    let body = truncate_body(body);
    match status.as_u16() {
        429 => LlmError::RateLimited {
            retry_after: retry_after(headers),
        },
        529 => LlmError::Overloaded,
        401 | 403 => LlmError::Unauthorized {
            status: status.as_u16(),
            body,
        },
        408 | 500..=599 => {
            if status == StatusCode::SERVICE_UNAVAILABLE && body.contains("overloaded") {
                LlmError::Overloaded
            } else {
                LlmError::Server {
                    status: status.as_u16(),
                    body,
                }
            }
        }
        _ => LlmError::BadRequest {
            status: status.as_u16(),
            body,
        },
    }
}

/// Reads `Retry-After` in seconds. HTTP-date values are ignored.
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    let seconds: u64 = value.parse().ok()?;
    debug!(seconds, "endpoint asked for a wait");
    Some(Duration::from_secs(seconds))
}

/// Sends a JSON request and returns the decoded JSON body of a 2xx response.
///
/// # Errors
///
/// Transport failures become [`LlmError::Transport`], non-2xx statuses go
/// through [`error_for_status`], and an unreadable 2xx body is
/// [`LlmError::Decode`].
pub async fn post_json(
    request: reqwest::RequestBuilder,
    body: &serde_json::Value,
) -> Result<serde_json::Value, LlmError> {
    let response = request
        .json(body)
        .send()
        .await
        .map_err(LlmError::Transport)?;
    let status = response.status();
    let headers = response.headers().clone();
    let text = response.text().await.map_err(LlmError::Transport)?;
    if !status.is_success() {
        return Err(error_for_status(status, &headers, &text));
    }
    serde_json::from_str(&text)
        .map_err(|error| LlmError::Decode(format!("{error} in body: {}", truncate_body(&text))))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    fn fast_policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
        }
    }

    #[tokio::test]
    async fn retries_retryable_errors_then_succeeds() {
        let calls = AtomicU32::new(0);
        let result = with_retry(fast_policy(), |_| {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if n < 2 {
                    Err(LlmError::Overloaded)
                } else {
                    Ok(n)
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn gives_up_after_max_attempts() {
        let calls = AtomicU32::new(0);
        let result: Result<(), _> = with_retry(fast_policy(), |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            async {
                Err(LlmError::Server {
                    status: 500,
                    body: String::new(),
                })
            }
        })
        .await;
        assert!(matches!(
            result,
            Err(LlmError::RetriesExhausted { attempts: 3, .. })
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn does_not_retry_bad_requests() {
        let calls = AtomicU32::new(0);
        let result: Result<(), _> = with_retry(fast_policy(), |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            async {
                Err(LlmError::BadRequest {
                    status: 400,
                    body: String::new(),
                })
            }
        })
        .await;
        assert!(matches!(result, Err(LlmError::BadRequest { .. })));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn retry_after_wins_but_is_capped() {
        let policy = RetryPolicy {
            max_attempts: 4,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(10),
        };
        assert_eq!(
            policy.delay_before(2, Some(Duration::from_secs(3))),
            Duration::from_secs(3)
        );
        assert_eq!(
            policy.delay_before(2, Some(Duration::from_secs(60))),
            Duration::from_secs(10)
        );
        let backoff = policy.delay_before(3, None);
        assert!(backoff >= Duration::from_secs(2) && backoff <= Duration::from_millis(2500));
    }

    #[test]
    fn status_mapping() {
        let headers = HeaderMap::new();
        assert!(matches!(
            error_for_status(StatusCode::TOO_MANY_REQUESTS, &headers, ""),
            LlmError::RateLimited { retry_after: None }
        ));
        assert!(matches!(
            error_for_status(StatusCode::from_u16(529).unwrap(), &headers, ""),
            LlmError::Overloaded
        ));
        assert!(matches!(
            error_for_status(StatusCode::UNAUTHORIZED, &headers, ""),
            LlmError::Unauthorized { status: 401, .. }
        ));
        assert!(matches!(
            error_for_status(StatusCode::BAD_GATEWAY, &headers, ""),
            LlmError::Server { status: 502, .. }
        ));
        assert!(matches!(
            error_for_status(StatusCode::NOT_FOUND, &headers, ""),
            LlmError::BadRequest { status: 404, .. }
        ));
    }
}
