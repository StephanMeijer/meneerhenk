//! SQLite: one connection behind a mutex. Writes take microseconds, so the
//! async methods run them in place.

use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;
use henk_domain::run::{EventId, RunId};
use rusqlite::{Connection, OptionalExtension as _, params};
use rusqlite_migration::{M, Migrations};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::store::RunStore;
use crate::types::{
    DraftDecision, DraftFilter, DraftGroup, DraftListing, DraftRates, DraftRecord, EventFilter,
    EventRecord, EventWithOutcomes, FindingAction, FindingRecord, InboundEvent, LaneRecord,
    LaneStatus, MAX_PAYLOAD_BYTES, NewRun, OutcomeRecord, OutcomeRow, Page, PruneCounts, RawRun,
    RunFilter, RunRecord, RunStatus, StoreError, ToolCallRecord, ToolUsage, TranscriptRecord,
    TranscriptSummary, VerdictFilter, attach_outcomes, draft_verdict, kind_str, now,
    platform_parse, platform_str, status_str, to_i64, to_u64,
};

/// The run store over SQLite.
#[derive(Debug)]
pub struct SqliteStore {
    connection: Mutex<Connection>,
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

    fn from_connection(mut connection: Connection) -> Result<Self, StoreError> {
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        migrations().to_latest(&mut connection)?;
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
        let cutoff = stale_before
            .format(&Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned());
        self.with(|c| {
            let mut statement = c.prepare(&format!(
                "SELECT {RUN_COLUMNS} FROM runs
                 WHERE status = ?1 AND (heartbeat_at IS NULL OR heartbeat_at < ?2)
                 ORDER BY started_at"
            ))?;
            let raw = statement
                .query_map(params![RunStatus::Running.as_str(), cutoff], raw_run)?
                .collect::<Result<Vec<_>, _>>()?;
            raw.into_iter().map(RawRun::into_record).collect()
        })
    }

    async fn drop_running_lanes(&self, run: &RunId, reason: &str) -> Result<(), StoreError> {
        self.with(|c| {
            c.execute(
                "UPDATE lanes SET status = ?2, finished_at = ?3, error = ?4
                 WHERE run_id = ?1 AND status = ?5",
                params![
                    run.as_str(),
                    LaneStatus::Dropped.as_str(),
                    now(),
                    reason,
                    LaneStatus::Running.as_str()
                ],
            )?;
            Ok(())
        })
    }

    async fn start_lane(&self, run: &RunId, name: &str, model: &str) -> Result<(), StoreError> {
        self.with(|c| {
            c.execute(
                "INSERT INTO lanes (run_id, name, model, status, started_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![run.as_str(), name, model, LaneStatus::Running.as_str(), now()],
            )?;
            Ok(())
        })
    }

    async fn finish_lane(
        &self,
        run: &RunId,
        name: &str,
        status: LaneStatus,
        turns: u64,
        input_tokens: u64,
        output_tokens: u64,
        error: Option<&str>,
    ) -> Result<(), StoreError> {
        self.with(|c| {
            c.execute(
                "UPDATE lanes SET status = ?3, turns = ?4, input_tokens = ?5, output_tokens = ?6, finished_at = ?7, error = ?8
                 WHERE run_id = ?1 AND name = ?2",
                params![
                    run.as_str(),
                    name,
                    status.as_str(),
                    i64::try_from(turns).unwrap_or(i64::MAX),
                    i64::try_from(input_tokens).unwrap_or(i64::MAX),
                    i64::try_from(output_tokens).unwrap_or(i64::MAX),
                    now(),
                    error
                ],
            )?;
            Ok(())
        })
    }

