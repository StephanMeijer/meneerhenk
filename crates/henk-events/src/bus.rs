//! The bus: record, deliver to every listener, record the outcomes.

use std::sync::Arc;

use henk_domain::run::EventId;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{info, instrument, warn};

use crate::event::{Event, Handled};

/// Something that reacts to events.
#[async_trait::async_trait]
pub trait Listener: Send + Sync + 'static {
    /// A short, stable name for records.
    fn name(&self) -> &'static str;

    /// Handles one event. Must not panic; a panic is recorded as a failure
    /// and never reaches the hook.
    async fn handle(&self, event: Arc<Event>) -> Handled;
}

/// A source of events. HTTP hooks mount routes in the binary and leave
/// `run` alone; pollers and gateways do their work in `run` until cancelled.
#[async_trait::async_trait]
pub trait Hook: Send + Sync + 'static {
    /// A short, stable name.
    fn name(&self) -> &'static str;

    /// Long-running work, if the hook has any.
    async fn run(self: Arc<Self>, _bus: Arc<EventBus>, _cancel: CancellationToken) {}
}

/// Local persistence of events and outcomes. Implemented by the binary over
/// its run store; recordings never leave the service.
pub trait EventRecorder: Send + Sync {
    /// Records an event before any listener sees it.
    ///
    /// # Errors
    ///
    /// Returns a description of the storage failure. Delivery continues.
    fn record_event(&self, event: &Event) -> Result<(), String>;

    /// Records what one listener did with an event.
    ///
    /// # Errors
    ///
    /// Returns a description of the storage failure.
    fn record_outcome(
        &self,
        event: &EventId,
        listener: &str,
        outcome: &Handled,
    ) -> Result<(), String>;
}

/// A recorder that keeps nothing. For tests and probes.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoRecorder;

impl EventRecorder for NoRecorder {
    fn record_event(&self, _: &Event) -> Result<(), String> {
        Ok(())
    }

    fn record_outcome(&self, _: &EventId, _: &str, _: &Handled) -> Result<(), String> {
        Ok(())
    }
}

/// Delivers every event to every listener and records both.
pub struct EventBus {
    listeners: Vec<Arc<dyn Listener>>,
    recorder: Arc<dyn EventRecorder>,
}

