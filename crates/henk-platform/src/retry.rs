//! Trying a platform request again when the platform failed for a moment
//! (#338): a 5xx, a rate limit, a dropped connection. One bad response must
//! not lose a run's output.

use std::future::Future;

pub use henk_llm::RetryPolicy;
use tracing::warn;

use crate::error::PlatformError;

/// Runs `attempt` until it succeeds, fails in a way another try will not
/// change, or `policy` runs out. `what` names the request in the WARN each
/// retry logs, with the status and the attempt.
///
/// Only for requests that are safe to repeat: reads, updates, and creates
/// the platform does not repeat. A create that could land twice goes
/// through [`create_once`].
///
/// # Errors
///
/// Returns the first error that is not retryable, or the last one when the
/// attempts run out.
pub async fn with_retry<T, F, Fut>(
    policy: RetryPolicy,
    what: &str,
    mut attempt: F,
) -> Result<T, PlatformError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, PlatformError>>,
{
    let attempts = policy.max_attempts.max(1);
    let mut number = 1;
    loop {
        match attempt().await {
            Ok(value) => return Ok(value),
            Err(error) if error.is_retryable() && number < attempts => {
                wait(policy, what, number, &error).await;
                number += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Creates something that must exist once, such as a comment: `create`,
/// and when it fails in a way another try might not, `find` first, since
/// the platform may have created it before it failed. What `find` returns
/// is taken as created; when it finds nothing, `create` is tried again.
///
/// # Errors
///
/// As [`with_retry`]. When `find` fails, the create's error is returned.
pub async fn create_once<T, C, CF, L, LF>(
    policy: RetryPolicy,
    what: &str,
    mut create: C,
    mut find: L,
) -> Result<T, PlatformError>
where
    C: FnMut() -> CF,
    CF: Future<Output = Result<T, PlatformError>>,
    L: FnMut() -> LF,
    LF: Future<Output = Result<Option<T>, PlatformError>>,
{
    let attempts = policy.max_attempts.max(1);
    let mut number = 1;
    loop {
        match create().await {
            Ok(value) => return Ok(value),
            Err(error) if error.is_retryable() && number < attempts => {
                wait(policy, what, number, &error).await;
                match find().await {
                    Ok(Some(found)) => {
                        warn!(
                            endpoint = what,
                            "the failed create had landed; not creating it again"
                        );
                        return Ok(found);
                    }
                    Ok(None) => number += 1,
                    Err(lookup) => {
                        warn!(endpoint = what, %lookup, "could not tell whether the failed create landed");
                        return Err(error);
                    }
                }
            }
            Err(error) => return Err(error),
        }
    }
}

async fn wait(policy: RetryPolicy, what: &str, number: u32, error: &PlatformError) {
    let pause = policy.delay_before(number + 1, error.retry_after());
    warn!(
        endpoint = what,
        attempt = number,
        status = error.http_status(),
        wait_ms = u64::try_from(pause.as_millis()).unwrap_or(u64::MAX),
        %error,
        "retrying platform request"
    );
    tokio::time::sleep(pause).await;
}
