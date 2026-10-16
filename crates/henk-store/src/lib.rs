//! Run records in `SQLite` or `PostgreSQL`.
//!
//! Every run has a row and a link (spec §1.1, §8.6). [`RunStore`] is what
//! callers hold; [`SqliteStore`] keeps records in one local file and
//! [`PgStore`] in a `PostgreSQL` database.

pub mod migrator;
mod postgres;
mod sqlite;
mod store;
mod types;

pub use postgres::{PgStore, describe_url};
pub use sqlite::SqliteStore;
pub use store::RunStore;
pub use types::{
    DayCounts, DraftDecision, DraftRecord, DraftVerdict, EventFacets, EventFilter, EventKey,
    EventRecord, EventWithOutcomes, FindingAction, FindingRecord, InboundEvent, LOOP_FINDING_KIND,
    MAX_PAYLOAD_BYTES, MOST_SESSION_REVIEWS, NewRun, OutcomeFilter, OutcomeRecord, PLAN_POINT_KIND,
    Page, PruneCounts, REQUIREMENT_POINT_KIND, RoundRecord, RunFilter, RunKey, RunRecord,
    RunStatus, SessionEnding, SessionRecord, SessionRole, SessionStatus, Stage, StageRecord,
    StageState, StageWrite, StoreError, TokenUsage, ToolCallFilter, ToolCallKey, ToolCallListing,
    ToolCallRecord, ToolTally, ToolUsage, TranscriptRecord, TranscriptSummary, UNKNOWN_TOOL,
    counted_as,
};
