//! Run records in `SQLite`.
//!
//! Every run has a row and a link (spec §1.1, §8.6). The store is small
//! and synchronous; callers wrap it in `spawn_blocking` or accept the few
//! microseconds a local write takes.

pub mod store;

pub use store::{
    EventRecord, FindingAction, InboundEvent, LaneRecord, LaneStatus, MAX_PAYLOAD_BYTES, NewRun,
    OutcomeRecord, RunRecord, RunStatus, RunStore, StoreError,
};
