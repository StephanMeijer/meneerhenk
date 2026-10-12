//! A scripted [`ModelClient`] for tests of callers.

use std::sync::Mutex;
use std::time::Duration;

use crate::client::ModelClient;
use crate::error::LlmError;
use crate::types::{Completion, CompletionRequest};

/// Returns scripted completions in order and records every request.
///
/// When the script runs out, it returns a [`LlmError::BadRequest`] so a
/// caller that loops forever fails loudly.
#[derive(Debug, Default)]
pub struct ScriptedClient {
    model: String,
    script: Mutex<std::collections::VecDeque<Result<Completion, LlmError>>>,
    requests: Mutex<Vec<CompletionRequest>>,
    delay: Duration,
}

impl ScriptedClient {
    /// Builds a client that answers with `completions`, one per call.
    #[must_use]
    pub fn new(
        model: impl Into<String>,
        completions: impl IntoIterator<Item = Result<Completion, LlmError>>,
    ) -> Self {
        Self {
            model: model.into(),
            script: Mutex::new(completions.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
            delay: Duration::ZERO,
        }
    }

    /// Makes every call take `delay` before answering, for deadline tests.
    #[must_use]
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// Every request seen so far.
    #[must_use]
    pub fn requests(&self) -> Vec<CompletionRequest> {
        self.requests.lock().map(|r| r.clone()).unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl ModelClient for ScriptedClient {
    fn model(&self) -> &str {
        &self.model
    }

    async fn complete(&self, request: &CompletionRequest) -> Result<Completion, LlmError> {
        if let Ok(mut requests) = self.requests.lock() {
            requests.push(request.clone());
        }
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        let next = self
            .script
            .lock()
            .ok()
            .and_then(|mut script| script.pop_front());
        next.unwrap_or_else(|| {
            Err(LlmError::BadRequest {
                status: 0,
                body: "scripted client exhausted".to_owned(),
            })
        })
    }
}

/// Asserts that `after`, the first request of a resumed round, starts with
/// `before`, the last request of the round before, byte for byte: the same
/// system prompt and tools, the earlier messages unchanged, then only the
/// answer to `before` and the update (#321, #330). That is what lets every
/// resumed turn read the earlier conversation from the prompt cache.
///
/// # Panics
///
/// When it does not.
pub fn assert_resumed_with_only_the_update(before: &CompletionRequest, after: &CompletionRequest) {
    assert_eq!(before.system, after.system, "the system prompt changed");
    assert_eq!(before.tools, after.tools, "the tools changed");
    assert!(
        after.messages.len() >= before.messages.len(),
        "the conversation shrank"
    );
    assert_eq!(
        after.messages.get(..before.messages.len()),
        Some(&before.messages[..]),
        "the earlier round is unchanged"
    );
    assert_eq!(
        after.messages.len(),
        before.messages.len() + 2,
        "only its answer and the update follow"
    );
}

/// A [`ScriptedClient`] behind a prompt cache that works the way
/// Anthropic's reports it (#330): a request's prefix that an earlier
/// request already sent (the system prompt and tools, then whole messages)
/// is read from the cache, and the rest is written into it. Sizes are in
/// characters of the requests' debug form, which is enough to see whether
/// a turn read its history from the cache. Usage the script gives is
/// replaced.
#[derive(Debug, Default)]
pub struct CachingClient {
    inner: ScriptedClient,
    seen: Mutex<Vec<CompletionRequest>>,
    usage: Mutex<Vec<(u64, u64)>>,
}

impl CachingClient {
    /// A cache in front of `inner`.
    #[must_use]
    pub fn new(inner: ScriptedClient) -> Self {
        Self {
            inner,
            seen: Mutex::new(Vec::new()),
            usage: Mutex::new(Vec::new()),
        }
    }

    /// What each request read from the cache and wrote into it, in order.
    #[must_use]
    pub fn cache_usage(&self) -> Vec<(u64, u64)> {
        self.usage.lock().map(|u| u.clone()).unwrap_or_default()
    }

    /// Every request seen so far.
    #[must_use]
    pub fn requests(&self) -> Vec<CompletionRequest> {
        self.inner.requests()
    }

    /// What `request` reads from the cache and what it writes into it.
    fn cached(&self, request: &CompletionRequest) -> (u64, u64) {
        let size = |text: String| u64::try_from(text.chars().count()).unwrap_or(u64::MAX);
        let head = size(format!("{:?}{:?}", request.system, request.tools));
        let parts: Vec<u64> = request
            .messages
            .iter()
            .map(|m| size(format!("{m:?}")))
            .collect();
        let total = head + parts.iter().sum::<u64>();
        let Ok(mut seen) = self.seen.lock() else {
            return (0, total);
        };
        let shared = seen
            .iter()
            .filter(|earlier| earlier.system == request.system && earlier.tools == request.tools)
            .map(|earlier| {
                earlier
                    .messages
                    .iter()
                    .zip(&request.messages)
                    .take_while(|(a, b)| a == b)
                    .count()
            })
            .max();
        seen.push(request.clone());
        match shared {
            None => (0, total),
            Some(messages) => {
                let read = head + parts.iter().take(messages).sum::<u64>();
                (read, total - read)
            }
        }
    }
}

#[async_trait::async_trait]
impl ModelClient for CachingClient {
    fn model(&self) -> &str {
        self.inner.model()
    }

    async fn complete(&self, request: &CompletionRequest) -> Result<Completion, LlmError> {
        let (read, written) = self.cached(request);
        if let Ok(mut usage) = self.usage.lock() {
            usage.push((read, written));
        }
        let mut completion = self.inner.complete(request).await?;
        completion.usage.input_tokens = 0;
        completion.usage.cache_read_tokens = read;
        completion.usage.cache_write_tokens = written;
        Ok(completion)
    }
}
