//! Starts plans (§4) for direct requests.

use std::sync::Arc;

use henk_events::{Event, EventKind, Handled, Listener};
use tracing::instrument;

use crate::coordinator::Coordinator;

/// Turns `PlanRequested` into a background plan.
pub struct PlanListener {
    coordinator: Arc<Coordinator>,
}

impl PlanListener {
    /// Builds the listener.
    #[must_use]
    pub fn new(coordinator: Arc<Coordinator>) -> Self {
        Self { coordinator }
    }
}

#[async_trait::async_trait]
impl Listener for PlanListener {
    fn name(&self) -> &'static str {
        "plan"
    }

    #[instrument(skip_all, fields(event = %event.id))]
    async fn handle(&self, event: Arc<Event>) -> Handled {
        let EventKind::PlanRequested {
            target,
            note,
            requester,
        } = &event.kind
        else {
            return Handled::Ignored("not a plan request".to_owned());
        };
        if !self
            .coordinator
            .app()
            .settings
            .allowlist
            .allows(&target.repo)
        {
            return Handled::Ignored(format!("{} is not on the allowlist", target.repo));
        }
        let trigger = requester
            .as_deref()
            .map_or_else(|| "requested".to_owned(), |r| format!("requested by {r}"));
        Handled::Started(self.coordinator.submit_plan(
            target.clone(),
            note.clone(),
            trigger,
            requester.clone(),
        ))
    }
}
