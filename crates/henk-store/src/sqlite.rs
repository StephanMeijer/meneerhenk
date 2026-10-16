//! SQLite: one connection behind a mutex. Writes take microseconds, so the
//! async methods run them in place.

use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use henk_domain::run::{EventId, RunId};
use rusqlite::Connection;
#[cfg(test)]
use rusqlite::params;
use rusqlite_migration::{M, Migrations};
use time::OffsetDateTime;
#[cfg(test)]
use time::format_description::well_known::Rfc3339;

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

/// The run store over SQLite.
///
/// The methods that have moved to the new layer (#598) go to `store`, which
/// opens the same file; the rest still use `connection`. Both are on the one
/// database, so the file is shared by two libraries until the last group
/// moves.
#[derive(Debug)]
pub struct SqliteStore {
    /// For the unit tests, which read and change the database directly. The
    /// store itself no longer uses it: it migrates through it when opened.
    #[cfg(test)]
    connection: Mutex<Connection>,
    store: Store,
    /// The file behind [`SqliteStore::in_memory`], removed with the store.
    /// Declared last so the connections close before the file goes.
    _temporary: Option<TemporaryFile>,
}

/// A database file in a private directory of its own under the temp
/// directory. The directory is new (created, never reused, mode 0700 on
/// Unix), so nothing stale or planted can be there; it is removed with the
/// WAL files when this is dropped.
#[derive(Debug)]
struct TemporaryFile {
    directory: PathBuf,
    file: PathBuf,
}

impl TemporaryFile {
    fn new() -> Result<Self, StoreError> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        loop {
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let directory = std::env::temp_dir().join(format!(
                "henk-store-memory-{}-{n}-{nanos:x}",
                std::process::id()
            ));
            // `create` fails when the path exists, and does not follow a
            // symlink there, so a planted name is skipped, not used.
            match builder.create(&directory) {
                Ok(()) => {
                    let file = directory.join("store.db");
                    return Ok(Self { directory, file });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => {
                    return Err(StoreError::Connect(format!(
                        "temporary database directory: {e}"
                    )));
                }
            }
        }
    }
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn migrations() -> Migrations<'static> {
    Migrations::new(vec![
        M::up(include_str!("../migrations/sqlite/001_initial.sql")),
        M::up(include_str!("../migrations/sqlite/002_inbound_events.sql")),
        M::up(include_str!("../migrations/sqlite/003_run_liveness.sql")),
        M::up(include_str!("../migrations/sqlite/004_dashboard.sql")),
        M::up(include_str!("../migrations/sqlite/005_prune_events.sql")),
        M::up(include_str!("../migrations/sqlite/006_event_requester.sql")),
        M::up(include_str!("../migrations/sqlite/007_tool_calls.sql")),
        M::up(include_str!("../migrations/sqlite/008_transcripts.sql")),
        M::up(include_str!("../migrations/sqlite/009_drafts.sql")),
        M::up(include_str!("../migrations/sqlite/010_superseded_by.sql")),
        M::up(include_str!("../migrations/sqlite/011_stages.sql")),
        M::up(include_str!("../migrations/sqlite/012_stats.sql")),
        M::up(include_str!("../migrations/sqlite/013_run_instant.sql")),
        M::up(include_str!("../migrations/sqlite/014_loop.sql")),
        M::up(include_str!("../migrations/sqlite/015_lane_cache.sql")),
        M::up(include_str!("../migrations/sqlite/016_sessions.sql")),
        M::up(include_str!("../migrations/sqlite/017_rounds.sql")),
        M::up(include_str!(
            "../migrations/sqlite/018_reasoning_tokens.sql"
        )),
        M::up(include_str!("../migrations/sqlite/019_tool_names.sql")),
        M::up(include_str!("../migrations/sqlite/020_run_children.sql")),
    ])
}

impl SqliteStore {
    /// Opens or creates the database at `path` and migrates it. The name
    /// `:memory:` is a store that is gone when it is dropped, as SQLite
    /// itself reads it (see [`Self::in_memory`]).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the file cannot be opened or migrated.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        if path == Path::new(":memory:") {
            return Self::in_memory();
        }
        Self::migrated(Connection::open(path)?, None, Store::sqlite(path), None)
    }

    /// A store that is gone when it is dropped, for tests and probes. While
    /// two libraries share the database (#598) it is a file of its own under
    /// the temp directory, not memory: an in-memory database cannot be opened
    /// by both.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when SQLite cannot create the database.
    pub fn in_memory() -> Result<Self, StoreError> {
        let file = TemporaryFile::new()?;
        let connection = Connection::open(&file.file)?;
        let store = Store::sqlite(&file.file);
        Self::migrated(connection, None, store, Some(file))
    }

    /// Opens or creates the database at `path` and migrates it only up to
    /// schema `version`: for tests that seed rows the way an older Henk
    /// wrote them, then open the database again to migrate the rest.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the file cannot be opened or migrated.
    #[cfg(feature = "testing")]
    pub fn open_at(path: &Path, version: usize) -> Result<Self, StoreError> {
        Self::migrated(
            Connection::open(path)?,
            Some(version),
            Store::sqlite(path).unmigrated(),
            None,
        )
    }

    /// The store over `connection`, migrated up to `version`, or to the
    /// latest schema when `None`.
    fn migrated(
        mut connection: Connection,
        version: Option<usize>,
        store: Store,
        temporary: Option<TemporaryFile>,
    ) -> Result<Self, StoreError> {
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        match version {
            Some(version) => migrations().to_version(&mut connection, version)?,
            None => migrations().to_latest(&mut connection)?,
        }
        Ok(Self {
            #[cfg(test)]
            connection: Mutex::new(connection),
            store,
            _temporary: temporary,
        })
    }

    #[cfg(test)]
    fn with<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        f(&connection)
    }
}

