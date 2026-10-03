//! Events from hooks, the bus that delivers them to listeners, and the local
//! record of both.
//!
//! A hook (a webhook route, later a Discord gateway or a mail poller) turns
//! what it receives into an [`Event`] and publishes it. The [`EventBus`]
//! records the event, hands it to every [`Listener`] and records what each
//! did. Nothing here does HTTP or holds a credential; the binary wires the
//! concrete hooks and listeners.

pub mod bus;
pub mod event;
pub mod github;
pub mod gitlab;

pub use bus::{EventBus, EventRecorder, Hook, Listener};
pub use event::{CommentKind, Event, EventKind, EventSource, Handled, PullRequestAction, Sender};
pub use github::parse_github;
pub use gitlab::parse_gitlab;
