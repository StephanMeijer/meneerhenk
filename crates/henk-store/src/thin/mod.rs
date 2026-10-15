//! The thin store: one [`RunStore`] over a small seam, in place of one
//! store per database (spike #601, candidate "thin").
//!
//! * [`seam`]: parameters and rows as plain values, `?1` placeholders,
//!   the SQL fragments where the databases differ, and `Db`, which runs a
//!   statement on either.
//! * `sqlite` and `pg`: the two ways to run it, the first on the blocking
//!   thread pool, the second on deadpool.
//! * [`migrate`]: one numbered sequence for both.

mod migrate;
mod pg;
mod seam;
mod sqlite;

use std::path::Path;

use async_trait::async_trait;
use henk_domain::run::{EventId, RunId, RunKind};
use time::OffsetDateTime;

use crate::store::RunStore;
use crate::types::{
    DayCounts, DraftDecision, DraftRecord, EventFacets, EventFilter, EventRecord,
    EventWithOutcomes, FindingAction, FindingRecord, InboundEvent, MAX_PAYLOAD_BYTES,
    MOST_SESSION_REVIEWS, NewRun, OutcomeFilter, OutcomeRecord, OutcomeRow, Page, PruneCounts,
    RawRun, RoundRecord, RunFilter, RunRecord, RunStatus, SessionEnding, SessionRecord,
    SessionRole, SessionStatus, Stage, StageRecord, StageState, StageWrite, StoreError, TokenUsage,
    ToolCallFilter, ToolCallListing, ToolCallRecord, ToolUsage, TranscriptRecord,
    TranscriptSummary, attach_outcomes, child_records, draft_verdict, event_id, kind_str,
    merge_days, platform_str, round_record, run_kind, run_platform, session_dots, session_endings,
    session_role, session_status, stage_dots, stage_records, to_i64, to_u32, to_u64,
};

use seam::{Db, Dialect, Row, Stmt, Value, in_list, params};

/// The run store over `SQLite` or `PostgreSQL`.
#[derive(Debug)]
pub struct ThinStore {
    db: Db,
}

impl ThinStore {
    /// Opens or creates the `SQLite` database at `path` and migrates it.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the file cannot be opened or migrated.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        Ok(Self {
            db: Db::Sqlite(sqlite::SqliteDb::open(Some(path), migrate::STEPS.len())?),
        })
    }

    /// An in-memory `SQLite` store, for tests and probes.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when `SQLite` cannot create the database.
    pub fn in_memory() -> Result<Self, StoreError> {
        Ok(Self {
            db: Db::Sqlite(sqlite::SqliteDb::open(None, migrate::STEPS.len())?),
        })
    }

    /// Connects to the `PostgreSQL` database at `url` and brings its schema
    /// up to date. `sslmode` in the URL decides TLS.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the URL does not parse, the server cannot
    /// be reached, or migrating fails. The URL itself is never in the error.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        Self::connect_at(url, migrate::STEPS.len()).await
    }

    /// Opens `path` like [`Self::open`], migrated only up to step `version`.
    ///
    /// # Errors
    ///
    /// As [`Self::open`].
    #[cfg(feature = "testing")]
    pub fn open_at(path: &Path, version: usize) -> Result<Self, StoreError> {
        Ok(Self {
            db: Db::Sqlite(sqlite::SqliteDb::open(Some(path), version)?),
        })
    }

    /// Connects like [`Self::connect`], migrated only up to step `version`.
    /// `PostgreSQL`'s old history recorded no state between steps 1 and 3
    /// or after 5 and 13, so a version there is not a place to stop at.
    ///
    /// # Errors
    ///
    /// As [`Self::connect`].
    pub async fn connect_at(url: &str, version: usize) -> Result<Self, StoreError> {
        Ok(Self {
            db: Db::Postgres(pg::PgDb::connect(url, version.min(migrate::STEPS.len())).await?),
        })
    }

    fn dialect(&self) -> Dialect {
        self.db.dialect()
    }

    async fn run(&self, sql: &str, params: Vec<Value>) -> Result<(), StoreError> {
        self.db.execute(sql, params).await.map(drop)
    }

    async fn rows(&self, sql: &str, params: Vec<Value>) -> Result<Vec<Row>, StoreError> {
        self.db.query(sql, params).await
    }

    async fn distinct(&self, column: &str) -> Result<Vec<String>, StoreError> {
        self.rows(
            &format!("SELECT DISTINCT {column} FROM inbound_events ORDER BY 1"),
            params![],
        )
        .await?
        .iter()
        .map(|r| r.text(0))
        .collect()
    }
}

