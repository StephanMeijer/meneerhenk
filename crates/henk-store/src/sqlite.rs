//! SQLite: one connection behind a mutex. Writes take microseconds, so the
//! async methods run them in place.

use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;
use henk_domain::run::{EventId, RunId, RunKind};
use rusqlite::{Connection, OptionalExtension as _, params};
use rusqlite_migration::{M, Migrations};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::store::RunStore;
use crate::types::{
    DayCounts, DraftDecision, DraftRecord, EventFacets, EventFilter, EventRecord,
    EventWithOutcomes, FindingAction, FindingRecord, InboundEvent, MAX_PAYLOAD_BYTES,
    MOST_SESSION_REVIEWS, NewRun, OutcomeFilter, OutcomeRecord, OutcomeRow, Page, PruneCounts,
    RawRun, RoundRecord, RoundRow, RunFilter, RunRecord, RunStatus, SessionEnding, SessionRecord,
    SessionRole, SessionStatus, Stage, StageRecord, StageState, StageWrite, StoreError, TokenUsage,
    ToolCallFilter, ToolCallListing, ToolCallRecord, ToolUsage, TranscriptRecord,
    TranscriptSummary, attach_outcomes, draft_verdict, event_id, kind_str, merge_days, now,
    platform_str, round_record, run_kind, run_platform, session_dots, session_endings,
    session_role, session_status, stage_dots, stage_records, to_i64, to_u64,
};

/// The run store over SQLite.
#[derive(Debug)]
pub struct SqliteStore {
    connection: Mutex<Connection>,
}

/// A stored time column as an [`instant`], so it compares as a time.
macro_rules! instant_of {
    ($column:literal) => {
        concat!(
            "substr(",
            $column,
            ", 1, 19) || substr(CASE WHEN substr(",
            $column,
            ", 20, 1) = '.' THEN substr(",
            $column,
            ", 21, length(",
            $column,
            ") - 21) ELSE '' END || '000000000', 1, 9)"
        )
    };
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
    ])
}

