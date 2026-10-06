//! Starts address runs (§3.5) for direct requests.

use std::sync::Arc;

use henk_events::{Event, EventKind, Handled, Listener};
use tracing::instrument;

use crate::coordinator::Coordinator;

/// Turns `AddressRequested` into a background address run.
pub struct AddressListener {
    coordinator: Arc<Coordinator>,
}

impl AddressListener {
    /// Builds the listener.
    #[must_use]
    pub fn new(coordinator: Arc<Coordinator>) -> Self {
        Self { coordinator }
    }
}

#[async_trait::async_trait]
impl Listener for AddressListener {
    fn name(&self) -> &'static str {
        "address"
    }

    #[instrument(skip_all, fields(event = %event.id))]
    async fn handle(&self, event: Arc<Event>) -> Handled {
        let EventKind::AddressRequested {
            target,
            note,
            requester,
        } = &event.kind
        else {
            return Handled::Ignored("not an address request".to_owned());
        };
        let app = self.coordinator.app();
        if app.settings.address.is_none() {
            return Handled::Ignored("address runs are not configured".to_owned());
        }
        if !app.settings.allowlist.allows(&target.repo) {
            return Handled::Ignored(format!("{} is not on the allowlist", target.repo));
        }
        let trigger = requester
            .as_deref()
            .map_or_else(|| "requested".to_owned(), |r| format!("requested by {r}"));
        match self.coordinator.submit_address(
            target.clone(),
            note.clone(),
            trigger,
            requester.clone(),
        ) {
            Ok(run) => Handled::Started(run),
            Err(why) => Handled::Ignored(why),
        }
    }
}