/// The saturating number of a count.
fn count(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// The columns [`raw_run`] reads, in order.
const RUN_COLUMNS: &str = "id, kind, platform, repo, target, commit_sha, requester, trigger, status, started_at, finished_at, link, summary, error, heartbeat_at, check_id, superseded_by, loop_stop, loop_rounds";

fn raw_run(row: &Row) -> Result<RawRun, StoreError> {
    Ok(RawRun {
        id: row.text(0)?,
        kind: row.text(1)?,
        platform: row.text(2)?,
        repo: row.text(3)?,
        target: row.int(4)?,
        commit: row.opt_text(5)?,
        requester: row.opt_text(6)?,
        trigger: row.text(7)?,
        status: row.text(8)?,
        started_at: row.text(9)?,
        finished_at: row.opt_text(10)?,
        link: row.text(11)?,
        summary: row.opt_text(12)?,
        error: row.opt_text(13)?,
        heartbeat_at: row.opt_text(14)?,
        check_id: row.opt_text(15)?,
        superseded_by: row.opt_text(16)?,
        loop_stop: row.opt_text(17)?,
        loop_rounds: row.opt_int(18)?,
    })
}

fn runs(rows: &[Row]) -> Result<Vec<RunRecord>, StoreError> {
    rows.iter().map(|row| raw_run(row)?.into_record()).collect()
}

/// The columns [`inbound`] reads, in order.
const EVENT_COLUMNS: &str = "id, received_at, source, kind, repo, target, payload, requester";

fn inbound(row: &Row) -> Result<InboundEvent, StoreError> {
    Ok(InboundEvent {
        id: event_id(row.text(0)?)?,
        received_at: row.text(1)?,
        source: row.text(2)?,
        kind: row.text(3)?,
        repo: row.opt_text(4)?,
        target: row
            .opt_int(5)?
            .map(|t| to_u64("inbound_events.target", t))
            .transpose()?,
        payload: row.opt_text(6)?,
        requester: row.opt_text(7)?,
    })
}

fn outcome_row(row: &Row) -> Result<OutcomeRow, StoreError> {
    Ok(OutcomeRow {
        event_id: row.text(0)?,
        listener: row.text(1)?,
        outcome: row.text(2)?,
        detail: row.text(3)?,
        run_id: row.opt_text(4)?,
        at: row.text(5)?,
    })
}

/// The time bounds of a run or tool call listing, as instants to compare.
fn window(since: Option<&String>, until: Option<&String>) -> Result<(Value, Value), StoreError> {
    Ok((
        Value::instant_or_none("since", since.map(String::as_str))?,
        Value::instant_or_none("until", until.map(String::as_str))?,
    ))
}

/// The `WHERE` of a run listing, over parameters 1 to 9: kind, status,
/// platform, repo, target, since, until, and the keyset (time, id). Times
/// are compared as instants (#216).
fn run_filter(d: Dialect) -> String {
    let started = d.instant("started_at");
    format!(
        "(?1 IS NULL OR kind = ?1) AND (?2 IS NULL OR status = ?2)
         AND (?3 IS NULL OR platform = ?3) AND (?4 IS NULL OR repo = ?4)
         AND (?5 IS NULL OR target = ?5)
         AND (?6 IS NULL OR {started} >= ?6) AND (?7 IS NULL OR {started} < ?7)
         AND (?8 IS NULL OR {started} < ?8 OR ({started} = ?8 AND id < ?9))"
    )
}

/// A page of runs, newest first as a time, over parameters 1 to 9 of
/// [`run_filter`] and 10 (the limit) and 11 (the offset).
fn run_listing(d: Dialect) -> String {
    format!(
        "SELECT {RUN_COLUMNS} FROM runs WHERE {filter}
         ORDER BY {started} DESC, id DESC LIMIT ?10 OFFSET ?11",
        filter = run_filter(d),
        started = d.instant("started_at"),
    )
}

/// A run filter as parameters 1 to 9 of [`run_filter`].
fn run_params(filter: &RunFilter) -> Result<Vec<Value>, StoreError> {
    let (since, until) = window(filter.since.as_ref(), filter.until.as_ref())?;
    let before = Value::instant_or_none(
        "before",
        filter.before.as_ref().map(|key| key.started_at.as_str()),
    )?;
    Ok(params![
        filter.kind.map(kind_str),
        filter.status.map(RunStatus::as_str),
        filter.platform.map(platform_str),
        filter.repo,
        filter.target.map(count),
        since,
        until,
        before,
        filter.before.as_ref().map(|key| key.id.as_str()),
    ])
}

/// The session row of a tool call `c`, as `s`, for its role.
const CALL_SESSION: &str = "LEFT JOIN sessions s ON s.run_id = c.run_id AND s.name = c.session";

/// The role of the session that made a tool call `c`, over
/// [`CALL_SESSION`]. A call whose session has no row counts as a lane's.
const CALL_ROLE: &str = "COALESCE(s.role, 'lane')";

/// The tool a call `c` counts under, as [`crate::types::counted_as`] says:
/// every tool that does not exist, which is every call with no origin, as
/// one (#341). [`call_filter`] matches a tool the same way.
const CALL_TOOL: &str = "CASE WHEN c.origin = '' THEN '(unknown)' ELSE c.tool END";

/// The `WHERE` of a tool call tally or listing over `tool_calls c` and
/// [`CALL_SESSION`], over parameters 1 to 5: tool, model, session role,
/// since and until, the last two as instants.
fn call_filter(d: Dialect) -> String {
    let at = d.instant("c.at");
    format!(
        "(?1 IS NULL OR {CALL_TOOL} = ?1) AND (?2 IS NULL OR c.model = ?2)
         AND (?3 IS NULL OR {CALL_ROLE} = ?3)
         AND (?4 IS NULL OR {at} >= ?4) AND (?5 IS NULL OR {at} < ?5)"
    )
}

/// A tool call filter as parameters 1 to 5 of [`call_filter`].
fn call_params(filter: &ToolCallFilter) -> Result<Vec<Value>, StoreError> {
    let (since, until) = window(filter.since.as_ref(), filter.until.as_ref())?;
    Ok(params![
        filter.tool,
        filter.model,
        filter.role.map(SessionRole::as_str),
        since,
        until,
    ])
}

#[async_trait]
impl RunStore for ThinStore {
    async fn create_run(&self, run: &NewRun) -> Result<(), StoreError> {
        self.run(
            "INSERT INTO runs (id, kind, platform, repo, target, commit_sha, requester, trigger, status, started_at, link, heartbeat_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?10)",
            params![
                run.id.as_str(),
                kind_str(run.kind),
                platform_str(run.platform),
                run.repo,
                to_i64("runs.target", run.target)?,
                run.commit,
                run.requester,
                run.trigger,
                RunStatus::Running.as_str(),
                Value::now(),
                run.link,
            ],
        )
        .await
    }

    async fn finish_run(
        &self,
        id: &RunId,
        status: RunStatus,
        summary: Option<&str>,
        error: Option<&str>,
    ) -> Result<(), StoreError> {
        self.run(
            "UPDATE runs SET status = ?2, finished_at = ?3, summary = ?4, error = ?5 WHERE id = ?1",
            params![id.as_str(), status.as_str(), Value::now(), summary, error],
        )
        .await
    }

    async fn supersede_run(&self, id: &RunId, by: &RunId, reason: &str) -> Result<(), StoreError> {
        self.run(
            "UPDATE runs SET status = ?2, finished_at = ?3, error = ?4, superseded_by = ?5 WHERE id = ?1",
            params![
                id.as_str(),
                RunStatus::Superseded.as_str(),
                Value::now(),
                reason,
                by.as_str()
            ],
        )
        .await
    }

    async fn run(&self, id: &RunId) -> Result<Option<RunRecord>, StoreError> {
        let rows = self
            .rows(
                &format!("SELECT {RUN_COLUMNS} FROM runs WHERE id = ?1"),
                params![id.as_str()],
            )
            .await?;
        runs(&rows).map(|found| found.into_iter().next())
    }

    async fn heartbeat(&self, id: &RunId) -> Result<(), StoreError> {
        self.run(
            "UPDATE runs SET heartbeat_at = ?2 WHERE id = ?1",
            params![id.as_str(), Value::now()],
        )
        .await
    }

    async fn end_loop(&self, id: &RunId, stop: &str, rounds: u32) -> Result<(), StoreError> {
        self.run(
            "UPDATE runs SET loop_stop = ?2, loop_rounds = ?3 WHERE id = ?1",
            params![id.as_str(), stop, rounds],
        )
        .await
    }

    async fn set_check(&self, id: &RunId, check_id: &str) -> Result<(), StoreError> {
        self.run(
            "UPDATE runs SET check_id = ?2 WHERE id = ?1",
            params![id.as_str(), check_id],
        )
        .await
    }

    async fn orphaned_runs(
        &self,
        stale_before: OffsetDateTime,
    ) -> Result<Vec<RunRecord>, StoreError> {
        let d = self.dialect();
        let rows = self
            .rows(
                &format!(
                    "SELECT {RUN_COLUMNS} FROM runs
                     WHERE status = ?1 AND (heartbeat_at IS NULL OR {heartbeat} < ?2)
                     ORDER BY {started}, id",
                    heartbeat = d.instant("heartbeat_at"),
                    started = d.instant("started_at"),
                ),
                params![RunStatus::Running.as_str(), Value::instant_at(stale_before)],
            )
            .await?;
        runs(&rows)
    }

    async fn latest_reviews(&self, since: OffsetDateTime) -> Result<Vec<RunRecord>, StoreError> {
        let d = self.dialect();
        let (r, n) = (d.instant("r.started_at"), d.instant("n.started_at"));
        let rows = self
            .rows(
                &format!(
                    "SELECT {RUN_COLUMNS} FROM runs r
                     WHERE r.kind = ?1 AND {r} >= ?2 AND NOT EXISTS (
                         SELECT 1 FROM runs n
                         WHERE n.kind = r.kind AND n.platform = r.platform
                           AND n.repo = r.repo AND n.target = r.target
                           AND ({n} > {r} OR ({n} = {r} AND n.id > r.id)))
                     ORDER BY {r}, r.id",
                ),
                params![kind_str(RunKind::Review), Value::instant_at(since)],
            )
            .await?;
        runs(&rows)
    }

    async fn claim_resume(
        &self,
        run: &RunId,
        interrupted: &str,
        resumed: &str,
    ) -> Result<bool, StoreError> {
        let changed = self
            .db
            .execute(
                "UPDATE runs SET error = ?4 WHERE id = ?1 AND status = ?2 AND error = ?3",
                params![
                    run.as_str(),
                    RunStatus::Failed.as_str(),
                    interrupted,
                    resumed
                ],
            )
            .await?;
        Ok(changed == 1)
    }

    async fn drop_running_sessions(&self, run: &RunId, reason: &str) -> Result<(), StoreError> {
        self.run(
            "UPDATE sessions SET status = ?2, finished_at = ?3, error = ?4
             WHERE run_id = ?1 AND status = ?5",
            params![
                run.as_str(),
                SessionStatus::DidNotFinish.as_str(),
                Value::now(),
                reason,
                SessionStatus::Running.as_str()
            ],
        )
        .await
    }

    async fn stage(&self, run: &RunId, write: &StageWrite) -> Result<(), StoreError> {
        let at = |time: Option<OffsetDateTime>| time.map_or_else(Value::now, Value::time_at);
        let ended = if write.state.ended() {
            at(write.ended_at)
        } else {
            Value::no_time()
        };
        self.run(
            "INSERT INTO stages (run_id, stage, state, started_at, ended_at, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (run_id, stage) DO UPDATE SET
                 state = excluded.state, ended_at = excluded.ended_at, detail = excluded.detail",
            params![
                run.as_str(),
                write.stage.as_str(),
                write.state.as_str(),
                at(write.started_at),
                ended,
                write.detail
            ],
        )
        .await
    }

    async fn fail_running_stages(&self, run: &RunId, reason: &str) -> Result<(), StoreError> {
        self.run(
            "UPDATE stages SET state = ?2, ended_at = ?3, detail = ?4
             WHERE run_id = ?1 AND state = ?5",
            params![
                run.as_str(),
                StageState::Failed.as_str(),
                Value::now(),
                reason,
                StageState::Running.as_str()
            ],
        )
        .await
    }

    async fn stages(&self, run: &RunId) -> Result<Vec<StageRecord>, StoreError> {
        let rows = self
            .rows(
                "SELECT stage, state, started_at, ended_at, detail FROM stages WHERE run_id = ?1",
                params![run.as_str()],
            )
            .await?;
        stage_records(
            rows.iter()
                .map(|r| {
                    Ok((
                        r.text(0)?,
                        r.text(1)?,
                        r.text(2)?,
                        r.opt_text(3)?,
                        r.text(4)?,
                    ))
                })
                .collect::<Result<_, StoreError>>()?,
        )
    }

    async fn link_child(
        &self,
        parent: &RunId,
        stage: Stage,
        child: &RunId,
    ) -> Result<(), StoreError> {
        self.run(
            "INSERT INTO run_children (parent, stage, child) VALUES (?1, ?2, ?3)
             ON CONFLICT (parent, stage) DO UPDATE SET child = excluded.child",
            params![parent.as_str(), stage.as_str(), child.as_str()],
        )
        .await
    }

    async fn children(&self, parent: &RunId) -> Result<Vec<(Stage, RunId)>, StoreError> {
        let rows = self
            .rows(
                "SELECT stage, child FROM run_children WHERE parent = ?1",
                params![parent.as_str()],
            )
            .await?;
        child_records(
            rows.iter()
                .map(|r| Ok((r.text(0)?, r.text(1)?)))
                .collect::<Result<_, StoreError>>()?,
        )
    }

    async fn start_session(
        &self,
        run: &RunId,
        name: &str,
        role: SessionRole,
        model: &str,
    ) -> Result<(), StoreError> {
        self.run(
            "INSERT INTO sessions (run_id, name, role, model, status, started_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                run.as_str(),
                name,
                role.as_str(),
                model,
                SessionStatus::Running.as_str(),
                Value::now()
            ],
        )
        .await
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
        self.run(
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
                Value::now(),
                error,
                count(tokens.cache_read),
                count(tokens.cache_write),
                count(tokens.reasoning),
            ],
        )
        .await
    }

    async fn sessions(&self, run: &RunId) -> Result<Vec<SessionRecord>, StoreError> {
        let rows = self
            .rows(
                "SELECT name, model, status, turns, input_tokens, output_tokens, error, started_at, finished_at, cache_read_tokens, cache_write_tokens, role, reasoning_tokens FROM sessions WHERE run_id = ?1 ORDER BY name",
                params![run.as_str()],
            )
            .await?;
        rows.iter()
            .map(|r| {
                Ok(SessionRecord {
                    name: r.text(0)?,
                    role: session_role(r.text(11)?)?,
                    model: r.text(1)?,
                    status: session_status(r.text(2)?)?,
                    turns: to_u64("sessions.turns", r.int(3)?)?,
                    input_tokens: to_u64("sessions.input_tokens", r.int(4)?)?,
                    output_tokens: to_u64("sessions.output_tokens", r.int(5)?)?,
                    cache_read_tokens: to_u64("sessions.cache_read_tokens", r.int(9)?)?,
                    cache_write_tokens: to_u64("sessions.cache_write_tokens", r.int(10)?)?,
                    reasoning_tokens: to_u64("sessions.reasoning_tokens", r.int(12)?)?,
                    error: r.opt_text(6)?,
                    started_at: r.text(7)?,
                    finished_at: r.opt_text(8)?,
                })
            })
            .collect()
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
        self.run(
            "INSERT INTO findings (run_id, session, path, line, comment_id, action, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                run.as_str(),
                session,
                path,
                line_number,
                comment_id,
                action.as_str(),
                Value::now()
            ],
        )
        .await
    }

    async fn record_draft(&self, run: &RunId, draft: &DraftRecord) -> Result<(), StoreError> {
        self.run(
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
                Value::time("drafts.created_at", &draft.at)?
            ],
        )
        .await
    }

    async fn decide_draft(
        &self,
        run: &RunId,
        draft: &str,
        decision: &DraftDecision,
    ) -> Result<(), StoreError> {
        self.run(
            "UPDATE drafts SET verdict = ?3, checker = ?4, reason = ?5, same_as = ?6, comment_id = ?7, decided_at = ?8 WHERE run_id = ?1 AND draft = ?2",
            params![
                run.as_str(),
                draft,
                decision.verdict.as_str(),
                decision.checker,
                decision.reason,
                decision.same_as,
                decision.comment_id,
                Value::time("drafts.decided_at", &decision.at)?
            ],
        )
        .await
    }

    async fn drafts(&self, run: &RunId) -> Result<Vec<DraftRecord>, StoreError> {
        let rows = self
            .rows(
                "SELECT created_at, draft, session, model, kind, path, line, target, body, verdict, checker, reason, same_as, comment_id, decided_at FROM drafts WHERE run_id = ?1 ORDER BY id",
                params![run.as_str()],
            )
            .await?;
        rows.iter()
            .map(|r| {
                let decision = r
                    .opt_text(9)?
                    .map(|verdict| {
                        Ok::<_, StoreError>(DraftDecision {
                            at: r.opt_text(14)?.unwrap_or_default(),
                            verdict: draft_verdict(&verdict)?,
                            checker: r.text(10)?,
                            reason: r.text(11)?,
                            same_as: r.text(12)?,
                            comment_id: r.text(13)?,
                        })
                    })
                    .transpose()?;
                Ok(DraftRecord {
                    at: r.text(0)?,
                    draft: r.text(1)?,
                    session: r.text(2)?,
                    model: r.text(3)?,
                    kind: r.text(4)?,
                    path: r.text(5)?,
                    line: to_u32("drafts.line", r.int(6)?)?,
                    target: r.text(7)?,
                    body: r.text(8)?,
                    decision,
                })
            })
            .collect()
    }

    async fn record_transcript(
        &self,
        run: &RunId,
        transcript: &TranscriptRecord,
    ) -> Result<(), StoreError> {
        self.run(
            "INSERT INTO transcripts (run_id, session, model, stop, turns, bytes, body, recorded_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                run.as_str(),
                transcript.session,
                transcript.model,
                transcript.stop,
                transcript.turns,
                count(transcript.bytes),
                transcript.body,
                Value::time("transcripts.recorded_at", &transcript.at)?
            ],
        )
        .await
    }

    async fn transcripts(&self, run: &RunId) -> Result<Vec<TranscriptSummary>, StoreError> {
        let rows = self
            .rows(
                "SELECT recorded_at, session, model, stop, turns, bytes FROM transcripts WHERE run_id = ?1 ORDER BY id",
                params![run.as_str()],
            )
            .await?;
        rows.iter()
            .map(|r| {
                Ok(TranscriptSummary {
                    at: r.text(0)?,
                    session: r.text(1)?,
                    model: r.text(2)?,
                    stop: r.text(3)?,
                    turns: to_u32("transcripts.turns", r.int(4)?)?,
                    bytes: u64::try_from(r.int(5)?).unwrap_or_default(),
                })
            })
            .collect()
    }

    async fn transcript(
        &self,
        run: &RunId,
        session: &str,
    ) -> Result<Option<TranscriptRecord>, StoreError> {
        let rows = self
            .rows(
                "SELECT recorded_at, session, model, stop, turns, bytes, body FROM transcripts WHERE run_id = ?1 AND session = ?2 ORDER BY id DESC LIMIT 1",
                params![run.as_str(), session],
            )
            .await?;
        rows.first()
            .map(|r| {
                Ok(TranscriptRecord {
                    at: r.text(0)?,
                    session: r.text(1)?,
                    model: r.text(2)?,
                    stop: r.text(3)?,
                    turns: to_u32("transcripts.turns", r.int(4)?)?,
                    bytes: u64::try_from(r.int(5)?).unwrap_or_default(),
                    body: r.text(6)?,
                })
            })
            .transpose()
    }

    async fn record_tool_call(&self, run: &RunId, call: &ToolCallRecord) -> Result<(), StoreError> {
        self.run(
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
                count(call.arguments_len),
                count(call.result_chars),
                count(call.elapsed_ms),
                Value::time("tool_calls.at", &call.at)?
            ],
        )
        .await
    }

    async fn tool_calls(&self, run: &RunId) -> Result<Vec<ToolCallRecord>, StoreError> {
        let rows = self
            .rows(
                "SELECT at, session, model, turn, tool, origin, outcome, arguments, arguments_len, result_chars, elapsed_ms FROM tool_calls WHERE run_id = ?1 ORDER BY id",
                params![run.as_str()],
            )
            .await?;
        rows.iter().map(|r| tool_call(r, 0)).collect()
    }

    async fn tool_usage(&self, filter: &ToolCallFilter) -> Result<Vec<ToolUsage>, StoreError> {
        let rows = self
            .rows(
                &format!(
                    "SELECT c.model, {CALL_ROLE}, {CALL_TOOL}, c.outcome, COUNT(*),
                            CAST(COALESCE(SUM(c.elapsed_ms), 0) AS BIGINT)
                     FROM tool_calls c {CALL_SESSION} WHERE {where_}
                     GROUP BY c.model, 2, 3, c.outcome",
                    where_ = call_filter(self.dialect()),
                ),
                call_params(filter)?,
            )
            .await?;
        let number = |n: i64| u64::try_from(n).unwrap_or_default();
        let tallies = rows
            .iter()
            .map(|r| {
                Ok((
                    r.text(0)?,
                    r.text(1)?,
                    r.text(2)?,
                    r.text(3)?,
                    number(r.int(4)?),
                    number(r.int(5)?),
                ))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        Ok(ToolUsage::across_runs(tallies))
    }

    async fn list_tool_calls(
        &self,
        filter: &ToolCallFilter,
        page: Page,
    ) -> Result<Vec<ToolCallListing>, StoreError> {
        let mut values = call_params(filter)?;
        values.extend(params![
            OutcomeFilter::param(filter.outcome.as_ref()),
            Value::time_or_none("at", filter.before.as_ref().map(|key| key.at.as_str()))?,
            filter.before.as_ref().map(|key| key.id),
            page.limit(),
            page.offset(),
        ]);
        let rows = self
            .rows(
                &format!(
                    "SELECT c.id, c.run_id, r.repo, r.target, r.platform, c.at, c.session, c.model,
                            c.turn, c.tool, c.origin, c.outcome, c.arguments, c.arguments_len,
                            c.result_chars, c.elapsed_ms,
                            EXISTS (SELECT 1 FROM transcripts t
                                    WHERE t.run_id = c.run_id AND t.session = c.session),
                            r.kind
                     FROM tool_calls c JOIN runs r ON r.id = c.run_id {CALL_SESSION}
                     WHERE {where_}
                       AND (?6 IS NULL OR (?6 = 'problems' AND c.outcome <> 'ok') OR c.outcome = ?6)
                       AND (?7 IS NULL OR c.at < ?7 OR (c.at = ?7 AND c.id < ?8))
                     ORDER BY c.at DESC, c.id DESC LIMIT ?9 OFFSET ?10",
                    where_ = call_filter(self.dialect()),
                ),
                values,
            )
            .await?;
        rows.iter()
            .map(|r| {
                Ok(ToolCallListing {
                    id: r.int(0)?,
                    run_id: r.text(1)?,
                    repo: r.text(2)?,
                    target: to_u64("runs.target", r.int(3)?)?,
                    platform: run_platform(r.text(4)?)?,
                    kind: run_kind(r.text(17)?)?,
                    call: tool_call(r, 5)?,
                    transcript_kept: r.bool(16)?,
                })
            })
            .collect()
    }

    async fn findings(&self, run: &RunId) -> Result<Vec<FindingRecord>, StoreError> {
        let rows = self
            .rows(
                "SELECT created_at, session, path, line, comment_id, action FROM findings WHERE run_id = ?1 ORDER BY id",
                params![run.as_str()],
            )
            .await?;
        rows.iter()
            .map(|r| {
                Ok(FindingRecord {
                    at: r.text(0)?,
                    session: r.text(1)?,
                    path: r.text(2)?,
                    line: to_u32("findings.line", r.int(3)?)?,
                    comment_id: r.text(4)?,
                    action: r.text(5)?,
                })
            })
            .collect()
    }

    async fn start_round(&self, run: &RunId, round: &RoundRecord) -> Result<(), StoreError> {
        self.run(
            "INSERT INTO session_rounds (run_id, session, role, round, started_at, first_turn, handover, handover_len)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                run.as_str(),
                round.session,
                round.role.as_str(),
                round.round,
                Value::time("session_rounds.started_at", &round.started_at)?,
                round.first_turn,
                round.handover,
                to_i64("session_rounds.handover_len", round.handover_len)?,
            ],
        )
        .await
    }

    async fn end_round(
        &self,
        run: &RunId,
        session: &str,
        round: u32,
        turns: u32,
    ) -> Result<(), StoreError> {
        self.run(
            "UPDATE session_rounds SET ended_at = ?4, turns = ?5
             WHERE run_id = ?1 AND session = ?2 AND round = ?3",
            params![run.as_str(), session, round, Value::now(), turns],
        )
        .await
    }

    async fn rounds(&self, run: &RunId) -> Result<Vec<RoundRecord>, StoreError> {
        let rows = self
            .rows(
                &format!(
                    "SELECT session, role, round, started_at, ended_at, first_turn, turns, handover, handover_len
                     FROM session_rounds WHERE run_id = ?1
                     ORDER BY {started}, session, round",
                    started = self.dialect().instant("started_at"),
                ),
                params![run.as_str()],
            )
            .await?;
        rows.iter()
            .map(|r| {
                round_record((
                    r.text(0)?,
                    r.text(1)?,
                    r.int(2)?,
                    r.text(3)?,
                    r.opt_text(4)?,
                    r.int(5)?,
                    r.int(6)?,
                    r.text(7)?,
                    r.int(8)?,
                ))
            })
            .collect()
    }

    async fn event(&self, run: &RunId, level: &str, message: &str) -> Result<(), StoreError> {
        self.run(
            "INSERT INTO events (run_id, at, level, message) VALUES (?1, ?2, ?3, ?4)",
            params![run.as_str(), Value::now(), level, message],
        )
        .await
    }

    async fn events(&self, run: &RunId) -> Result<Vec<EventRecord>, StoreError> {
        let rows = self
            .rows(
                "SELECT at, level, message FROM events WHERE run_id = ?1 ORDER BY id",
                params![run.as_str()],
            )
            .await?;
        rows.iter()
            .map(|r| {
                Ok(EventRecord {
                    at: r.text(0)?,
                    level: r.text(1)?,
                    message: r.text(2)?,
                })
            })
            .collect()
    }

    async fn joined(&self, run: &RunId, source: &str) -> Result<(), StoreError> {
        self.run(
            "INSERT INTO requests (run_id, at, source) VALUES (?1, ?2, ?3)",
            params![run.as_str(), Value::now(), source],
        )
        .await
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
        self.run(
            &format!("INSERT INTO inbound_events ({EVENT_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"),
            params![
                event.id.as_str(),
                Value::time("inbound_events.received_at", &event.received_at)?,
                event.source,
                event.kind,
                event.repo,
                target,
                payload,
                event.requester,
            ],
        )
        .await
    }

    async fn record_outcome(&self, outcome: &OutcomeRecord) -> Result<(), StoreError> {
        self.run(
            "INSERT INTO event_outcomes (event_id, listener, outcome, detail, run_id, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                outcome.event_id.as_str(),
                outcome.listener,
                outcome.outcome,
                outcome.detail,
                outcome.run_id,
                Value::time("event_outcomes.at", &outcome.at)?,
            ],
        )
        .await
    }

    async fn inbound_event(&self, id: &EventId) -> Result<Option<InboundEvent>, StoreError> {
        let rows = self
            .rows(
                &format!("SELECT {EVENT_COLUMNS} FROM inbound_events WHERE id = ?1"),
                params![id.as_str()],
            )
            .await?;
        rows.first().map(inbound).transpose()
    }

    async fn event_facets(&self) -> Result<EventFacets, StoreError> {
        Ok(EventFacets {
            sources: self.distinct("source").await?,
            kinds: self.distinct("kind").await?,
        })
    }

    async fn outcomes(&self, id: &EventId) -> Result<Vec<OutcomeRecord>, StoreError> {
        let rows = self
            .rows(
                &format!(
                    "SELECT listener, outcome, detail, run_id, at FROM event_outcomes WHERE event_id = ?1 ORDER BY {}",
                    self.dialect().written()
                ),
                params![id.as_str()],
            )
            .await?;
        rows.iter()
            .map(|r| {
                Ok(OutcomeRecord {
                    event_id: id.clone(),
                    listener: r.text(0)?,
                    outcome: r.text(1)?,
                    detail: r.text(2)?,
                    run_id: r.opt_text(3)?,
                    at: r.text(4)?,
                })
            })
            .collect()
    }

    async fn inbound_events_for_run(&self, run: &RunId) -> Result<Vec<InboundEvent>, StoreError> {
        let rows = self
            .rows(
                "SELECT e.id, e.received_at, e.source, e.kind, e.repo, e.target, e.payload, e.requester
                 FROM inbound_events e
                 WHERE e.id IN (SELECT o.event_id FROM event_outcomes o WHERE o.run_id = ?1)
                 ORDER BY e.received_at, e.id",
                params![run.as_str()],
            )
            .await?;
        rows.iter().map(inbound).collect()
    }

    async fn prune_events(&self, older_than: OffsetDateTime) -> Result<PruneCounts, StoreError> {
        let cutoff = Value::time_at(older_than);
        let statement = |sql: &str| Stmt {
            sql: sql.to_owned(),
            params: vec![cutoff.clone()],
        };
        let changed = self
            .db
            .atomically(vec![
                statement(
                    "DELETE FROM event_outcomes WHERE event_id IN
                     (SELECT id FROM inbound_events WHERE received_at < ?1)",
                ),
                statement("DELETE FROM inbound_events WHERE received_at < ?1"),
                statement("DELETE FROM transcripts WHERE recorded_at < ?1"),
            ])
            .await?;
        let [outcomes, events, transcripts] = changed.as_slice() else {
            return Err(StoreError::Schema("prune ran three statements".to_owned()));
        };
        let (outcomes, events, transcripts) = (*outcomes, *events, *transcripts);
        Ok(PruneCounts {
            events,
            outcomes,
            transcripts,
        })
    }

    async fn list_runs(
        &self,
        filter: &RunFilter,
        page: Page,
    ) -> Result<Vec<RunRecord>, StoreError> {
        let mut values = run_params(filter)?;
        values.extend(params![page.limit(), page.offset()]);
        let rows = self.rows(&run_listing(self.dialect()), values).await?;
        runs(&rows)
    }

    async fn daily_stats(&self, since: OffsetDateTime) -> Result<Vec<DayCounts>, StoreError> {
        let d = self.dialect();
        let since = Value::instant_at(since);
        let pairs = |rows: Vec<Row>| -> Result<Vec<(String, i64)>, StoreError> {
            rows.iter().map(|r| Ok((r.text(0)?, r.int(1)?))).collect()
        };
        let runs = self
            .rows(
                &format!(
                    "SELECT {day}, COUNT(*),
                            SUM(CASE WHEN status = 'finished' THEN 1 ELSE 0 END),
                            SUM(CASE WHEN status = 'failed' THEN 1 ELSE 0 END)
                     FROM runs WHERE {at} >= ?1 GROUP BY 1",
                    day = d.day("started_at"),
                    at = d.instant("started_at"),
                ),
                params![since],
            )
            .await?
            .iter()
            .map(|r| Ok((r.text(0)?, r.int(1)?, r.int(2)?, r.int(3)?)))
            .collect::<Result<Vec<_>, StoreError>>()?;
        let findings = pairs(
            self.rows(
                &format!(
                    "SELECT {day}, COUNT(*) FROM findings
                     WHERE action IN ('posted', 'unverified') AND {at} >= ?1 GROUP BY 1",
                    day = d.day("created_at"),
                    at = d.instant("created_at"),
                ),
                params![since],
            )
            .await?,
        )?;
        let drafts = pairs(
            self.rows(
                &format!(
                    "SELECT {day}, COUNT(*) FROM drafts
                     WHERE {at} >= ?1 AND kind NOT IN ('loop_finding', 'plan_point', 'requirement_point') GROUP BY 1",
                    day = d.day("created_at"),
                    at = d.instant("created_at"),
                ),
                params![since],
            )
            .await?,
        )?;
        Ok(merge_days(runs, findings, drafts))
    }

    async fn sessions_of(
        &self,
        runs: &[RunId],
    ) -> Result<Vec<(RunId, String, SessionRole, SessionStatus)>, StoreError> {
        if runs.is_empty() {
            return Ok(Vec::new());
        }
        let rows = self
            .rows(
                &format!(
                    "SELECT run_id, name, role, status FROM sessions WHERE run_id IN ({}) ORDER BY run_id, name",
                    in_list(1, runs.len())
                ),
                runs.iter().map(|run| Value::Text(run.as_str().to_owned())).collect(),
            )
            .await?;
        session_dots(
            rows.iter()
                .map(|r| Ok((r.text(0)?, r.text(1)?, r.text(2)?, r.text(3)?)))
                .collect::<Result<_, StoreError>>()?,
        )
    }

    async fn session_endings(
        &self,
        since: Option<OffsetDateTime>,
        reviews: u32,
    ) -> Result<Vec<SessionEnding>, StoreError> {
        let d = self.dialect();
        let (inner, outer) = (d.instant("started_at"), d.instant("r.started_at"));
        let rows = self
            .rows(
                &format!(
                    "SELECT r.id, r.started_at, s.name, s.role, s.model, s.status, s.error
                     FROM (SELECT id, started_at FROM runs
                           WHERE kind = 'review' AND status NOT IN ('running', 'superseded', 'cancelled')
                             AND (?1 IS NULL OR {inner} >= ?1)
                           ORDER BY {inner} DESC, id DESC LIMIT ?2) r
                     JOIN sessions s ON s.run_id = r.id
                     ORDER BY {outer} DESC, r.id DESC, s.name"
                ),
                params![
                    since.map_or_else(Value::no_time, Value::instant_at),
                    i64::from(reviews.min(MOST_SESSION_REVIEWS))
                ],
            )
            .await?;
        session_endings(
            rows.iter()
                .map(|r| {
                    Ok((
                        r.text(0)?,
                        r.text(1)?,
                        r.text(2)?,
                        r.text(3)?,
                        r.text(4)?,
                        r.text(5)?,
                        r.opt_text(6)?,
                    ))
                })
                .collect::<Result<_, StoreError>>()?,
        )
    }

    async fn stages_of(
        &self,
        runs: &[RunId],
    ) -> Result<Vec<(RunId, Stage, StageState)>, StoreError> {
        if runs.is_empty() {
            return Ok(Vec::new());
        }
        let rows = self
            .rows(
                &format!(
                    "SELECT run_id, stage, state FROM stages WHERE run_id IN ({})",
                    in_list(1, runs.len())
                ),
                runs.iter()
                    .map(|run| Value::Text(run.as_str().to_owned()))
                    .collect(),
            )
            .await?;
        stage_dots(
            rows.iter()
                .map(|r| Ok((r.text(0)?, r.text(1)?, r.text(2)?)))
                .collect::<Result<_, StoreError>>()?,
        )
    }

    async fn count_runs(&self, filter: &RunFilter) -> Result<u64, StoreError> {
        let rows = self
            .rows(
                &format!(
                    "SELECT COUNT(*) FROM runs WHERE {}",
                    run_filter(self.dialect())
                ),
                run_params(filter)?,
            )
            .await?;
        let count = rows.first().map_or(Ok(0), |r| r.int(0))?;
        Ok(u64::try_from(count).unwrap_or_default())
    }

    async fn list_inbound_events(
        &self,
        filter: &EventFilter,
        page: Page,
    ) -> Result<Vec<EventWithOutcomes>, StoreError> {
        let before = filter.before.as_ref();
        let events = self
            .rows(
                &format!(
                    "SELECT {EVENT_COLUMNS} FROM inbound_events
                     WHERE (?1 IS NULL OR source = ?1) AND (?2 IS NULL OR kind = ?2) AND (?3 IS NULL OR repo = ?3)
                       AND (?4 IS NULL OR received_at < ?4 OR (received_at = ?4 AND id < ?5))
                     ORDER BY received_at DESC, id DESC LIMIT ?6 OFFSET ?7"
                ),
                params![
                    filter.source,
                    filter.kind,
                    filter.repo,
                    Value::time_or_none(
                        "received_at",
                        before.map(|key| key.received_at.as_str())
                    )?,
                    before.map(|key| key.id.as_str()),
                    page.limit(),
                    page.offset(),
                ],
            )
            .await?
            .iter()
            .map(inbound)
            .collect::<Result<Vec<_>, _>>()?;
        if events.is_empty() {
            return Ok(Vec::new());
        }
        // One query for every outcome of the page, not one per event.
        let outcomes = self
            .rows(
                &format!(
                    "SELECT event_id, listener, outcome, detail, run_id, at FROM event_outcomes
                     WHERE event_id IN ({}) ORDER BY {}",
                    in_list(1, events.len()),
                    self.dialect().written()
                ),
                events
                    .iter()
                    .map(|e| Value::Text(e.id.as_str().to_owned()))
                    .collect(),
            )
            .await?
            .iter()
            .map(outcome_row)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(attach_outcomes(events, &outcomes))
    }
}