    async fn lanes(&self, run: &RunId) -> Result<Vec<LaneRecord>, StoreError> {
        self.with(|c| {
            let mut statement = c.prepare(
                "SELECT name, model, status, turns, input_tokens, output_tokens, error FROM lanes WHERE run_id = ?1 ORDER BY name",
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
                ))
            })?;
            let mut lanes = Vec::new();
            for row in rows {
                let (name, model, status, turns, input_tokens, output_tokens, error) = row?;
                let status = LaneStatus::parse(&status)
                    .ok_or(StoreError::Corrupt { column: "lanes.status", value: status })?;
                lanes.push(LaneRecord {
                    name,
                    model,
                    status,
                    turns: to_u64("lanes.turns", turns)?,
                    input_tokens: to_u64("lanes.input_tokens", input_tokens)?,
                    output_tokens: to_u64("lanes.output_tokens", output_tokens)?,
                    error,
                });
            }
            Ok(lanes)
        })
    }

    async fn record_finding(
        &self,
        run: &RunId,
        lane: &str,
        path: &str,
        line_number: u32,
        comment_id: &str,
        action: FindingAction,
    ) -> Result<(), StoreError> {
        self.with(|c| {
            c.execute(
                "INSERT INTO findings (run_id, lane, path, line, comment_id, action, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![run.as_str(), lane, path, line_number, comment_id, action.as_str(), now()],
            )?;
            Ok(())
        })
    }

    async fn record_draft(&self, run: &RunId, draft: &DraftRecord) -> Result<(), StoreError> {
        let at = if draft.at.is_empty() {
            now()
        } else {
            draft.at.clone()
        };
        self.with(|c| {
            c.execute(
                "INSERT INTO drafts (run_id, draft, lane, model, kind, path, line, target, body, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) ON CONFLICT (run_id, draft) DO UPDATE SET kind = excluded.kind, target = excluded.target, body = excluded.body",
                params![
                    run.as_str(),
                    draft.draft,
                    draft.lane,
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
        let at = if decision.at.is_empty() {
            now()
        } else {
            decision.at.clone()
        };
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
                "SELECT created_at, draft, lane, model, kind, path, line, target, body, verdict, checker, reason, same_as, comment_id, decided_at FROM drafts WHERE run_id = ?1 ORDER BY id",
            )?;
            let rows = statement.query_map(params![run.as_str()], |row| {
                Ok((
                    DraftRecord {
                        at: row.get(0)?,
                        draft: row.get(1)?,
                        lane: row.get(2)?,
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

    async fn draft_rates(
        &self,
        group: DraftGroup,
        filter: &DraftFilter,
    ) -> Result<Vec<DraftRates>, StoreError> {
        let (since, until) = draft_window(filter)?;
        let (key, columns, by) = match group {
            DraftGroup::Model => ("d.model", "NULL, NULL, NULL", "d.model"),
            DraftGroup::Lane => ("d.lane", "NULL, NULL, NULL", "d.lane"),
            DraftGroup::Repo => ("r.repo", "NULL, NULL, NULL", "r.repo"),
            DraftGroup::Target => (
                "r.repo || ' #' || r.target",
                "r.repo, r.target, r.platform",
                "r.repo, r.target, r.platform",
            ),
        };
        self.with(|c| {
            let mut statement = c.prepare(&format!(
                "SELECT {key}, {columns}, COUNT(*), {VERDICT_SUMS}
                 FROM drafts d JOIN runs r ON r.id = d.run_id
                 WHERE {DRAFT_FILTER}
                 GROUP BY {by} ORDER BY COUNT(*) DESC, 1"
            ))?;
            let rows = statement
                .query_map(
                    params![filter.model, filter.lane, filter.repo, since, until],
                    |row| {
                        let count = |i: usize| -> rusqlite::Result<u64> {
                            let n: i64 = row.get(i)?;
                            Ok(u64::try_from(n).unwrap_or_default())
                        };
                        let target: Option<i64> = row.get(2)?;
                        Ok(DraftRates {
                            key: row.get(0)?,
                            repo: row.get(1)?,
                            target: target.and_then(|t| u64::try_from(t).ok()),
                            platform: row
                                .get::<_, Option<String>>(3)?
                                .as_deref()
                                .and_then(platform_parse),

                            drafts: count(4)?,
                            confirmed: count(5)?,
                            rejected: count(6)?,
                            same_as: count(7)?,
                            unchecked: count(8)?,
                            not_checked: count(9)?,
                            cancelled: count(10)?,
                            failed: count(11)?,
                            waiting: count(12)?,
                        })
                    },
                )?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
    }

    async fn list_drafts(
        &self,
        filter: &DraftFilter,
        page: Page,
    ) -> Result<Vec<DraftListing>, StoreError> {
        let (since, until) = draft_window(filter)?;
        let verdict = VerdictFilter::param(filter.verdict);
        let (before_at, before_id) = filter
            .before
            .as_ref()
            .map(|k| (k.created_at.as_str(), k.id))
            .unzip();
        let rows: Vec<DraftRow> = self.with(|c| {
            let mut statement = c.prepare(&format!(
                "SELECT d.id, d.run_id, r.repo, r.target, d.created_at, d.draft, d.lane, d.model,
                        d.kind, d.path, d.line, d.target, d.body, d.verdict, d.checker, d.reason,
                        d.same_as, d.comment_id, d.decided_at, r.platform
                 FROM drafts d JOIN runs r ON r.id = d.run_id
                 WHERE {DRAFT_FILTER}
                   AND (?6 IS NULL OR (?6 = 'waiting' AND d.verdict IS NULL) OR d.verdict = ?6)
                   AND (?7 IS NULL OR d.created_at < ?7 OR (d.created_at = ?7 AND d.id < ?8))
                 ORDER BY d.created_at DESC, d.id DESC LIMIT ?9 OFFSET ?10"
            ))?;
            let rows = statement.query_map(
                params![
                    filter.model,
                    filter.lane,
                    filter.repo,
                    since,
                    until,
                    verdict,
                    before_at,
                    before_id,
                    page.limit(),
                    page.offset()
                ],
                draft_row,
            )?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        })?;
        rows.into_iter().map(DraftRow::into_listing).collect()
    }

    async fn record_transcript(
        &self,
        run: &RunId,
        transcript: &TranscriptRecord,
    ) -> Result<(), StoreError> {
        let at = if transcript.at.is_empty() {
            now()
        } else {
            transcript.at.clone()
        };
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
        let at = if call.at.is_empty() {
            now()
        } else {
            call.at.clone()
        };
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

    async fn tool_usage_since(&self, since: OffsetDateTime) -> Result<Vec<ToolUsage>, StoreError> {
        let cutoff = since
            .format(&Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned());
        let rows = self.with(|c| {
            let mut statement = c.prepare(
                "SELECT model, session, tool, outcome, COUNT(*), COALESCE(SUM(elapsed_ms), 0) FROM tool_calls WHERE at >= ?1 GROUP BY model, session, tool, outcome",
            )?;
            let number = |n: i64| u64::try_from(n).unwrap_or_default();
            let rows = statement.query_map(params![cutoff], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    number(row.get(4)?),
                    number(row.get(5)?),
                ))
            })?;
            rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::from)
        })?;
        Ok(ToolUsage::across_runs(rows))
    }

    async fn findings(&self, run: &RunId) -> Result<Vec<FindingRecord>, StoreError> {
        self.with(|c| {
            let mut statement = c.prepare(
                "SELECT created_at, lane, path, line, comment_id, action FROM findings WHERE run_id = ?1 ORDER BY id",
            )?;
            let rows = statement.query_map(params![run.as_str()], |row| {
                Ok(FindingRecord {
                    at: row.get(0)?,
                    lane: row.get(1)?,
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
                    if outcome.at.is_empty() { now() } else { outcome.at.clone() },
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
            let id = EventId::parse(id.clone()).map_err(|_| StoreError::Corrupt {
                column: "inbound_events.id",
                value: id,
            })?;
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
        let status = filter.status.map(status_str);
        let platform = filter.platform.map(platform_str);
        let target = filter.target.map(|t| i64::try_from(t).unwrap_or(i64::MAX));
        let (before_at, before_id) = run_key(filter);
        let (since, until) = run_window(filter)?;
        self.with(|c| {
            let mut statement = c.prepare(&format!(
                "SELECT {RUN_COLUMNS} FROM runs WHERE {RUN_FILTER}
                 ORDER BY started_at DESC, id DESC LIMIT ?10 OFFSET ?11"
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

    async fn count_runs(&self, filter: &RunFilter) -> Result<u64, StoreError> {
        let kind = filter.kind.map(kind_str);
        let status = filter.status.map(status_str);
        let platform = filter.platform.map(platform_str);
        let target = filter.target.map(|t| i64::try_from(t).unwrap_or(i64::MAX));
        let (before_at, before_id) = run_key(filter);
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
        let id = EventId::parse(self.id.clone()).map_err(|_| StoreError::Corrupt {
            column: "inbound_events.id",
            value: self.id,
        })?;
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
/// `since` and `until` are [`instant`]s, compared to `started_at` as one
/// too: as text, "12:00:00.4Z" sorts before "12:00:00Z".
const RUN_FILTER: &str = "(?1 IS NULL OR kind = ?1) AND (?2 IS NULL OR status = ?2)
     AND (?3 IS NULL OR platform = ?3) AND (?4 IS NULL OR repo = ?4)
     AND (?5 IS NULL OR target = ?5)
     AND (?6 IS NULL OR substr(started_at, 1, 19) || substr(CASE WHEN substr(started_at, 20, 1) = '.'
            THEN substr(started_at, 21, length(started_at) - 21) ELSE '' END || '000000000', 1, 9) >= ?6)
     AND (?7 IS NULL OR substr(started_at, 1, 19) || substr(CASE WHEN substr(started_at, 20, 1) = '.'
            THEN substr(started_at, 21, length(started_at) - 21) ELSE '' END || '000000000', 1, 9) < ?7)
     AND (?8 IS NULL OR started_at < ?8 OR (started_at = ?8 AND id < ?9))";

/// A row of a draft listing as read, before its texts are checked.
struct DraftRow {
    id: i64,
    run_id: String,
    repo: String,
    target: i64,
    platform: String,
    draft: DraftRecord,
    verdict: Option<String>,
    decision: [String; 4],
    decided: Option<String>,
}

/// Reads the columns `list_drafts` selects, in order.
fn draft_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DraftRow> {
    Ok(DraftRow {
        id: row.get(0)?,
        run_id: row.get(1)?,
        repo: row.get(2)?,
        target: row.get(3)?,
        draft: DraftRecord {
            at: row.get(4)?,
            draft: row.get(5)?,
            lane: row.get(6)?,
            model: row.get(7)?,
            kind: row.get(8)?,
            path: row.get(9)?,
            line: row.get(10)?,
            target: row.get(11)?,
            body: row.get(12)?,
            decision: None,
        },
        verdict: row.get(13)?,
        decision: [row.get(14)?, row.get(15)?, row.get(16)?, row.get(17)?],
        decided: row.get(18)?,
        platform: row.get(19)?,
    })
}

impl DraftRow {
    fn into_listing(self) -> Result<DraftListing, StoreError> {
        let Self {
            id,
            run_id,
            repo,
            target,
            platform,
            mut draft,
            verdict,
            decision: [checker, reason, same_as, comment_id],
            decided,
        } = self;
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
        Ok(DraftListing {
            id,
            run_id,
            repo,
            target: to_u64("runs.target", target)?,
            platform: platform_parse(&platform).ok_or_else(|| StoreError::Corrupt {
                column: "runs.platform",
                value: platform.clone(),
            })?,
            draft,
        })
    }
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

/// The `WHERE` of a draft count or listing over `drafts d JOIN runs r`,
/// over parameters 1 to 5: model, lane, repo, since and until, the last
/// two as [`instant`]s.
const DRAFT_FILTER: &str = concat!(
    "(?1 IS NULL OR d.model = ?1) AND (?2 IS NULL OR d.lane = ?2) AND (?3 IS NULL OR r.repo = ?3)",
    " AND (?4 IS NULL OR ",
    instant_of!("d.created_at"),
    " >= ?4) AND (?5 IS NULL OR ",
    instant_of!("d.created_at"),
    " < ?5)"
);

/// One count per verdict, and the waiting ones, in [`DraftRates`] order.
const VERDICT_SUMS: &str = "SUM(CASE WHEN d.verdict = 'confirmed' THEN 1 ELSE 0 END),
     SUM(CASE WHEN d.verdict = 'rejected' THEN 1 ELSE 0 END),
     SUM(CASE WHEN d.verdict = 'same_as' THEN 1 ELSE 0 END),
     SUM(CASE WHEN d.verdict = 'unchecked' THEN 1 ELSE 0 END),
     SUM(CASE WHEN d.verdict = 'not_checked' THEN 1 ELSE 0 END),
     SUM(CASE WHEN d.verdict = 'cancelled' THEN 1 ELSE 0 END),
     SUM(CASE WHEN d.verdict = 'failed' THEN 1 ELSE 0 END),
     SUM(CASE WHEN d.verdict IS NULL THEN 1 ELSE 0 END)";

/// The time bounds of a draft count or listing, as [`instant`]s.
fn draft_window(filter: &DraftFilter) -> Result<(Option<String>, Option<String>), StoreError> {
    let since = filter
        .since
        .as_deref()
        .map(|v| instant("since", v))
        .transpose()?;
    let until = filter
        .until
        .as_deref()
        .map(|v| instant("until", v))
        .transpose()?;
    Ok((since, until))
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
    let since = filter
        .since
        .as_deref()
        .map(|v| instant("since", v))
        .transpose()?;
    let until = filter
        .until
        .as_deref()
        .map(|v| instant("until", v))
        .transpose()?;
    Ok((since, until))
}

/// The keyset of a run listing, as two parameters.
fn run_key(filter: &RunFilter) -> (Option<&str>, Option<&str>) {
    filter
        .before
        .as_ref()
        .map(|k| (k.started_at.as_str(), k.id.as_str()))
        .unzip()
}

/// The columns [`raw_run`] reads, in order.
const RUN_COLUMNS: &str = "id, kind, platform, repo, target, commit_sha, requester, trigger, status, started_at, finished_at, link, summary, error, heartbeat_at, check_id";

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
