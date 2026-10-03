//! Sources of events inside `henk serve` (§3.1 triggers).
//!
//! Every hook implements [`henk_events::Hook`]. HTTP hooks also implement
//! [`HttpHook`] and mount their own routes; they verify what they receive,
//! turn it into an [`henk_events::Event`] and publish it. A hook never
//! decides what to do with an event; listeners do.

pub mod api;
pub mod github;
pub mod gitlab;

use std::sync::Arc;

use axum::Router;
use henk_events::{Event, EventBus, EventKind, EventSource};
use serde_json::Value;

pub use api::ApiHook;
pub use github::GitHubHook;
pub use gitlab::GitLabHook;

use crate::ids::new_event_id;

/// A hook that receives over HTTP.
pub trait HttpHook: henk_events::Hook {
    /// The routes to mount on the server.
    fn routes(self: Arc<Self>) -> Router;
}

/// Builds an event with a fresh id and the current time.
#[must_use]
pub fn new_event(source: EventSource, kind: EventKind, payload: Option<Value>) -> Event {
    Event {
        id: new_event_id(),
        received_at: now_rfc3339(),
        source,
        kind,
        payload,
    }
}

/// The current time, RFC 3339.
#[must_use]
pub fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// Publishes and returns the id, for a `202` body.
pub fn publish(bus: &Arc<EventBus>, event: Event) -> String {
    bus.publish(event).to_string()
}
