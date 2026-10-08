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
    DayCounts, DraftDecision, DraftFilter, DraftGroup, DraftKey, DraftListing, DraftRates,
    DraftRecord, DraftVerdict, EventFilter, EventKey, EventRecord, EventWithOutcomes,
    FindingAction, FindingRecord, InboundEvent, LaneRecord, LaneStatus, MAX_PAYLOAD_BYTES, NewRun,
    OutcomeFilter, OutcomeRecord, Page, PruneCounts, RunFilter, RunKey, RunRecord, RunStatus,
    Stage, StageRecord, StageState, StageWrite, StoreError, ToolCallFilter, ToolCallKey,
    ToolCallListing, ToolCallRecord, ToolTally, ToolUsage, TranscriptRecord, TranscriptSummary,
    VerdictFilter, session_kind,
};
