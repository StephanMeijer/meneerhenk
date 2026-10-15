//! Starts autonomous ticket runs (§4b, #536) for direct requests.

use std::sync::Arc;

use henk_events::{Event, EventKind, Handled, Listener};
use tracing::instrument;

use crate::coordinator::{Coordinator, Submitted};

/// Turns `TicketRequested` into a background ticket run: the issue is
/// planned, taken to a pull request and reviewed, with nobody starting each
/// step.
pub struct TicketListener {
    coordinator: Arc<Coordinator>,
}

impl TicketListener {
    /// Builds the listener.
    #[must_use]
    pub fn new(coordinator: Arc<Coordinator>) -> Self {
        Self { coordinator }
    }
}

#[async_trait::async_trait]
impl Listener for TicketListener {
    fn name(&self) -> &'static str {
        "ticket"
    }

    #[instrument(skip_all, fields(event = %event.id))]
    async fn handle(&self, event: Arc<Event>) -> Handled {
        let EventKind::TicketRequested {
            target,
            note,
            requester,
        } = &event.kind
        else {
            return Handled::Ignored("not a ticket request".to_owned());
        };
        let settings = &self.coordinator.app().settings;
        if settings.autonomous.is_none() {
            return Handled::Ignored("autonomous runs are not configured".to_owned());
        }
        if !settings.allowlist.allows(&target.repo) {
            return Handled::Ignored(format!("{} is not on the allowlist", target.repo));
        }
        let trigger = requester
            .as_deref()
            .map_or_else(|| "requested".to_owned(), |r| format!("requested by {r}"));
        match self.coordinator.submit_ticket(
            target.clone(),
            note.clone(),
            trigger,
            requester.clone(),
        ) {
            Ok(Submitted::Started(run)) => Handled::Started(run),
            Ok(Submitted::Joined(run)) => Handled::Joined(run),
            Err(error) => Handled::Failed(format!("{error:#}")),
        }
    }
}
