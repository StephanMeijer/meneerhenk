//! `PostgreSQL`: a small connection pool, TLS on rustls with the platform's
//! roots, and native timestamps. Behaves exactly like [`crate::SqliteStore`];
//! `tests/conformance.rs` holds both to that.

use std::time::Duration;

use async_trait::async_trait;
use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod};
use henk_domain::run::{EventId, RunId};
use rustls_platform_verifier::BuilderVerifierExt as _;
use time::OffsetDateTime;
use tokio_postgres::config::Host;
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::db::Store;
use crate::store::RunStore;
use crate::types::{
    DayCounts, DraftDecision, DraftRecord, EventFacets, EventFilter, EventRecord,
    EventWithOutcomes, FindingAction, FindingRecord, InboundEvent, NewRun, OutcomeRecord, Page,
    PruneCounts, RoundRecord, RunFilter, RunRecord, RunStatus, SessionEnding, SessionRecord,
    SessionRole, SessionStatus, Stage, StageRecord, StageState, StageWrite, StoreError, TokenUsage,
    ToolCallFilter, ToolCallListing, ToolCallRecord, ToolUsage, TranscriptRecord,
    TranscriptSummary,
};

/// Schema migrations, applied in order. Only ever append.
const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/postgres/001_initial.sql"),
    include_str!("../migrations/postgres/002_dashboard.sql"),
    include_str!("../migrations/postgres/003_event_requester.sql"),
    include_str!("../migrations/postgres/004_tool_calls.sql"),
    include_str!("../migrations/postgres/005_transcripts.sql"),
    include_str!("../migrations/postgres/006_drafts.sql"),
    include_str!("../migrations/postgres/007_superseded_by.sql"),
    include_str!("../migrations/postgres/008_stages.sql"),
    include_str!("../migrations/postgres/009_stats.sql"),
    include_str!("../migrations/postgres/010_loop.sql"),
    include_str!("../migrations/postgres/011_lane_cache.sql"),
    include_str!("../migrations/postgres/012_sessions.sql"),
    include_str!("../migrations/postgres/013_rounds.sql"),
    include_str!("../migrations/postgres/014_reasoning_tokens.sql"),
    include_str!("../migrations/postgres/015_tool_names.sql"),
    include_str!("../migrations/postgres/016_run_children.sql"),
];

/// Serialises migrations between Henk processes starting together.
const MIGRATION_LOCK: i64 = 0x4865_6e6b; // "Henk"

/// Connections kept open at most. Henk's writes are small and few.
const POOL_SIZE: usize = 8;

/// How long a new connection may take when the URL does not say.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The run store over `PostgreSQL`.
#[derive(Debug)]
pub struct PgStore {
    /// Every method of the trait goes here (#598). The old migrations ran
    /// through a pool of their own when the store was connected, and it is
    /// gone again.
    store: Store,
}

