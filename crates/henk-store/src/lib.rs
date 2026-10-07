//! Run records in `SQLite` or `PostgreSQL`.
//!
//! Every run has a row and a link (spec §1.1, §8.6). [`RunStore`] is what
//! callers hold; [`SqliteStore`] keeps records in one local file and
//! [`PgStore`] in a `PostgreSQL` database.

mod postgres;
mod sqlite;
mod store;
mod types;

pub use postgres::{PgStore, describe_url};
pub use sqlite::SqliteStore;
pub use store::RunStore;
pub use types::{
    EventFilter, EventRecord, EventWithOutcomes, FindingAction, FindingRecord, InboundEvent,
    LaneRecord, LaneStatus, MAX_PAYLOAD_BYTES, NewRun, OutcomeRecord, Page, PruneCounts, RunFilter,
    RunRecord, RunStatus, StoreError, ToolCallRecord, ToolTally, ToolUsage, TranscriptRecord,
    TranscriptSummary, session_kind,
};