impl std::fmt::Debug for EventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventBus")
            .field(
                "listeners",
                &self.listeners.iter().map(|l| l.name()).collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl EventBus {
    /// Builds a bus. The listener list is fixed for the life of the bus.
    #[must_use]
    pub fn new(recorder: Arc<dyn EventRecorder>, listeners: Vec<Arc<dyn Listener>>) -> Self {
        Self {
            listeners,
            recorder,
        }
    }

    /// The listener names, in delivery order.
    pub fn listeners(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.listeners.iter().map(|l| l.name())
    }

    /// Records the event, runs every listener concurrently, records and
    /// returns each outcome. Tests call this and assert on the result.
    #[instrument(skip_all, fields(event = %event.id, kind = event.kind.name(), source = event.source.name()))]
    pub async fn deliver(&self, event: Event) -> Vec<(&'static str, Handled)> {
        if let Err(error) = self.recorder.record_event(&event) {
            warn!(%error, "could not record the event");
        }
        let event = Arc::new(event);
        let mut set = JoinSet::new();
        let mut names = std::collections::HashMap::new();
        for listener in &self.listeners {
            let listener = Arc::clone(listener);
            let event = Arc::clone(&event);
            let name = listener.name();
            let handle = set.spawn(async move { listener.handle(event).await });
            names.insert(handle.id(), name);
        }
        let mut outcomes = Vec::with_capacity(self.listeners.len());
        while let Some(joined) = set.join_next_with_id().await {
            let (name, outcome) = match joined {
                Ok((id, outcome)) => (names.get(&id).copied().unwrap_or("unknown"), outcome),
                Err(error) => (
                    names.get(&error.id()).copied().unwrap_or("unknown"),
                    Handled::Failed(format!("listener panicked: {error}")),
                ),
            };
            if let Err(error) = self.recorder.record_outcome(&event.id, name, &outcome) {
                warn!(%error, listener = name, "could not record the outcome");
            }
            info!(listener = name, outcome = outcome.name(), detail = %outcome.detail(), "handled");
            outcomes.push((name, outcome));
        }
        outcomes.sort_by(|a, b| a.0.cmp(b.0));
        outcomes
    }

    /// Delivers on a spawned task so the caller, a hook, never waits.
    pub fn publish(self: &Arc<Self>, event: Event) -> EventId {
        let id = event.id.clone();
        let bus = Arc::clone(self);
        tokio::spawn(async move {
            let _ = bus.deliver(event).await;
        });
        id
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing
    )]

    use std::sync::Mutex;

    use henk_domain::run::RunId;

    use super::*;
    use crate::event::{EventKind, EventSource};

    #[derive(Default)]
    struct MemoryRecorder {
        events: Mutex<Vec<String>>,
        outcomes: Mutex<Vec<(String, String, String)>>,
    }

    impl EventRecorder for MemoryRecorder {
        fn record_event(&self, event: &Event) -> Result<(), String> {
            self.events.lock().unwrap().push(event.id.to_string());
            Ok(())
        }

        fn record_outcome(
            &self,
            event: &EventId,
            listener: &str,
            outcome: &Handled,
        ) -> Result<(), String> {
            self.outcomes.lock().unwrap().push((
                event.to_string(),
                listener.to_owned(),
                outcome.name().to_owned(),
            ));
            Ok(())
        }
    }

    struct Starts;
    struct Shrugs;
    struct Panics;

    #[async_trait::async_trait]
    impl Listener for Starts {
        fn name(&self) -> &'static str {
            "starts"
        }
        async fn handle(&self, _: Arc<Event>) -> Handled {
            Handled::Started(RunId::parse("r-1").unwrap())
        }
    }

    #[async_trait::async_trait]
    impl Listener for Shrugs {
        fn name(&self) -> &'static str {
            "shrugs"
        }
        async fn handle(&self, event: Arc<Event>) -> Handled {
            Handled::Ignored(format!("not for me: {}", event.kind.name()))
        }
    }

    #[async_trait::async_trait]
    impl Listener for Panics {
        fn name(&self) -> &'static str {
            "panics"
        }
        async fn handle(&self, _: Arc<Event>) -> Handled {
            panic!("boom")
        }
    }

    fn event() -> Event {
        Event {
            id: EventId::parse("e-1").unwrap(),
            received_at: "2026-10-03T00:00:00Z".into(),
            source: EventSource::Api { requester: None },
            kind: EventKind::Ignored("test".into()),
            payload: None,
        }
    }

    #[tokio::test]
    async fn delivers_to_every_listener_and_records_everything() {
        let recorder = Arc::new(MemoryRecorder::default());
        let bus = EventBus::new(
            recorder.clone(),
            vec![Arc::new(Starts), Arc::new(Shrugs), Arc::new(Panics)],
        );
        let outcomes = bus.deliver(event()).await;
        assert_eq!(outcomes.len(), 3);
        assert!(
            matches!(outcomes[0], ("panics", Handled::Failed(_))),
            "{outcomes:?}"
        );
        assert!(matches!(outcomes[1], ("shrugs", Handled::Ignored(_))));
        assert!(matches!(outcomes[2], ("starts", Handled::Started(_))));
        assert_eq!(recorder.events.lock().unwrap().as_slice(), ["e-1"]);
        let stored = recorder.outcomes.lock().unwrap();
        assert_eq!(stored.len(), 3);
        assert!(
            stored
                .iter()
                .any(|(_, l, o)| l == "panics" && o == "failed")
        );
    }

    #[tokio::test]
    async fn publish_returns_at_once_and_delivers_in_the_background() {
        let recorder = Arc::new(MemoryRecorder::default());
        let bus = Arc::new(EventBus::new(recorder.clone(), vec![Arc::new(Starts)]));
        let id = bus.publish(event());
        assert_eq!(id.as_str(), "e-1");
        for _ in 0..50 {
            if recorder.outcomes.lock().unwrap().len() == 1 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("outcome was not recorded");
    }
}