impl PgStore {
    /// Connects to the database at `url` and brings its schema up to date.
    /// `sslmode` in the URL decides TLS, as usual for `PostgreSQL`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the URL does not parse, the server cannot
    /// be reached, or migrating fails. The URL itself is never in the error.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        Self::connect_up_to(url, MIGRATIONS.len(), true).await
    }

    /// Connects like [`Self::connect`], but migrates only up to schema
    /// `version`: for tests that seed rows the way an older Henk wrote
    /// them, then connect again to migrate the rest.
    ///
    /// # Errors
    ///
    /// As [`Self::connect`].
    #[cfg(feature = "testing")]
    pub async fn connect_at(url: &str, version: usize) -> Result<Self, StoreError> {
        Self::connect_up_to(url, version.min(MIGRATIONS.len()), false).await
    }

    /// `migrate_new` says whether the new layer migrates too: off for a
    /// database that is left at an old schema on purpose.
    async fn connect_up_to(
        url: &str,
        version: usize,
        migrate_new: bool,
    ) -> Result<Self, StoreError> {
        let mut config = parse_url(url)?;
        if config.get_connect_timeout().is_none() {
            config.connect_timeout(CONNECT_TIMEOUT);
        }
        let manager = Manager::from_config(
            config,
            tls()?,
            ManagerConfig {
                recycling_method: RecyclingMethod::Fast,
            },
        );
        let pool = Pool::builder(manager)
            .max_size(POOL_SIZE)
            .build()
            .map_err(|error| StoreError::Connect(error.to_string()))?;
        Self::migrate(&pool, version).await?;
        // The new layer second: it adopts what the migrations above leave.
        let store = Store::postgres(url, migrate_new).await?;
        Ok(Self { store })
    }

    /// Applies the migrations up to `upto` the database lacks.
    async fn migrate(pool: &Pool, upto: usize) -> Result<(), StoreError> {
        let mut client = pool.get().await?;
        let transaction = client.transaction().await?;
        transaction
            .execute("SELECT pg_advisory_xact_lock($1)", &[&MIGRATION_LOCK])
            .await?;
        transaction
            .batch_execute(
                "CREATE TABLE IF NOT EXISTS henk_schema_migrations (
                    version    INTEGER PRIMARY KEY,
                    applied_at TIMESTAMPTZ NOT NULL DEFAULT now()
                 )",
            )
            .await?;
        let applied: i32 = transaction
            .query_one(
                "SELECT COALESCE(MAX(version), 0) FROM henk_schema_migrations",
                &[],
            )
            .await?
            .try_get(0)?;
        let known = i32::try_from(MIGRATIONS.len()).unwrap_or(i32::MAX);
        if applied > known {
            return Err(StoreError::Schema(format!(
                "the database is at schema version {applied}, newer than the {known} this Henk knows"
            )));
        }
        for (version, sql) in (1..).zip(MIGRATIONS.iter().take(upto)) {
            if version <= applied {
                continue;
            }
            transaction.batch_execute(sql).await?;
            transaction
                .execute(
                    "INSERT INTO henk_schema_migrations (version) VALUES ($1)",
                    &[&version],
                )
                .await?;
        }
        transaction.commit().await?;
        Ok(())
    }
}

/// Where `url` points, without user or password: for logs and `henk doctor`.
///
/// # Errors
///
/// Returns [`StoreError::Connect`] when the URL does not parse.
pub fn describe_url(url: &str) -> Result<String, StoreError> {
    let config = parse_url(url)?;
    let ports = config.get_ports();
    let hosts: Vec<String> = config
        .get_hosts()
        .iter()
        .enumerate()
        .map(|(i, host)| {
            let host = match host {
                Host::Tcp(name) => name.clone(),
                Host::Unix(path) => path.display().to_string(),
            };
            match ports.get(i).or_else(|| ports.first()) {
                Some(port) => format!("{host}:{port}"),
                None => host,
            }
        })
        .collect();
    Ok(format!(
        "{}/{}",
        hosts.join(","),
        config.get_dbname().unwrap_or("")
    ))
}

fn parse_url(url: &str) -> Result<tokio_postgres::Config, StoreError> {
    // The parse error can quote the URL, and the URL holds the password.
    url.parse()
        .map_err(|_| StoreError::Connect("the database URL does not parse".to_owned()))
}

fn tls() -> Result<MakeRustlsConnect, StoreError> {
    let config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|error| StoreError::Connect(error.to_string()))?
    .with_platform_verifier()
    .map_err(|error| StoreError::Connect(error.to_string()))?
    .with_no_client_auth();
    Ok(MakeRustlsConnect::new(config))
}

#[async_trait]
impl RunStore for PgStore {
    async fn create_run(&self, run: &NewRun) -> Result<(), StoreError> {
        self.store.create_run(run).await
    }

    async fn finish_run(
        &self,
        id: &RunId,
        status: RunStatus,
        summary: Option<&str>,
        error: Option<&str>,
    ) -> Result<(), StoreError> {
        self.store.finish_run(id, status, summary, error).await
    }

    async fn supersede_run(&self, id: &RunId, by: &RunId, reason: &str) -> Result<(), StoreError> {
        self.store.supersede_run(id, by, reason).await
    }

    async fn run(&self, id: &RunId) -> Result<Option<RunRecord>, StoreError> {
        self.store.run(id).await
    }