impl SqliteStore {
    /// Opens or creates the database at `path` and migrates it.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the file cannot be opened or migrated.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        Self::from_connection(Connection::open(path)?)
    }

    /// An in-memory store, for tests and probes.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when SQLite cannot create the database.
    pub fn in_memory() -> Result<Self, StoreError> {
        Self::from_connection(Connection::open_in_memory()?)
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
        Self::migrated(Connection::open(path)?, Some(version))
    }

    fn from_connection(connection: Connection) -> Result<Self, StoreError> {
        Self::migrated(connection, None)
    }

    /// The store over `connection`, migrated up to `version`, or to the
    /// latest schema when `None`.
    fn migrated(mut connection: Connection, version: Option<usize>) -> Result<Self, StoreError> {
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        match version {
            Some(version) => migrations().to_version(&mut connection, version)?,
            None => migrations().to_latest(&mut connection)?,
        }
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

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
        let target = to_i64("runs.target", run.target)?;
        self.with(|c| {
            c.execute(
                "INSERT INTO runs (id, kind, platform, repo, target, commit_sha, requester, trigger, status, started_at, link, heartbeat_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?10)",
                params![
                    run.id.as_str(),
                    kind_str(run.kind),
                    platform_str(run.platform),
                    run.repo,
                    target,
                    run.commit,
                    run.requester,
                    run.trigger,
                    RunStatus::Running.as_str(),
                    now(),
                    run.link,
                ],
            )?;
            Ok(())
        })
    }

    async fn finish_run(
        &self,
        id: &RunId,
        status: RunStatus,
        summary: Option<&str>,
        error: Option<&str>,
    ) -> Result<(), StoreError> {
        self.with(|c| {
            c.execute(
                "UPDATE runs SET status = ?2, finished_at = ?3, summary = ?4, error = ?5 WHERE id = ?1",
                params![id.as_str(), status.as_str(), now(), summary, error],
            )?;
            Ok(())
        })
    }

    async fn supersede_run(&self, id: &RunId, by: &RunId, reason: &str) -> Result<(), StoreError> {
        self.with(|c| {
            c.execute(
                "UPDATE runs SET status = ?2, finished_at = ?3, error = ?4, superseded_by = ?5 WHERE id = ?1",
                params![
                    id.as_str(),
                    RunStatus::Superseded.as_str(),
                    now(),
                    reason,
                    by.as_str()
                ],
            )?;
            Ok(())
        })
    }

    async fn run(&self, id: &RunId) -> Result<Option<RunRecord>, StoreError> {
        self.with(|c| {
            c.query_row(
                &format!("SELECT {RUN_COLUMNS} FROM runs WHERE id = ?1"),
                params![id.as_str()],
                raw_run,
            )
            .optional()?
            .map(RawRun::into_record)
            .transpose()
        })
    }

    async fn heartbeat(&self, id: &RunId) -> Result<(), StoreError> {
        self.with(|c| {
            c.execute(
                "UPDATE runs SET heartbeat_at = ?2 WHERE id = ?1",
                params![id.as_str(), now()],
            )?;
            Ok(())
        })
    }

    async fn end_loop(&self, id: &RunId, stop: &str, rounds: u32) -> Result<(), StoreError> {
        self.with(|c| {
            c.execute(
                "UPDATE runs SET loop_stop = ?2, loop_rounds = ?3 WHERE id = ?1",
                params![id.as_str(), stop, rounds],
            )?;
            Ok(())
        })
    }

    async fn set_check(&self, id: &RunId, check_id: &str) -> Result<(), StoreError> {
        self.with(|c| {
            c.execute(
                "UPDATE runs SET check_id = ?2 WHERE id = ?1",
                params![id.as_str(), check_id],
            )?;
            Ok(())
        })
    }

    async fn orphaned_runs(
        &self,
        stale_before: OffsetDateTime,
    ) -> Result<Vec<RunRecord>, StoreError> {
        let cutoff = instant(
            "stale_before",
            &stale_before.format(&Rfc3339).unwrap_or_default(),
        )?;
        self.with(|c| {
            let mut statement = c.prepare(&format!(
                "SELECT {RUN_COLUMNS} FROM runs WHERE {ORPHANED}
                 ORDER BY {started}, id",
                started = instant_of!("started_at"),
            ))?;
            let raw = statement
                .query_map(params![RunStatus::Running.as_str(), cutoff], raw_run)?
                .collect::<Result<Vec<_>, _>>()?;
            raw.into_iter().map(RawRun::into_record).collect()
        })
    }

    async fn latest_reviews(&self, since: OffsetDateTime) -> Result<Vec<RunRecord>, StoreError> {
        let since = instant(
            "since",
            &since
                .format(&Rfc3339)
                .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned()),
        )?;
        self.with(|c| {
            let mut statement = c.prepare(&format!(
                "SELECT {RUN_COLUMNS} FROM ({LATEST_REVIEWS}) ORDER BY at, id"
            ))?;
            let raw = statement
                .query_map(params![kind_str(RunKind::Review), since], raw_run)?
                .collect::<Result<Vec<_>, _>>()?;
            raw.into_iter().map(RawRun::into_record).collect()
        })
    }

    async fn claim_resume(
        &self,
        run: &RunId,
        interrupted: &str,
        resumed: &str,
    ) -> Result<bool, StoreError> {
        self.with(|c| {
            let changed = c.execute(
                "UPDATE runs SET error = ?4 WHERE id = ?1 AND status = ?2 AND error = ?3",
                params![
                    run.as_str(),
                    RunStatus::Failed.as_str(),
                    interrupted,
                    resumed
                ],
            )?;
            Ok(changed == 1)
        })
    }

    async fn stage(&self, run: &RunId, write: &StageWrite) -> Result<(), StoreError> {
        let at = |time: Option<OffsetDateTime>| {
            time.map_or_else(now, |t| t.format(&Rfc3339).unwrap_or_else(|_| now()))
        };
        let started = at(write.started_at);
        let ended = write.state.ended().then(|| at(write.ended_at));
        self.with(|c| {
            c.execute(
                "INSERT INTO stages (run_id, stage, state, started_at, ended_at, detail)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (run_id, stage) DO UPDATE SET
                     state = excluded.state, ended_at = excluded.ended_at, detail = excluded.detail",
                params![
                    run.as_str(),
                    write.stage.as_str(),
                    write.state.as_str(),
                    started,
                    ended,
                    write.detail
                ],
            )?;
            Ok(())
        })
    }

    async fn fail_running_stages(&self, run: &RunId, reason: &str) -> Result<(), StoreError> {
        self.with(|c| {
            c.execute(
                "UPDATE stages SET state = ?2, ended_at = ?3, detail = ?4
                 WHERE run_id = ?1 AND state = ?5",
                params![
                    run.as_str(),
                    StageState::Failed.as_str(),
                    now(),
                    reason,
                    StageState::Running.as_str()
                ],
            )?;
            Ok(())
        })
    }

    async fn stages(&self, run: &RunId) -> Result<Vec<StageRecord>, StoreError> {
        self.with(|c| {
            let mut statement = c.prepare(
                "SELECT stage, state, started_at, ended_at, detail FROM stages WHERE run_id = ?1",
            )?;
            let rows = statement
                .query_map(params![run.as_str()], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            stage_records(rows)
        })
    }

    async fn drop_running_sessions(&self, run: &RunId, reason: &str) -> Result<(), StoreError> {
        self.with(|c| {
            c.execute(
                "UPDATE sessions SET status = ?2, finished_at = ?3, error = ?4
                 WHERE run_id = ?1 AND status = ?5",
                params![
                    run.as_str(),
                    SessionStatus::DidNotFinish.as_str(),
                    now(),
                    reason,
                    SessionStatus::Running.as_str()
                ],
            )?;
            Ok(())
        })
    }

    async fn start_session(
        &self,
        run: &RunId,
        name: &str,
        role: SessionRole,
        model: &str,
    ) -> Result<(), StoreError> {
        self.with(|c| {
            c.execute(
                "INSERT INTO sessions (run_id, name, role, model, status, started_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    run.as_str(),
                    name,
                    role.as_str(),
                    model,
                    SessionStatus::Running.as_str(),
                    now()
                ],
            )?;
            Ok(())
        })
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
        let count = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
        self.with(|c| {
            c.execute(
                "UPDATE sessions SET status = ?3, turns = ?4, input_tokens = ?5, output_tokens = ?6, finished_at = ?7, error = ?8,
                 cache_read_tokens = ?9, cache_write_tokens = ?10, reasoning_tokens = ?11
                 WHERE run_id = ?1 AND name = ?2",
                params![
                    run.as_str(),
                    name,
                    status.as_str(),
                    count(turns),
                    count(tokens.input),
                    count(tokens.output),
                    now(),
                    error,
                    count(tokens.cache_read),
                    count(tokens.cache_write),
                    count(tokens.reasoning),
                ],
            )?;
            Ok(())
        })
    }

    async fn sessions(&self, run: &RunId) -> Result<Vec<SessionRecord>, StoreError> {
        self.with(|c| {
            let mut statement = c.prepare(
                "SELECT name, model, status, turns, input_tokens, output_tokens, error, started_at, finished_at, cache_read_tokens, cache_write_tokens, role, reasoning_tokens FROM sessions WHERE run_id = ?1 ORDER BY name",
            )?;
            let rows = statement.query_map(params![run.as_str()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, i64>(9)?,
                    row.get::<_, i64>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, i64>(12)?,
                ))
            })?;
            let mut sessions = Vec::new();
            for row in rows {
                let (
                    name,
                    model,
                    status,
                    turns,
                    input_tokens,
                    output_tokens,
                    error,
                    started_at,
                    finished_at,
                    cache_read,
                    cache_write,
                    role,
                    reasoning,
                ) = row?;
                sessions.push(SessionRecord {
                    name,
                    role: session_role(role)?,
                    model,
                    status: session_status(status)?,
                    turns: to_u64("sessions.turns", turns)?,
                    input_tokens: to_u64("sessions.input_tokens", input_tokens)?,
                    output_tokens: to_u64("sessions.output_tokens", output_tokens)?,
                    cache_read_tokens: to_u64("sessions.cache_read_tokens", cache_read)?,
                    cache_write_tokens: to_u64("sessions.cache_write_tokens", cache_write)?,
                    reasoning_tokens: to_u64("sessions.reasoning_tokens", reasoning)?,
                    error,
                    started_at,
                    finished_at,
                });
            }
            Ok(sessions)
        })
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
        self.with(|c| {
            c.execute(
                "INSERT INTO findings (run_id, session, path, line, comment_id, action, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![run.as_str(), session, path, line_number, comment_id, action.as_str(), now()],
            )?;
            Ok(())
        })
    }

    async fn record_draft(&self, run: &RunId, draft: &DraftRecord) -> Result<(), StoreError> {
        let at = at_or_now(&draft.at);
        self.with(|c| {
            c.execute(
                "INSERT INTO drafts (run_id, draft, session, model, kind, path, line, target, body, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) ON CONFLICT (run_id, draft) DO UPDATE SET kind = excluded.kind, target = excluded.target, body = excluded.body",
                params![
                    run.as_str(),
                    draft.draft,
                    draft.session,
                    draft.model,
                    draft.kind,
                    draft.path,
                    draft.line,
                    draft.target,
                    draft.body,
                    at
                ],
            )?;
            Ok(())
        })
    }

    async fn decide_draft(
        &self,
        run: &RunId,
        draft: &str,
        decision: &DraftDecision,
    ) -> Result<(), StoreError> {
        let at = at_or_now(&decision.at);
        self.with(|c| {
            c.execute(
                "UPDATE drafts SET verdict = ?3, checker = ?4, reason = ?5, same_as = ?6, comment_id = ?7, decided_at = ?8 WHERE run_id = ?1 AND draft = ?2",
                params![
                    run.as_str(),
                    draft,
                    decision.verdict.as_str(),
                    decision.checker,
                    decision.reason,
                    decision.same_as,
                    decision.comment_id,
                    at
                ],
            )?;
            Ok(())
        })
    }

    async fn drafts(&self, run: &RunId) -> Result<Vec<DraftRecord>, StoreError> {
        type Row = (
            DraftRecord,
            Option<String>,
            String,
            String,
            String,
            String,
            Option<String>,
        );
        let rows: Vec<Row> = self.with(|c| {
            let mut statement = c.prepare(
                "SELECT created_at, draft, session, model, kind, path, line, target, body, verdict, checker, reason, same_as, comment_id, decided_at FROM drafts WHERE run_id = ?1 ORDER BY id",
            )?;
            let rows = statement.query_map(params![run.as_str()], |row| {
                Ok((
                    DraftRecord {
                        at: row.get(0)?,
                        draft: row.get(1)?,
                        session: row.get(2)?,
                        model: row.get(3)?,
                        kind: row.get(4)?,
                        path: row.get(5)?,
                        line: row.get(6)?,
                        target: row.get(7)?,
                        body: row.get(8)?,
                        decision: None,
                    },
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                    row.get(13)?,
                    row.get(14)?,
                ))
            })?;
            rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::from)
        })?;
        rows.into_iter()
            .map(
                |(mut draft, verdict, checker, reason, same_as, comment_id, decided)| {
                    if let Some(verdict) = verdict {
                        draft.decision = Some(DraftDecision {
                            at: decided.unwrap_or_default(),
                            verdict: draft_verdict(&verdict)?,
                            checker,
                            reason,
                            same_as,
                            comment_id,
                        });
                    }
                    Ok(draft)
                },
            )
            .collect()
    }

    async fn record_transcript(
        &self,
        run: &RunId,
        transcript: &TranscriptRecord,
    ) -> Result<(), StoreError> {
        let at = at_or_now(&transcript.at);
        self.with(|c| {
            c.execute(
                "INSERT INTO transcripts (run_id, session, model, stop, turns, bytes, body, recorded_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    run.as_str(),
                    transcript.session,
                    transcript.model,
                    transcript.stop,
                    transcript.turns,
                    i64::try_from(transcript.bytes).unwrap_or(i64::MAX),
                    transcript.body,
                    at
                ],
            )?;
            Ok(())
        })
    }

    async fn start_round(&self, run: &RunId, round: &RoundRecord) -> Result<(), StoreError> {
        let at = at_or_now(&round.started_at);
        let handover_len = to_i64("session_rounds.handover_len", round.handover_len)?;
        self.with(|c| {
            c.execute(
                "INSERT INTO session_rounds (run_id, session, role, round, started_at, first_turn, handover, handover_len)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    run.as_str(),
                    round.session,
                    round.role.as_str(),
                    round.round,
                    at,
                    round.first_turn,
                    round.handover,
                    handover_len,
                ],
            )?;
            Ok(())
        })
    }

    async fn end_round(
        &self,
        run: &RunId,
        session: &str,
        round: u32,
        turns: u32,
    ) -> Result<(), StoreError> {
        self.with(|c| {
            c.execute(
                "UPDATE session_rounds SET ended_at = ?4, turns = ?5
                 WHERE run_id = ?1 AND session = ?2 AND round = ?3",
                params![run.as_str(), session, round, now(), turns],
            )?;
            Ok(())
        })
    }

    async fn rounds(&self, run: &RunId) -> Result<Vec<RoundRecord>, StoreError> {
        self.with(|c| {
            let rows = c
                .prepare(ROUNDS)?
                .query_map(params![run.as_str()], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                    ))
                })?
                .collect::<Result<Vec<RoundRow>, _>>()?;
            rows.into_iter().map(round_record).collect()
        })
    }

    async fn transcripts(&self, run: &RunId) -> Result<Vec<TranscriptSummary>, StoreError> {
        self.with(|c| {
            let mut statement = c.prepare(
                "SELECT recorded_at, session, model, stop, turns, bytes FROM transcripts WHERE run_id = ?1 ORDER BY id",
            )?;
            let rows = statement.query_map(params![run.as_str()], |row| {
                Ok(TranscriptSummary {
                    at: row.get(0)?,
                    session: row.get(1)?,
                    model: row.get(2)?,
                    stop: row.get(3)?,
                    turns: row.get(4)?,
                    bytes: u64::try_from(row.get::<_, i64>(5)?).unwrap_or_default(),
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::from)
        })
    }

    async fn transcript(
        &self,
        run: &RunId,
        session: &str,
    ) -> Result<Option<TranscriptRecord>, StoreError> {
        self.with(|c| {
            let mut statement = c.prepare(
                "SELECT recorded_at, session, model, stop, turns, bytes, body FROM transcripts WHERE run_id = ?1 AND session = ?2 ORDER BY id DESC LIMIT 1",
            )?;
            let mut rows = statement.query_map(params![run.as_str(), session], |row| {
                Ok(TranscriptRecord {
                    at: row.get(0)?,
                    session: row.get(1)?,
                    model: row.get(2)?,
                    stop: row.get(3)?,
                    turns: row.get(4)?,
                    bytes: u64::try_from(row.get::<_, i64>(5)?).unwrap_or_default(),
                    body: row.get(6)?,
                })
            })?;
            rows.next().transpose().map_err(StoreError::from)
        })
    }

    async fn record_tool_call(&self, run: &RunId, call: &ToolCallRecord) -> Result<(), StoreError> {
        let at = at_or_now(&call.at);
        let number = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
        self.with(|c| {
            c.execute(
                "INSERT INTO tool_calls (run_id, session, model, turn, tool, origin, outcome, arguments, arguments_len, result_chars, elapsed_ms, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    run.as_str(),
                    call.session,
                    call.model,
                    call.turn,
                    call.tool,
                    call.origin,
                    call.outcome,
                    call.arguments,
                    number(call.arguments_len),
                    number(call.result_chars),
                    number(call.elapsed_ms),
                    at
                ],
            )?;
            Ok(())
        })
    }

    async fn tool_calls(&self, run: &RunId) -> Result<Vec<ToolCallRecord>, StoreError> {
        self.with(|c| {
            let mut statement = c.prepare(
                "SELECT at, session, model, turn, tool, origin, outcome, arguments, arguments_len, result_chars, elapsed_ms FROM tool_calls WHERE run_id = ?1 ORDER BY id",
            )?;
            let number = |n: i64| u64::try_from(n).unwrap_or_default();
            let rows = statement.query_map(params![run.as_str()], |row| {
                Ok(ToolCallRecord {
                    at: row.get(0)?,
                    session: row.get(1)?,
                    model: row.get(2)?,
                    turn: row.get(3)?,
                    tool: row.get(4)?,
                    origin: row.get(5)?,
                    outcome: row.get(6)?,
                    arguments: row.get(7)?,
                    arguments_len: number(row.get(8)?),
                    result_chars: number(row.get(9)?),
                    elapsed_ms: number(row.get(10)?),
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::from)
        })
    }

    async fn tool_usage(&self, filter: &ToolCallFilter) -> Result<Vec<ToolUsage>, StoreError> {
        let (since, until) = window(filter.since.as_deref(), filter.until.as_deref())?;
        let rows = self.with(|c| {
            let mut statement = c.prepare(&format!(
                "SELECT c.model, {CALL_ROLE}, {CALL_TOOL}, c.outcome, COUNT(*), COALESCE(SUM(c.elapsed_ms), 0)
                 FROM tool_calls c {CALL_SESSION} WHERE {CALL_FILTER}
                 GROUP BY c.model, 2, 3, c.outcome"
            ))?;
            let number = |n: i64| u64::try_from(n).unwrap_or_default();
            let rows = statement.query_map(
                params![filter.tool, filter.model, filter.role.map(SessionRole::as_str), since, until],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        number(row.get(4)?),
                        number(row.get(5)?),
                    ))
                },
            )?;
            rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::from)
        })?;
        Ok(ToolUsage::across_runs(rows))
    }

    async fn list_tool_calls(
        &self,
        filter: &ToolCallFilter,
        page: Page,
    ) -> Result<Vec<ToolCallListing>, StoreError> {
        let (since, until) = window(filter.since.as_deref(), filter.until.as_deref())?;
        let outcome = OutcomeFilter::param(filter.outcome.as_ref());
        let (before_at, before_id) = filter
            .before
            .as_ref()
            .map(|k| (k.at.as_str(), k.id))
            .unzip();
        let rows: Vec<CallRow> = self.with(|c| {
            let mut statement = c.prepare(&format!(
                "SELECT c.id, c.run_id, r.repo, r.target, r.platform, c.at, c.session, c.model,
                        c.turn, c.tool, c.origin, c.outcome, c.arguments, c.arguments_len,
                        c.result_chars, c.elapsed_ms,
                        EXISTS (SELECT 1 FROM transcripts t
                                WHERE t.run_id = c.run_id AND t.session = c.session),
                        r.kind
                 FROM tool_calls c JOIN runs r ON r.id = c.run_id {CALL_SESSION}
                 WHERE {CALL_FILTER}
                   AND (?6 IS NULL OR (?6 = 'problems' AND c.outcome <> 'ok') OR c.outcome = ?6)
                   AND (?7 IS NULL OR c.at < ?7 OR (c.at = ?7 AND c.id < ?8))
                 ORDER BY c.at DESC, c.id DESC LIMIT ?9 OFFSET ?10"
            ))?;
            let rows = statement.query_map(
                params![
                    filter.tool,
                    filter.model,
                    filter.role.map(SessionRole::as_str),
                    since,
                    until,
                    outcome,
                    before_at,
                    before_id,
                    page.limit(),
                    page.offset()
                ],
                call_row,
            )?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        })?;
        rows.into_iter().map(CallRow::into_listing).collect()
    }

    async fn findings(&self, run: &RunId) -> Result<Vec<FindingRecord>, StoreError> {
        self.with(|c| {
            let mut statement = c.prepare(
                "SELECT created_at, session, path, line, comment_id, action FROM findings WHERE run_id = ?1 ORDER BY id",
            )?;
            let rows = statement.query_map(params![run.as_str()], |row| {
                Ok(FindingRecord {
                    at: row.get(0)?,
                    session: row.get(1)?,
                    path: row.get(2)?,
                    line: row.get(3)?,
                    comment_id: row.get(4)?,
                    action: row.get(5)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        })
    }

    async fn event(&self, run: &RunId, level: &str, message: &str) -> Result<(), StoreError> {
        self.with(|c| {
            c.execute(
                "INSERT INTO events (run_id, at, level, message) VALUES (?1, ?2, ?3, ?4)",
                params![run.as_str(), now(), level, message],
            )?;
            Ok(())
        })
    }

    async fn events(&self, run: &RunId) -> Result<Vec<EventRecord>, StoreError> {
        self.with(|c| {
            let mut statement =
                c.prepare("SELECT at, level, message FROM events WHERE run_id = ?1 ORDER BY id")?;
            let rows = statement.query_map(params![run.as_str()], |row| {
                Ok(EventRecord {
                    at: row.get(0)?,
                    level: row.get(1)?,
                    message: row.get(2)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        })
    }

    async fn joined(&self, run: &RunId, source: &str) -> Result<(), StoreError> {
        self.with(|c| {
            c.execute(
                "INSERT INTO requests (run_id, at, source) VALUES (?1, ?2, ?3)",
                params![run.as_str(), now(), source],
            )?;
            Ok(())
        })
    }

    async fn record_event(&self, event: &InboundEvent) -> Result<(), StoreError> {
        let payload = event
            .payload
            .as_deref()
            .filter(|p| p.len() <= MAX_PAYLOAD_BYTES);
        let target = event
            .target
            .map(|t| to_i64("inbound_events.target", t))
            .transpose()?;
        self.with(|c| {
            c.execute(
                "INSERT INTO inbound_events (id, received_at, source, kind, repo, target, payload, requester) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    event.id.as_str(),
                    event.received_at,
                    event.source,
                    event.kind,
                    event.repo,
                    target,
                    payload,
                    event.requester,
                ],
            )?;
            Ok(())
        })
    }

    async fn record_outcome(&self, outcome: &OutcomeRecord) -> Result<(), StoreError> {
        self.with(|c| {
            c.execute(
                "INSERT INTO event_outcomes (event_id, listener, outcome, detail, run_id, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    outcome.event_id.as_str(),
                    outcome.listener,
                    outcome.outcome,
                    outcome.detail,
                    outcome.run_id,
                    at_or_now(&outcome.at),
                ],
            )?;
            Ok(())
        })
    }

    async fn inbound_event(&self, id: &EventId) -> Result<Option<InboundEvent>, StoreError> {
        self.with(|c| {
            c.query_row(
                "SELECT id, received_at, source, kind, repo, target, payload, requester FROM inbound_events WHERE id = ?1",
                params![id.as_str()],
                raw_inbound,
            )
            .optional()?
            .map(RawInbound::into_event)
            .transpose()
        })
    }

    async fn event_facets(&self) -> Result<EventFacets, StoreError> {
        self.with(|c| {
            let distinct = |column: &str| -> Result<Vec<String>, StoreError> {
                let mut statement = c.prepare(&format!(
                    "SELECT DISTINCT {column} FROM inbound_events ORDER BY 1"
                ))?;
                let values = statement
                    .query_map([], |row| row.get(0))?
                    .collect::<Result<Vec<String>, _>>()?;
                Ok(values)
            };
            Ok(EventFacets {
                sources: distinct("source")?,
                kinds: distinct("kind")?,
            })
        })
    }

    async fn outcomes(&self, id: &EventId) -> Result<Vec<OutcomeRecord>, StoreError> {
        self.with(|c| {
            let mut statement = c.prepare(
                "SELECT listener, outcome, detail, run_id, at FROM event_outcomes WHERE event_id = ?1 ORDER BY rowid",
            )?;
            let rows = statement.query_map(params![id.as_str()], |row| {
                Ok(OutcomeRecord {
                    event_id: id.clone(),
                    listener: row.get(0)?,
                    outcome: row.get(1)?,
                    detail: row.get(2)?,
                    run_id: row.get(3)?,
                    at: row.get(4)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::from)
        })
    }

    async fn prune_events(&self, older_than: OffsetDateTime) -> Result<PruneCounts, StoreError> {
        let cutoff = older_than
            .format(&Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned());
        self.with(|c| {
            // The store's mutex is held, so nothing else can interleave.
            let transaction = c.unchecked_transaction()?;
            let outcomes = transaction.execute(
                "DELETE FROM event_outcomes WHERE event_id IN
                 (SELECT id FROM inbound_events WHERE received_at < ?1)",
                params![cutoff],
            )?;
            let events = transaction.execute(
                "DELETE FROM inbound_events WHERE received_at < ?1",
                params![cutoff],
            )?;
            let transcripts = transaction.execute(
                "DELETE FROM transcripts WHERE recorded_at < ?1",
                params![cutoff],
            )?;
            transaction.commit()?;
            Ok(PruneCounts {
                events: u64::try_from(events).unwrap_or(u64::MAX),
                outcomes: u64::try_from(outcomes).unwrap_or(u64::MAX),
                transcripts: u64::try_from(transcripts).unwrap_or(u64::MAX),
            })
        })
    }

    async fn inbound_events_for_run(&self, run: &RunId) -> Result<Vec<InboundEvent>, StoreError> {
        let ids: Vec<String> = self.with(|c| {
            let mut statement = c.prepare(
                "SELECT DISTINCT e.id FROM inbound_events e JOIN event_outcomes o ON o.event_id = e.id WHERE o.run_id = ?1 ORDER BY e.received_at",
            )?;
            let rows = statement.query_map(params![run.as_str()], |row| row.get(0))?;
            rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::from)
        })?;
        let mut events = Vec::new();
        for id in ids {
            let id = event_id(id)?;
            if let Some(event) = self.inbound_event(&id).await? {
                events.push(event);
            }
        }
        Ok(events)
    }

    async fn list_runs(
        &self,
        filter: &RunFilter,
        page: Page,
    ) -> Result<Vec<RunRecord>, StoreError> {
        let kind = filter.kind.map(kind_str);
        let status = filter.status.map(RunStatus::as_str);
        let platform = filter.platform.map(platform_str);
        let target = filter.target.map(|t| i64::try_from(t).unwrap_or(i64::MAX));
        let (before_at, before_id) = run_key(filter)?;
        let (since, until) = run_window(filter)?;
        self.with(|c| {
            let mut statement = c.prepare(&format!(
                "SELECT {RUN_COLUMNS} FROM runs WHERE {RUN_FILTER}
                 ORDER BY {RUN_ORDER} LIMIT ?10 OFFSET ?11"
            ))?;
            let raw = statement
                .query_map(
                    params![
                        kind,
                        status,
                        platform,
                        filter.repo,
                        target,
                        since,
                        until,
                        before_at,
                        before_id,
                        page.limit(),
                        page.offset()
                    ],
                    raw_run,
                )?
                .collect::<Result<Vec<_>, _>>()?;
            raw.into_iter().map(RawRun::into_record).collect()
        })
    }

    async fn daily_stats(&self, since: OffsetDateTime) -> Result<Vec<DayCounts>, StoreError> {
        let since = instant("since", &since.format(&Rfc3339).unwrap_or_default())?;
        self.with(|c| {
            let runs = c
                .prepare(DAY_RUNS)?
                .query_map(params![since], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let findings = c
                .prepare(DAY_FINDINGS)?
                .query_map(params![since], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            let drafts = c
                .prepare(DAY_DRAFTS)?
                .query_map(params![since], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(merge_days(runs, findings, drafts))
        })
    }

    async fn sessions_of(
        &self,
        runs: &[RunId],
    ) -> Result<Vec<(RunId, String, SessionRole, SessionStatus)>, StoreError> {
        if runs.is_empty() {
            return Ok(Vec::new());
        }
        let marks = vec!["?"; runs.len()].join(", ");
        self.with(|c| {
            let rows = c
                .prepare(&format!(
                    "SELECT run_id, name, role, status FROM sessions WHERE run_id IN ({marks}) ORDER BY run_id, name"
                ))?
                .query_map(
                    rusqlite::params_from_iter(runs.iter().map(RunId::as_str)),
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )?
                .collect::<Result<Vec<_>, _>>()?;
            session_dots(rows)
        })
    }

    async fn session_endings(
        &self,
        since: Option<OffsetDateTime>,
        reviews: u32,
    ) -> Result<Vec<SessionEnding>, StoreError> {
        let since = since
            .map(|at| instant("since", &at.format(&Rfc3339).unwrap_or_default()))
            .transpose()?;
        let reviews = i64::from(reviews.min(MOST_SESSION_REVIEWS));
        self.with(|c| {
            let rows = c
                .prepare(SESSION_ENDINGS)?
                .query_map(params![since, reviews], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            session_endings(rows)
        })
    }

    async fn stages_of(
        &self,
        runs: &[RunId],
    ) -> Result<Vec<(RunId, Stage, StageState)>, StoreError> {
        if runs.is_empty() {
            return Ok(Vec::new());
        }
        let marks = vec!["?"; runs.len()].join(", ");
        self.with(|c| {
            let rows = c
                .prepare(&format!(
                    "SELECT run_id, stage, state FROM stages WHERE run_id IN ({marks})"
                ))?
                .query_map(
                    rusqlite::params_from_iter(runs.iter().map(RunId::as_str)),
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?
                .collect::<Result<Vec<_>, _>>()?;
            stage_dots(rows)
        })
    }

    async fn count_runs(&self, filter: &RunFilter) -> Result<u64, StoreError> {
        let kind = filter.kind.map(kind_str);
        let status = filter.status.map(RunStatus::as_str);
        let platform = filter.platform.map(platform_str);
        let target = filter.target.map(|t| i64::try_from(t).unwrap_or(i64::MAX));
        let (before_at, before_id) = run_key(filter)?;
        let (since, until) = run_window(filter)?;
        self.with(|c| {
            let count: i64 = c.query_row(
                &format!("SELECT COUNT(*) FROM runs WHERE {RUN_FILTER}"),
                params![
                    kind,
                    status,
                    platform,
                    filter.repo,
                    target,
                    since,
                    until,
                    before_at,
                    before_id
                ],
                |row| row.get(0),
            )?;
            Ok(u64::try_from(count).unwrap_or_default())
        })
    }

    async fn list_inbound_events(
        &self,
        filter: &EventFilter,
        page: Page,
    ) -> Result<Vec<EventWithOutcomes>, StoreError> {
        self.with(|c| {
            let mut statement = c.prepare(
                "SELECT id, received_at, source, kind, repo, target, payload, requester FROM inbound_events
                 WHERE (?1 IS NULL OR source = ?1) AND (?2 IS NULL OR kind = ?2) AND (?3 IS NULL OR repo = ?3)
                   AND (?4 IS NULL OR received_at < ?4 OR (received_at = ?4 AND id < ?5))
                 ORDER BY received_at DESC, id DESC LIMIT ?6 OFFSET ?7",
            )?;
            let (before_at, before_id) = filter
                .before
                .as_ref()
                .map(|k| (k.received_at.as_str(), k.id.as_str()))
                .unzip();
            let rows = statement
                .query_map(
                    params![
                        filter.source,
                        filter.kind,
                        filter.repo,
                        before_at,
                        before_id,
                        page.limit(),
                        page.offset()
                    ],
                    raw_inbound,
                )?
                .collect::<Result<Vec<_>, _>>()?;
            let events = rows
                .into_iter()
                .map(RawInbound::into_event)
                .collect::<Result<Vec<_>, _>>()?;
            if events.is_empty() {
                return Ok(Vec::new());
            }
            // One query for every outcome of the page, not one per event.
            let marks = vec!["?"; events.len()].join(", ");
            let mut statement = c.prepare(&format!(
                "SELECT event_id, listener, outcome, detail, run_id, at FROM event_outcomes
                 WHERE event_id IN ({marks}) ORDER BY rowid"
            ))?;
            let ids: Vec<&str> = events.iter().map(|e| e.id.as_str()).collect();
            let outcomes = statement
                .query_map(rusqlite::params_from_iter(ids), |row| {
                    Ok(OutcomeRow {
                        event_id: row.get(0)?,
                        listener: row.get(1)?,
                        outcome: row.get(2)?,
                        detail: row.get(3)?,
                        run_id: row.get(4)?,
                        at: row.get(5)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(attach_outcomes(events, &outcomes))
        })
    }
}

/// An inbound event row before its values are checked.
struct RawInbound {
    id: String,
    received_at: String,
    source: String,
    kind: String,
    repo: Option<String>,
    target: Option<i64>,
    payload: Option<String>,
    requester: Option<String>,
}

fn raw_inbound(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawInbound> {
    Ok(RawInbound {
        id: row.get(0)?,
        received_at: row.get(1)?,
        source: row.get(2)?,
        kind: row.get(3)?,
        repo: row.get(4)?,
        target: row.get(5)?,
        payload: row.get(6)?,
        requester: row.get(7)?,
    })
}

impl RawInbound {
    fn into_event(self) -> Result<InboundEvent, StoreError> {
        let id = event_id(self.id)?;
        Ok(InboundEvent {
            id,
            received_at: self.received_at,
            source: self.source,
            kind: self.kind,
            repo: self.repo,
            target: self
                .target
                .map(|t| to_u64("inbound_events.target", t))
                .transpose()?,
            payload: self.payload,
            requester: self.requester,
        })
    }
}

/// The `WHERE` of a run listing, over parameters 1 to 9: kind, status,
/// platform, repo, target, since, until, and the keyset (time, id).
/// `since`, `until` and the keyset's time are [`instant`]s, compared to
/// `started_at` as one too: as text, "12:00:00.4Z" sorts before
/// "12:00:00Z" (#216).
const RUN_FILTER: &str = concat!(
    "(?1 IS NULL OR kind = ?1) AND (?2 IS NULL OR status = ?2)
     AND (?3 IS NULL OR platform = ?3) AND (?4 IS NULL OR repo = ?4)
     AND (?5 IS NULL OR target = ?5)
     AND (?6 IS NULL OR ",
    instant_of!("started_at"),
    " >= ?6) AND (?7 IS NULL OR ",
    instant_of!("started_at"),
    " < ?7) AND (?8 IS NULL OR ",
    instant_of!("started_at"),
    " < ?8 OR (",
    instant_of!("started_at"),
    " = ?8 AND id < ?9))"
);

/// The `WHERE` of the orphaned runs, over parameters 1 (the running
/// status) and 2 (the cutoff, an [`instant`]): no heartbeat, or one before
/// the cutoff as a time.
const ORPHANED: &str = concat!(
    "status = ?1 AND (heartbeat_at IS NULL OR ",
    instant_of!("heartbeat_at"),
    " < ?2)"
);

/// The order of a run listing: newest first as a time, then by id.
const RUN_ORDER: &str = concat!(instant_of!("started_at"), " DESC, id DESC");

/// A caller-supplied time, as given; empty means now.
fn at_or_now(at: &str) -> String {
    if at.is_empty() { now() } else { at.to_owned() }
}

/// The time bounds of a tool call count or listing, as [`instant`]s.
fn window(
    since: Option<&str>,
    until: Option<&str>,
) -> Result<(Option<String>, Option<String>), StoreError> {
    let since = since.map(|v| instant("since", v)).transpose()?;
    let until = until.map(|v| instant("until", v)).transpose()?;
    Ok((since, until))
}

/// The sessions of the newest reviews that ended (#229), over parameters 1
/// (since, an [`instant`], or null) and 2 (how many reviews). Ordered as
/// instants, not as text, so runs started in the same second keep their
/// order (#216).
const SESSION_ENDINGS: &str = concat!(
    "SELECT r.id, r.started_at, s.name, s.role, s.model, s.status, s.error
     FROM (SELECT id, started_at, ",
    instant_of!("started_at"),
    " AS at FROM runs
           WHERE kind = 'review' AND status NOT IN ('running', 'superseded', 'cancelled')
             AND (?1 IS NULL OR ",
    instant_of!("started_at"),
    " >= ?1)
           ORDER BY at DESC, id DESC LIMIT ?2) r
     JOIN sessions s ON s.run_id = r.id
     ORDER BY r.at DESC, r.id DESC, s.name"
);

/// The newest review of each pull request that started at or after ?2 (an
/// [`instant`]), over parameter 1 (the review kind), with its start as the
/// instant `at` (#160). Compared as instants, not as text, so runs started
/// in the same second keep their order: as text, "12:00:00Z" sorts after
/// "12:00:00.4Z".
const LATEST_REVIEWS: &str = concat!(
    "SELECT r.*, ",
    instant_of!("r.started_at"),
    " AS at FROM runs r WHERE r.kind = ?1 AND ",
    instant_of!("r.started_at"),
    " >= ?2 AND NOT EXISTS (
         SELECT 1 FROM runs n
         WHERE n.kind = r.kind AND n.platform = r.platform
           AND n.repo = r.repo AND n.target = r.target
           AND (",
    instant_of!("n.started_at"),
    " > ",
    instant_of!("r.started_at"),
    " OR (",
    instant_of!("n.started_at"),
    " = ",
    instant_of!("r.started_at"),
    " AND n.id > r.id)))"
);

/// Per-UTC-day counts for the overview (#225); stored times are UTC
/// RFC 3339, so the first ten characters are the day.
const DAY_RUNS: &str = concat!(
    "SELECT substr(started_at, 1, 10), COUNT(*),
     SUM(CASE WHEN status = 'finished' THEN 1 ELSE 0 END),
     SUM(CASE WHEN status = 'failed' THEN 1 ELSE 0 END)
     FROM runs WHERE ",
    instant_of!("started_at"),
    " >= ?1 GROUP BY 1"
);

/// Per-UTC-day counts for the overview (#225); stored times are UTC
/// RFC 3339, so the first ten characters are the day.
const DAY_FINDINGS: &str = concat!(
    "SELECT substr(created_at, 1, 10), COUNT(*) FROM findings
     WHERE action IN ('posted', 'unverified') AND ",
    instant_of!("created_at"),
    " >= ?1 GROUP BY 1"
);

/// Per-UTC-day counts for the overview (#225); stored times are UTC
/// RFC 3339, so the first ten characters are the day. A review loop's
/// findings are left out, as in [`DRAFT_FILTER`] (#285).
const DAY_DRAFTS: &str = concat!(
    "SELECT substr(created_at, 1, 10), COUNT(*) FROM drafts WHERE ",
    instant_of!("created_at"),
    " >= ?1 AND kind NOT IN ('loop_finding', 'plan_point') GROUP BY 1"
);

/// The rounds of a run (#374), over parameter 1, in the order they
/// started: as instants, not as text (#216).
const ROUNDS: &str = concat!(
    "SELECT session, role, round, started_at, ended_at, first_turn, turns, handover, handover_len
     FROM session_rounds WHERE run_id = ?1 ORDER BY ",
    instant_of!("started_at"),
    ", session, round"
);

/// The session row of a tool call `c`, as `s`, for its role.
const CALL_SESSION: &str = "LEFT JOIN sessions s ON s.run_id = c.run_id AND s.name = c.session";

/// The role of the session that made a tool call `c`, over
/// [`CALL_SESSION`]. A call whose session has no row counts as a lane's.
const CALL_ROLE: &str = "COALESCE(s.role, 'lane')";

/// The tool a call `c` counts under, as [`crate::types::counted_as`] says:
/// every tool that does not exist, which is every call with no origin, as
/// one (#341). [`CALL_FILTER`] matches a tool the same way.
const CALL_TOOL: &str = "CASE WHEN c.origin = '' THEN '(unknown)' ELSE c.tool END";

/// The `WHERE` of a tool call tally or listing over `tool_calls c` and
/// [`CALL_SESSION`], over parameters 1 to 5: tool, model, session role,
/// since and until, the last two as [`instant`]s. A tool is matched as
/// [`CALL_TOOL`] counts it.
const CALL_FILTER: &str = concat!(
    "(?1 IS NULL OR CASE WHEN c.origin = '' THEN '(unknown)' ELSE c.tool END = ?1) AND (?2 IS NULL OR c.model = ?2)",
    " AND (?3 IS NULL OR COALESCE(s.role, 'lane') = ?3)",
    " AND (?4 IS NULL OR ",
    instant_of!("c.at"),
    " >= ?4) AND (?5 IS NULL OR ",
    instant_of!("c.at"),
    " < ?5)"
);

/// A row of a tool call listing as read.
struct CallRow {
    id: i64,
    run_id: String,
    repo: String,
    target: i64,
    platform: String,
    call: ToolCallRecord,
    transcript_kept: bool,
    kind: String,
}

/// Reads the columns `list_tool_calls` selects, in order.
fn call_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CallRow> {
    let number = |n: i64| u64::try_from(n).unwrap_or_default();
    Ok(CallRow {
        id: row.get(0)?,
        run_id: row.get(1)?,
        repo: row.get(2)?,
        target: row.get(3)?,
        platform: row.get(4)?,
        call: ToolCallRecord {
            at: row.get(5)?,
            session: row.get(6)?,
            model: row.get(7)?,
            turn: row.get(8)?,
            tool: row.get(9)?,
            origin: row.get(10)?,
            outcome: row.get(11)?,
            arguments: row.get(12)?,
            arguments_len: number(row.get(13)?),
            result_chars: number(row.get(14)?),
            elapsed_ms: number(row.get(15)?),
        },
        transcript_kept: row.get(16)?,
        kind: row.get(17)?,
    })
}

impl CallRow {
    fn into_listing(self) -> Result<ToolCallListing, StoreError> {
        Ok(ToolCallListing {
            id: self.id,
            run_id: self.run_id,
            repo: self.repo,
            target: to_u64("runs.target", self.target)?,
            platform: run_platform(self.platform)?,
            kind: run_kind(self.kind)?,
            call: self.call,
            transcript_kept: self.transcript_kept,
        })
    }
}

/// An RFC 3339 time as a fixed-width UTC text that sorts as the time does:
/// `2026-10-07T12:00:00` and nine digits of nanoseconds. `RUN_FILTER` turns
/// a stored `started_at` (UTC, from [`now`]) into the same shape.
fn instant(column: &'static str, value: &str) -> Result<String, StoreError> {
    let at = OffsetDateTime::parse(value, &Rfc3339)
        .map_err(|_| StoreError::Corrupt {
            column,
            value: value.to_owned(),
        })?
        .to_offset(time::UtcOffset::UTC);
    Ok(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{:09}",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute(),
        at.second(),
        at.nanosecond()
    ))
}

/// The time bounds of a run listing, as [`instant`]s.
fn run_window(filter: &RunFilter) -> Result<(Option<String>, Option<String>), StoreError> {
    window(filter.since.as_deref(), filter.until.as_deref())
}

/// The keyset of a run listing, as two parameters: the time as an
/// [`instant`], so a key compares as a time whatever its fraction.
fn run_key(filter: &RunFilter) -> Result<(Option<String>, Option<&str>), StoreError> {
    let Some(key) = filter.before.as_ref() else {
        return Ok((None, None));
    };
    Ok((
        Some(instant("before", &key.started_at)?),
        Some(key.id.as_str()),
    ))
}

/// The columns [`raw_run`] reads, in order.
const RUN_COLUMNS: &str = "id, kind, platform, repo, target, commit_sha, requester, trigger, status, started_at, finished_at, link, summary, error, heartbeat_at, check_id, superseded_by, loop_stop, loop_rounds";

fn raw_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRun> {
    Ok(RawRun {
        id: row.get(0)?,
        kind: row.get(1)?,
        platform: row.get(2)?,
        repo: row.get(3)?,
        target: row.get(4)?,
        commit: row.get(5)?,
        requester: row.get(6)?,
        trigger: row.get(7)?,
        status: row.get(8)?,
        started_at: row.get(9)?,
        finished_at: row.get(10)?,
        link: row.get(11)?,
        summary: row.get(12)?,
        error: row.get(13)?,
        heartbeat_at: row.get(14)?,
        check_id: row.get(15)?,
        superseded_by: row.get(16)?,
        loop_stop: row.get(17)?,
        loop_rounds: row.get(18)?,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

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

    /// The plan of a run listing, one line per step.
    fn run_listing_plan(store: &SqliteStore) -> Vec<String> {
        store
            .with(|c| {
                let mut statement = c.prepare(&format!(
                    "EXPLAIN QUERY PLAN SELECT {RUN_COLUMNS} FROM runs WHERE {RUN_FILTER}
                     ORDER BY {RUN_ORDER} LIMIT ?10 OFFSET ?11"
                ))?;
                let rows = statement.query_map(
                    params![
                        None::<String>,
                        None::<String>,
                        None::<String>,
                        None::<String>,
                        None::<i64>,
                        None::<String>,
                        None::<String>,
                        None::<String>,
                        None::<String>,
                        50,
                        0
                    ],
                    |row| row.get::<_, String>(3),
                )?;
                Ok(rows.collect::<Result<_, _>>()?)
            })
            .unwrap()
    }

    #[test]
    fn the_run_listing_walks_the_instant_index_instead_of_sorting() {
        let store = SqliteStore::in_memory().unwrap();
        let plan = run_listing_plan(&store);
        assert!(
            plan.iter()
                .any(|step| step.contains("USING INDEX runs_started_instant")),
            "{plan:?}"
        );
        assert!(
            !plan.iter().any(|step| step.contains("TEMP B-TREE")),
            "{plan:?}"
        );
    }
}
