//! Starts execute runs (§4a, #329) for direct requests.

use std::sync::Arc;

use henk_events::{Event, EventKind, Handled, Listener};
use tracing::instrument;

use crate::coordinator::Coordinator;

/// Turns `ExecuteRequested` into a background execute run, followed by a
/// review of the pull request it opens.
pub struct ExecuteListener {
    coordinator: Arc<Coordinator>,
}

impl ExecuteListener {
    /// Builds the listener.
    #[must_use]
    pub fn new(coordinator: Arc<Coordinator>) -> Self {
        Self { coordinator }
    }
}

#[async_trait::async_trait]
impl Listener for ExecuteListener {
    fn name(&self) -> &'static str {
        "execute"
    }

    #[instrument(skip_all, fields(event = %event.id))]
    async fn handle(&self, event: Arc<Event>) -> Handled {
        let EventKind::ExecuteRequested {
            target,
            note,
            requester,
        } = &event.kind
        else {
            return Handled::Ignored("not an execute request".to_owned());
        };
        let settings = &self.coordinator.app().settings;
        if settings.execution.is_none() {
            return Handled::Ignored("execution is not configured".to_owned());
        }
        if !settings.allowlist.allows(&target.repo) {
            return Handled::Ignored(format!("{} is not on the allowlist", target.repo));
        }
        let trigger = requester
            .as_deref()
            .map_or_else(|| "requested".to_owned(), |r| format!("requested by {r}"));
        Handled::Started(self.coordinator.submit_execute(
            target.clone(),
            note.clone(),
            trigger,
            requester.clone(),
        ))
    }
}