    async fn heartbeat(&self, id: &RunId) -> Result<(), StoreError> {
        self.store.heartbeat(id).await
    }

    async fn end_loop(&self, id: &RunId, stop: &str, rounds: u32) -> Result<(), StoreError> {
        self.store.end_loop(id, stop, rounds).await
    }

    async fn set_check(&self, id: &RunId, check_id: &str) -> Result<(), StoreError> {
        self.store.set_check(id, check_id).await
    }

    async fn orphaned_runs(
        &self,
        stale_before: OffsetDateTime,
    ) -> Result<Vec<RunRecord>, StoreError> {
        self.store.orphaned_runs(stale_before).await
    }

    async fn latest_reviews(&self, since: OffsetDateTime) -> Result<Vec<RunRecord>, StoreError> {
        self.store.latest_reviews(since).await
    }

    async fn claim_resume(
        &self,
        run: &RunId,
        interrupted: &str,
        resumed: &str,
    ) -> Result<bool, StoreError> {
        self.store.claim_resume(run, interrupted, resumed).await
    }

    async fn drop_running_sessions(&self, run: &RunId, reason: &str) -> Result<(), StoreError> {
        self.store.drop_running_sessions(run, reason).await
    }

    async fn daily_stats(&self, since: OffsetDateTime) -> Result<Vec<DayCounts>, StoreError> {
        self.store.daily_stats(since).await
    }

    async fn sessions_of(
        &self,
        runs: &[RunId],
    ) -> Result<Vec<(RunId, String, SessionRole, SessionStatus)>, StoreError> {
        self.store.sessions_of(runs).await
    }

    async fn session_endings(
        &self,
        since: Option<OffsetDateTime>,
        reviews: u32,
    ) -> Result<Vec<SessionEnding>, StoreError> {
        self.store.session_endings(since, reviews).await
    }

    async fn stages_of(
        &self,
        runs: &[RunId],
    ) -> Result<Vec<(RunId, Stage, StageState)>, StoreError> {
        self.store.stages_of(runs).await
    }

    async fn stage(&self, run: &RunId, write: &StageWrite) -> Result<(), StoreError> {
        self.store.stage(run, write).await
    }

    async fn fail_running_stages(&self, run: &RunId, reason: &str) -> Result<(), StoreError> {
        self.store.fail_running_stages(run, reason).await
    }

    async fn stages(&self, run: &RunId) -> Result<Vec<StageRecord>, StoreError> {
        self.store.stages(run).await
    }

    async fn link_child(
        &self,
        parent: &RunId,
        stage: Stage,
        child: &RunId,
    ) -> Result<(), StoreError> {
        self.store.link_child(parent, stage, child).await
    }

    async fn children(&self, parent: &RunId) -> Result<Vec<(Stage, RunId)>, StoreError> {
        self.store.children(parent).await
    }

    async fn start_session(
        &self,
        run: &RunId,
        name: &str,
        role: SessionRole,
        model: &str,
    ) -> Result<(), StoreError> {
        self.store.start_session(run, name, role, model).await
    }

    async fn finish_session(
        &self,
        run: &RunId,
        name: &str,
        status: SessionStatus,
        turns: u64,
        tokens: TokenUsage,
        error: Option<&str>,
    ) -> Result<(), StoreError> {
        self.store
            .finish_session(run, name, status, turns, tokens, error)
            .await
    }

    async fn sessions(&self, run: &RunId) -> Result<Vec<SessionRecord>, StoreError> {
        self.store.sessions(run).await
    }

    async fn record_finding(
        &self,
        run: &RunId,
        session: &str,
        path: &str,
        line_number: u32,
        comment_id: &str,
        action: FindingAction,
    ) -> Result<(), StoreError> {
        self.store
            .record_finding(run, session, path, line_number, comment_id, action)
            .await
    }

    async fn record_draft(&self, run: &RunId, draft: &DraftRecord) -> Result<(), StoreError> {
        self.store.record_draft(run, draft).await
    }

    async fn decide_draft(
        &self,
        run: &RunId,
        draft: &str,
        decision: &DraftDecision,
    ) -> Result<(), StoreError> {
        self.store.decide_draft(run, draft, decision).await
    }

