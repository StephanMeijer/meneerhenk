//! The event recorder over the run store. Recordings stay in this database.

use std::sync::Arc;

use henk_domain::run::EventId;
use henk_events::{Event, EventRecorder, Handled};
use henk_store::{InboundEvent, OutcomeRecord, RunStore};

/// Records events and outcomes in [`RunStore`].
#[derive(Debug, Clone)]
pub struct StoreRecorder(pub Arc<dyn RunStore>);

#[async_trait::async_trait]
impl EventRecorder for StoreRecorder {
    async fn record_event(&self, event: &Event) -> Result<(), String> {
        let (repo, target) = event.kind.locator().map_or((None, None), |(repo, number)| {
            (Some(repo.path()), Some(number))
        });
        self.0
            .record_event(&InboundEvent {
                id: event.id.clone(),
                received_at: event.received_at.clone(),
                source: event.source.name().to_owned(),
                kind: event.kind.name().to_owned(),
                repo,
                target,
                payload: event.payload.as_ref().map(serde_json::Value::to_string),
            })
            .await
            .map_err(|e| e.to_string())
    }

    async fn record_outcome(
        &self,
        event: &EventId,
        listener: &str,
        outcome: &Handled,
    ) -> Result<(), String> {
        self.0
            .record_outcome(&OutcomeRecord {
                event_id: event.clone(),
                listener: listener.to_owned(),
                outcome: outcome.name().to_owned(),
                detail: outcome.detail(),
                run_id: outcome.run().map(ToString::to_string),
                at: String::new(),
            })
            .await
            .map_err(|e| e.to_string())
    }
}