#[async_trait]
impl RunStore for SqliteStore {
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

    async fn drop_running_sessions(&self, run: &RunId, reason: &str) -> Result<(), StoreError> {
        self.store.drop_running_sessions(run, reason).await
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::used_underscore_binding)]

    use henk_domain::allowlist::Platform;
    use henk_domain::run::RunKind;

    use super::*;

    fn new_run(id: &str) -> NewRun {
        NewRun {
            id: RunId::parse(id).unwrap(),
            kind: RunKind::Review,
            platform: Platform::GitHub,
            repo: "o/r".into(),
            target: 7,
            commit: Some("abc".into()),
            requester: None,
            trigger: "opened".into(),
            link: format!("https://henk.example/runs/{id}"),
        }
    }

    /// A stale file at a name an earlier version would have used does not
    /// leak into a new in-memory store, and the store's directory is private
    /// and gone with it.
    #[tokio::test]
    async fn an_in_memory_store_is_new_whatever_is_left_in_the_temp_directory() {
        let stale =
            std::env::temp_dir().join(format!("henk-store-memory-{}-0.db", std::process::id()));
        std::fs::write(&stale, b"not a database").unwrap();

        let store = SqliteStore::in_memory().unwrap();
        store.create_run(&new_run("r-fresh")).await.unwrap();
        let directory = store._temporary.as_ref().unwrap().directory.clone();
        assert!(directory.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&directory).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "the directory is private");
        }
        let other = SqliteStore::in_memory().unwrap();
        assert_ne!(
            directory,
            other._temporary.as_ref().unwrap().directory,
            "every store has a directory of its own"
        );
        assert!(
            other
                .with(|c| Ok(c.query_row("SELECT count(*) FROM runs", [], |r| r.get::<_, i64>(0))?))
                .unwrap()
                == 0
        );

        drop(store);
        assert!(!directory.exists(), "the directory goes with the store");
        assert_eq!(std::fs::read(&stale).unwrap(), b"not a database");
        std::fs::remove_file(&stale).unwrap();
    }

    /// #160: the newest review of a pull request is found by time, also
    /// when two start in the same second and their stored times differ in
    /// fraction width, and the window's bound compares the same way.
    #[tokio::test]
    async fn the_newest_review_compares_start_times_as_instants() {
        let store = SqliteStore::in_memory().unwrap();
        for (run, at) in [
            ("r-b", "2026-10-07T12:00:00Z"),
            ("r-a", "2026-10-07T12:00:00.4Z"),
        ] {
            store.create_run(&new_run(run)).await.unwrap();
            store
                .with(|c| {
                    c.execute(
                        "UPDATE runs SET started_at = ?1 WHERE id = ?2",
                        params![at, run],
                    )?;
                    Ok(())
                })
                .unwrap();
        }
        let at = |v: &str| OffsetDateTime::parse(v, &Rfc3339).unwrap();

        let latest = store
            .latest_reviews(at("2026-10-07T11:00:00Z"))
            .await
            .unwrap();
        let ids: Vec<&str> = latest.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["r-a"], "r-a started 0.4 s after r-b");

        let after_both = store
            .latest_reviews(at("2026-10-07T12:00:00.5Z"))
            .await
            .unwrap();
        assert!(after_both.is_empty(), "{after_both:?}");
    }

    #[tokio::test]
    async fn a_run_that_never_had_a_heartbeat_is_orphaned() {
        let store = SqliteStore::in_memory().unwrap();
        store.create_run(&new_run("r-silent")).await.unwrap();
        store
            .with(|c| {
                c.execute(
                    "UPDATE runs SET heartbeat_at = NULL WHERE id = 'r-silent'",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        let an_hour_ago = OffsetDateTime::now_utc() - time::Duration::hours(1);
        let orphans = store.orphaned_runs(an_hour_ago).await.unwrap();
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans.first().unwrap().id.as_str(), "r-silent");
    }

    #[tokio::test]
    async fn a_session_stored_as_dropped_before_231_reads_as_did_not_finish() {
        let store = SqliteStore::in_memory().unwrap();
        store.create_run(&new_run("r-old")).await.unwrap();
        let run = RunId::parse("r-old").unwrap();
        store
            .start_session(&run, "a", SessionRole::Reviewer, "m")
            .await
            .unwrap();
        store
            .with(|c| {
                c.execute(
                    "UPDATE sessions SET status = 'dropped', error = 'gave up' WHERE run_id = 'r-old'",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        let sessions = store.sessions(&run).await.unwrap();
        assert_eq!(
            sessions.first().unwrap().status,
            SessionStatus::DidNotFinish
        );
        assert_eq!(sessions.first().unwrap().error.as_deref(), Some("gave up"));
    }

    #[tokio::test]
    async fn joining_a_running_run_is_recorded() {
        let store = SqliteStore::in_memory().unwrap();
        store.create_run(&new_run("r-3")).await.unwrap();
        let run = RunId::parse("r-3").unwrap();
        store.joined(&run, "comment").await.unwrap();
        store.joined(&run, "webhook").await.unwrap();
        let sources: Vec<String> = store
            .with(|c| {
                let mut statement =
                    c.prepare("SELECT source FROM requests WHERE run_id = ?1 ORDER BY rowid")?;
                let rows = statement.query_map(params![run.as_str()], |row| row.get(0))?;
                Ok(rows.collect::<Result<_, _>>()?)
            })
            .unwrap();
        assert_eq!(
            sources,
            ["comment", "webhook"],
            "one row per join, in order"
        );
    }
}
