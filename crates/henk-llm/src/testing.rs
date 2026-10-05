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
