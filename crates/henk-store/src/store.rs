//! The run store: what every backend can do.

use async_trait::async_trait;
use henk_domain::run::{EventId, RunId};
use time::OffsetDateTime;

use crate::types::{
    EventFilter, EventRecord, EventWithOutcomes, FindingAction, FindingRecord, InboundEvent,
    LaneRecord, LaneStatus, NewRun, OutcomeRecord, Page, RunFilter, RunRecord, RunStatus,
    StoreError,
};

/// Run records (spec §1.1, §8.6): runs, lanes, findings, timelines, and
/// inbound events with what each listener did with them. Implemented over
/// `SQLite` ([`crate::SqliteStore`]) and `PostgreSQL`.
#[async_trait]
pub trait RunStore: Send + Sync + std::fmt::Debug {
    /// Records a new run as running.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure, including a duplicate id.
    async fn create_run(&self, run: &NewRun) -> Result<(), StoreError>;

    /// Ends a run.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure.
    async fn finish_run(
        &self,
        id: &RunId,
        status: RunStatus,
        summary: Option<&str>,
        error: Option<&str>,
    ) -> Result<(), StoreError>;

    /// Reads a run.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure or a corrupt row.
    async fn run(&self, id: &RunId) -> Result<Option<RunRecord>, StoreError>;

    /// Records that the process running `id` is alive.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure.
    async fn heartbeat(&self, id: &RunId) -> Result<(), StoreError>;

    /// Stores the platform's id for the review's check.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure.
    async fn set_check(&self, id: &RunId, check_id: &str) -> Result<(), StoreError>;

    /// Runs still `running` whose heartbeat is older than `stale_before`,
    /// or that never had one: a process that died left them (#7).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure or a corrupt row.
    async fn orphaned_runs(
        &self,
        stale_before: OffsetDateTime,
    ) -> Result<Vec<RunRecord>, StoreError>;

    /// Drops the lanes of `run` that are still running, with `reason`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure.
    async fn drop_running_lanes(&self, run: &RunId, reason: &str) -> Result<(), StoreError>;

    /// Records a lane as running.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure.
    async fn start_lane(&self, run: &RunId, name: &str, model: &str) -> Result<(), StoreError>;

    /// Ends a lane.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure.
    #[expect(
        clippy::too_many_arguments,
        reason = "one argument per column of a single UPDATE"
    )]
    async fn finish_lane(
        &self,
        run: &RunId,
        name: &str,
        status: LaneStatus,
        turns: u64,
        input_tokens: u64,
        output_tokens: u64,
        error: Option<&str>,
    ) -> Result<(), StoreError>;

    /// The lanes of a run.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure or a corrupt row.
    async fn lanes(&self, run: &RunId) -> Result<Vec<LaneRecord>, StoreError>;

    /// Records what happened to a finding.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure.
    async fn record_finding(
        &self,
        run: &RunId,
        lane: &str,
        path: &str,
        line_number: u32,
        comment_id: &str,
        action: FindingAction,
    ) -> Result<(), StoreError>;

    /// The finding actions of a run, oldest first.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure.
    async fn findings(&self, run: &RunId) -> Result<Vec<FindingRecord>, StoreError>;

    /// Adds a line to a run's timeline.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure.
    async fn event(&self, run: &RunId, level: &str, message: &str) -> Result<(), StoreError>;

    /// The timeline of a run, oldest first.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure.
    async fn events(&self, run: &RunId) -> Result<Vec<EventRecord>, StoreError>;

    /// Notes that another request joined a running run.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure.
    async fn joined(&self, run: &RunId, source: &str) -> Result<(), StoreError>;

    /// Records an inbound event. A payload over [`crate::MAX_PAYLOAD_BYTES`] is dropped.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure, including a duplicate id.
    async fn record_event(&self, event: &InboundEvent) -> Result<(), StoreError>;

    /// Records what a listener did with an event.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure.
    async fn record_outcome(&self, outcome: &OutcomeRecord) -> Result<(), StoreError>;

    /// Reads one event.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure or a corrupt row.
    async fn inbound_event(&self, id: &EventId) -> Result<Option<InboundEvent>, StoreError>;

    /// The outcomes of one event, in recording order.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure.
    async fn outcomes(&self, id: &EventId) -> Result<Vec<OutcomeRecord>, StoreError>;

    /// The events whose outcomes point at a run, oldest first.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure or a corrupt row.
    async fn inbound_events_for_run(&self, run: &RunId) -> Result<Vec<InboundEvent>, StoreError>;

    /// Runs matching `filter`, newest first, one page of them.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure or a corrupt row.
    async fn list_runs(&self, filter: &RunFilter, page: Page)
    -> Result<Vec<RunRecord>, StoreError>;

    /// How many runs match `filter`, across every page.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure.
    async fn count_runs(&self, filter: &RunFilter) -> Result<u64, StoreError>;

    /// Inbound events matching `filter`, newest first, one page of them,
    /// each with its outcomes.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure or a corrupt row.
    async fn list_inbound_events(
        &self,
        filter: &EventFilter,
        page: Page,
    ) -> Result<Vec<EventWithOutcomes>, StoreError>;
}