    async fn drafts(&self, run: &RunId) -> Result<Vec<DraftRecord>, StoreError> {
        self.store.drafts(run).await
    }

    async fn record_transcript(
        &self,
        run: &RunId,
        transcript: &TranscriptRecord,
    ) -> Result<(), StoreError> {
        self.store.record_transcript(run, transcript).await
    }

    async fn start_round(&self, run: &RunId, round: &RoundRecord) -> Result<(), StoreError> {
        self.store.start_round(run, round).await
    }

    async fn end_round(
        &self,
        run: &RunId,
        session: &str,
        round: u32,
        turns: u32,
    ) -> Result<(), StoreError> {
        self.store.end_round(run, session, round, turns).await
    }

    async fn rounds(&self, run: &RunId) -> Result<Vec<RoundRecord>, StoreError> {
        self.store.rounds(run).await
    }

    async fn transcripts(&self, run: &RunId) -> Result<Vec<TranscriptSummary>, StoreError> {
        self.store.transcripts(run).await
    }

    async fn transcript(
        &self,
        run: &RunId,
        session: &str,
    ) -> Result<Option<TranscriptRecord>, StoreError> {
        self.store.transcript(run, session).await
    }

    async fn record_tool_call(&self, run: &RunId, call: &ToolCallRecord) -> Result<(), StoreError> {
        self.store.record_tool_call(run, call).await
    }

    async fn tool_calls(&self, run: &RunId) -> Result<Vec<ToolCallRecord>, StoreError> {
        self.store.tool_calls(run).await
    }

    async fn tool_usage(&self, filter: &ToolCallFilter) -> Result<Vec<ToolUsage>, StoreError> {
        self.store.tool_usage(filter).await
    }

    async fn list_tool_calls(
        &self,
        filter: &ToolCallFilter,
        page: Page,
    ) -> Result<Vec<ToolCallListing>, StoreError> {
        self.store.list_tool_calls(filter, page).await
    }

    async fn findings(&self, run: &RunId) -> Result<Vec<FindingRecord>, StoreError> {
        self.store.findings(run).await
    }

    async fn event(&self, run: &RunId, level: &str, message: &str) -> Result<(), StoreError> {
        self.store.event(run, level, message).await
    }

    async fn events(&self, run: &RunId) -> Result<Vec<EventRecord>, StoreError> {
        self.store.events(run).await
    }

    async fn joined(&self, run: &RunId, source: &str) -> Result<(), StoreError> {
        self.store.joined(run, source).await
    }

    async fn record_event(&self, event: &InboundEvent) -> Result<(), StoreError> {
        self.store.record_event(event).await
    }

    async fn record_outcome(&self, outcome: &OutcomeRecord) -> Result<(), StoreError> {
        self.store.record_outcome(outcome).await
    }

    async fn inbound_event(&self, id: &EventId) -> Result<Option<InboundEvent>, StoreError> {
        self.store.inbound_event(id).await
    }

    async fn event_facets(&self) -> Result<EventFacets, StoreError> {
        self.store.event_facets().await
    }

    async fn outcomes(&self, id: &EventId) -> Result<Vec<OutcomeRecord>, StoreError> {
        self.store.outcomes(id).await
    }

    async fn prune_events(&self, older_than: OffsetDateTime) -> Result<PruneCounts, StoreError> {
        self.store.prune_events(older_than).await
    }

    async fn inbound_events_for_run(&self, run: &RunId) -> Result<Vec<InboundEvent>, StoreError> {
        self.store.inbound_events_for_run(run).await
    }

    async fn list_runs(
        &self,
        filter: &RunFilter,
        page: Page,
    ) -> Result<Vec<RunRecord>, StoreError> {
        self.store.list_runs(filter, page).await
    }

    async fn count_runs(&self, filter: &RunFilter) -> Result<u64, StoreError> {
        self.store.count_runs(filter).await
    }

    async fn list_inbound_events(
        &self,
        filter: &EventFilter,
        page: Page,
    ) -> Result<Vec<EventWithOutcomes>, StoreError> {
        self.store.list_inbound_events(filter, page).await
    }
}
