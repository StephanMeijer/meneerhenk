//! The run store: what every backend can do.

use async_trait::async_trait;
use henk_domain::run::{EventId, RunId};
use time::OffsetDateTime;

use crate::types::{
    DayCounts, DraftDecision, DraftFilter, DraftGroup, DraftListing, DraftRates, DraftRecord,
    EventFilter, EventRecord, EventWithOutcomes, FindingAction, FindingRecord, InboundEvent,
    LaneRecord, LaneStatus, NewRun, OutcomeRecord, Page, PruneCounts, RunFilter, RunRecord,
    RunStatus, Stage, StageRecord, StageState, StageWrite, StoreError, ToolCallFilter,
    ToolCallListing, ToolCallRecord, ToolUsage, TranscriptRecord, TranscriptSummary,
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

    /// Ends a review as superseded by `by`, the review of a newer commit
    /// that replaced it (#231), with the reason.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure.
    async fn supersede_run(&self, id: &RunId, by: &RunId, reason: &str) -> Result<(), StoreError>;

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

    /// Records where a stage of a run stands (#226). The first write of a
    /// stage sets when it started; a write that ends it sets when.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure.
    async fn stage(&self, run: &RunId, write: &StageWrite) -> Result<(), StoreError>;

    /// Ends every stage of `run` still running as failed, with `reason`:
    /// for a run that ended while one was going.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure.
    async fn fail_running_stages(&self, run: &RunId, reason: &str) -> Result<(), StoreError>;

    /// A run's stages, in the order they come.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure or a corrupt row.
    async fn stages(&self, run: &RunId) -> Result<Vec<StageRecord>, StoreError>;

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

    /// Records a review lane's draft as queued (#189), or its new text when
    /// the lane replaced it. Its `decision` is ignored.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure.
    async fn record_draft(&self, run: &RunId, draft: &DraftRecord) -> Result<(), StoreError>;

    /// Records what became of draft `draft` (`d3`) of a run.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure.
    async fn decide_draft(
        &self,
        run: &RunId,
        draft: &str,
        decision: &DraftDecision,
    ) -> Result<(), StoreError>;

    /// The drafts of a run, in the order they were queued.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure or a corrupt row.
    async fn drafts(&self, run: &RunId) -> Result<Vec<DraftRecord>, StoreError>;

    /// What became of the drafts `filter` matches, per `group`, the
    /// largest group first (#205). The filter's verdict and keyset do not
    /// apply.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure or a bad time.
    async fn draft_rates(
        &self,
        group: DraftGroup,
        filter: &DraftFilter,
    ) -> Result<Vec<DraftRates>, StoreError>;

    /// Drafts across runs, newest first, by filter and page (#205).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure, a bad time or a
    /// corrupt row.
    async fn list_drafts(
        &self,
        filter: &DraftFilter,
        page: Page,
    ) -> Result<Vec<DraftListing>, StoreError>;

    /// Records one tool call of a session (#190). An empty `at` means now.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure.
    async fn record_tool_call(&self, run: &RunId, call: &ToolCallRecord) -> Result<(), StoreError>;

    /// The tool calls of a run, in the order they were recorded.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure or a corrupt row.
    async fn tool_calls(&self, run: &RunId) -> Result<Vec<ToolCallRecord>, StoreError>;

    /// Tool usage across runs since `since`, per model, kind of session
    /// ([`crate::session_kind`]) and tool: the data for metrics.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure.
    async fn tool_usage_since(&self, since: OffsetDateTime) -> Result<Vec<ToolUsage>, StoreError> {
        let filter = ToolCallFilter {
            since: Some(
                since
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned()),
            ),
            ..ToolCallFilter::default()
        };
        self.tool_usage(&filter).await
    }

    /// Tool usage across runs, per model, kind of session and tool, for the
    /// calls `filter` matches (#203). Its outcome and keyset do not apply.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure or a bad time.
    async fn tool_usage(&self, filter: &ToolCallFilter) -> Result<Vec<ToolUsage>, StoreError>;

    /// Tool calls across runs, newest first, by filter and page (#203).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure, a bad time or a
    /// corrupt row.
    async fn list_tool_calls(
        &self,
        filter: &ToolCallFilter,
        page: Page,
    ) -> Result<Vec<ToolCallListing>, StoreError>;

    /// Stores one session's whole conversation (#191). An empty `at` means
    /// now.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure.
    async fn record_transcript(
        &self,
        run: &RunId,
        transcript: &TranscriptRecord,
    ) -> Result<(), StoreError>;

    /// The transcripts a run has, without their bodies, in the order they
    /// were stored.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure or a corrupt row.
    async fn transcripts(&self, run: &RunId) -> Result<Vec<TranscriptSummary>, StoreError>;

    /// The latest transcript of `session` in a run, with its body.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure or a corrupt row.
    async fn transcript(
        &self,
        run: &RunId,
        session: &str,
    ) -> Result<Option<TranscriptRecord>, StoreError>;

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

    /// Deletes inbound events received before `older_than`, with their
    /// outcomes, and session transcripts recorded before it (#191), in one
    /// transaction. Runs, their lanes, findings and tool calls are kept:
    /// links to runs are posted on the platforms (§8.6).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure; nothing is deleted then.
    async fn prune_events(&self, older_than: OffsetDateTime) -> Result<PruneCounts, StoreError>;

    /// Runs matching `filter`, newest first, one page of them.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure or a corrupt row.
    async fn list_runs(&self, filter: &RunFilter, page: Page)
    -> Result<Vec<RunRecord>, StoreError>;

    /// What happened per UTC day since `since` (#225): runs started,
    /// finished and failed, findings posted and drafts written. Oldest day
    /// first; days with nothing are left out.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure.
    async fn daily_stats(&self, since: OffsetDateTime) -> Result<Vec<DayCounts>, StoreError>;

    /// The lanes of each of `runs` as (run, lane, status), for lists that
    /// show where each run's lanes stand, in one read.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure or a corrupt row.
    async fn lanes_of(
        &self,
        runs: &[RunId],
    ) -> Result<Vec<(RunId, String, LaneStatus)>, StoreError>;

    /// The stages of each of `runs` as (run, stage, state), in one read.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure or a corrupt row.
    async fn stages_of(
        &self,
        runs: &[RunId],
    ) -> Result<Vec<(RunId, Stage, StageState)>, StoreError>;

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