/// A tool call from the eleven columns `tool_calls` selects, from `first`:
/// at, session, model, turn, tool, origin, outcome, arguments, arguments
/// length, result characters, elapsed.
fn tool_call(r: &Row, first: usize) -> Result<ToolCallRecord, StoreError> {
    let number = |i: usize| -> Result<u64, StoreError> {
        Ok(u64::try_from(r.int(first + i)?).unwrap_or_default())
    };
    Ok(ToolCallRecord {
        at: r.text(first)?,
        session: r.text(first + 1)?,
        model: r.text(first + 2)?,
        turn: to_u32("tool_calls.turn", r.int(first + 3)?)?,
        tool: r.text(first + 4)?,
        origin: r.text(first + 5)?,
        outcome: r.text(first + 6)?,
        arguments: r.text(first + 7)?,
        arguments_len: number(8)?,
        result_chars: number(9)?,
        elapsed_ms: number(10)?,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// The plan of a run listing, one line per step.
    async fn plan(store: &ThinStore) -> Vec<String> {
        let mut values = run_params(&RunFilter::default()).unwrap();
        values.extend(params![50_u32, 0_u32]);
        store
            .rows(
                &format!("EXPLAIN QUERY PLAN {}", run_listing(Dialect::Sqlite)),
                values,
            )
            .await
            .unwrap()
            .iter()
            .map(|row| row.text(3).unwrap())
            .collect()
    }

    /// The expression of step 13's index and [`Dialect::instant`] are one text.
    #[tokio::test]
    async fn the_run_listing_walks_the_instant_index_instead_of_sorting() {
        let store = ThinStore::in_memory().unwrap();
        let plan = plan(&store).await;
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
