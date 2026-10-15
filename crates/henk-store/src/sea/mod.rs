//! The run store on `SeaORM` (spike #601): one implementation for SQLite and
//! `PostgreSQL`. The query builder (`sea-query`, re-exported by sea-orm)
//! makes the SQL; `DatabaseConnection` picks the dialect, the placeholders
//! and the pool. No entities: the tables are named by strings and rows are
//! read by position, like the two stores this replaces.
//!
//! What the builder cannot say is written as raw SQL, and each such place is
//! marked `RAW` below.

mod migrator;

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use henk_domain::run::{EventId, RunId, RunKind};
use sea_orm::sea_query::{
    Asterisk, Condition, Expr, ExprTrait, Func, JoinType, OnConflict, Order, Query,
    SelectStatement, Value,
};
use sea_orm::{
    ConnectOptions, ConnectionTrait, Database, DatabaseConnection, DbBackend, DbErr, QueryResult,
    StatementBuilder, TryGetable,
};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::store::RunStore;
use crate::types::{
    DayCounts, DraftDecision, DraftRecord, EventFacets, EventFilter, EventRecord,
    EventWithOutcomes, FindingAction, FindingRecord, InboundEvent, MOST_SESSION_REVIEWS, NewRun,
    OutcomeFilter, OutcomeRecord, OutcomeRow, Page, PruneCounts, RawRun, RoundRecord, RunFilter,
    RunRecord, RunStatus, SessionEnding, SessionRecord, SessionRole, SessionStatus, Stage,
    StageRecord, StageState, StageWrite, StoreError, TokenUsage, ToolCallFilter, ToolCallListing,
    ToolCallRecord, ToolUsage, TranscriptRecord, TranscriptSummary, attach_outcomes, child_records,
    draft_verdict, event_id, kind_str, merge_days, now, platform_str, round_record, run_kind,
    run_platform, session_dots, session_endings, session_role, session_status, stage_dots,
    stage_records, to_i64, to_u32, to_u64,
};

/// Connections the pool keeps at most. Henk's writes are small and few.
const POOL_SIZE: u32 = 8;

/// How long a statement waits for SQLite's write lock before it fails.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The run store over `SeaORM`, on SQLite or `PostgreSQL`.
#[derive(Debug)]
pub struct Candidate {
    db: DatabaseConnection,
    backend: DbBackend,
}

impl Candidate {
    /// Opens or creates the SQLite database at `path` and migrates it.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the file cannot be opened or migrated.
    pub async fn open(path: &Path) -> Result<Self, StoreError> {
        Self::connect(&format!("sqlite://{}?mode=rwc", path.display())).await
    }

    /// Connects to the database at `url` (`sqlite://...` or `postgres://...`)
    /// and brings its schema up to date. `sslmode` in a `PostgreSQL` URL
    /// decides TLS, as usual.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the URL does not parse, the server cannot
    /// be reached, or migrating fails.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        let mut options = ConnectOptions::new(url);
        options
            .max_connections(POOL_SIZE)
            .connect_timeout(Duration::from_secs(10))
            .sqlx_logging(false)
            // Every SQLite connection of the pool: WAL, foreign keys, and a
            // busy timeout so concurrent writers wait instead of failing.
            .map_sqlx_sqlite_opts(|sqlite| {
                sqlite
                    .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
                    .foreign_keys(true)
                    .busy_timeout(BUSY_TIMEOUT)
            });
        let db = Database::connect(options).await.map_err(StoreError::from)?;
        let backend = db.get_database_backend();
        migrator::migrate(&db)
            .await
            .map_err(|error| StoreError::Migration(error.to_string()))?;
        Ok(Self { db, backend })
    }

    fn sqlite(&self) -> bool {
        self.backend == DbBackend::Sqlite
    }

    // --- running statements --------------------------------------------------

    async fn all(&self, query: &SelectStatement) -> Result<Vec<QueryResult>, StoreError> {
        self.db.query_all(query).await.map_err(StoreError::from)
    }

    async fn first(&self, query: &SelectStatement) -> Result<Option<QueryResult>, StoreError> {
        self.db.query_one(query).await.map_err(StoreError::from)
    }

    async fn exec<S: StatementBuilder>(&self, statement: &S) -> Result<u64, StoreError> {
        Ok(self
            .db
            .execute(statement)
            .await
            .map_err(StoreError::from)?
            .rows_affected())
    }

    // --- time, the one thing that differs between the databases --------------

    /// A stored time column as an expression that compares as an instant:
    /// `TIMESTAMPTZ` already does; SQLite's RFC 3339 text is turned into a
    /// fixed-width UTC text first (#216, `instant_of!` of the old store).
    /// RAW: SQLite's `substr` expression.
    fn instant(&self, column: &str) -> Expr {
        if self.sqlite() {
            let c = column;
            Expr::cust(format!(
                "substr({c}, 1, 19) || substr(CASE WHEN substr({c}, 20, 1) = '.' THEN substr({c}, 21, length({c}) - 21) ELSE '' END || '000000000', 1, 9)"
            ))
        } else {
            Expr::cust(column.to_owned())
        }
    }

    /// A bound for [`Self::instant`].
    fn instant_value(&self, column: &'static str, value: &str) -> Result<Value, StoreError> {
        let at = parse_time(column, value)?;
        Ok(if self.sqlite() {
            format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{:09}",
                at.year(),
                u8::from(at.month()),
                at.day(),
                at.hour(),
                at.minute(),
                at.second(),
                at.nanosecond()
            )
            .into()
        } else {
            at.into()
        })
    }

    /// A time to store, or to compare with a column as it is stored: the
    /// text itself on SQLite, a timestamp on `PostgreSQL`. Empty means now.
    fn time_value(&self, column: &'static str, value: &str) -> Result<Value, StoreError> {
        if self.sqlite() {
            Ok(if value.is_empty() {
                now()
            } else {
                value.to_owned()
            }
            .into())
        } else {
            Ok(parse_time(column, value)?.into())
        }
    }

    // --- shared query pieces -------------------------------------------------

    /// The `WHERE` of a run listing. Only the parts the filter sets are
    /// added, so there is no `(? IS NULL OR ...)`.
    fn run_conditions(&self, filter: &RunFilter) -> Result<Condition, StoreError> {
        let mut all = Condition::all();
        if let Some(kind) = filter.kind {
            all = all.add(Expr::col("kind").eq(kind_str(kind)));
        }
        if let Some(status) = filter.status {
            all = all.add(Expr::col("status").eq(status.as_str()));
        }
        if let Some(platform) = filter.platform {
            all = all.add(Expr::col("platform").eq(platform_str(platform)));
        }
        if let Some(repo) = &filter.repo {
            all = all.add(Expr::col("repo").eq(repo.as_str()));
        }
        if let Some(target) = filter.target {
            all = all.add(Expr::col("target").eq(to_i64("runs.target", target)?));
        }
        if let Some(since) = &filter.since {
            all = all.add(
                self.instant("started_at")
                    .gte(self.instant_value("since", since)?),
            );
        }
        if let Some(until) = &filter.until {
            all = all.add(
                self.instant("started_at")
                    .lt(self.instant_value("until", until)?),
            );
        }
        if let Some(key) = &filter.before {
            let at = self.instant_value("before", &key.started_at)?;
            all = all.add(
                Condition::any()
                    .add(self.instant("started_at").lt(at.clone()))
                    .add(
                        Condition::all()
                            .add(self.instant("started_at").eq(at))
                            .add(Expr::col("id").lt(key.id.as_str())),
                    ),
            );
        }
        Ok(all)
    }

    /// The `WHERE` of a tool call tally or listing, over `tool_calls c` and
    /// `sessions s`.
    fn call_conditions(&self, filter: &ToolCallFilter) -> Result<Condition, StoreError> {
        let mut all = Condition::all();
        if let Some(tool) = &filter.tool {
            all = all.add(call_tool().eq(tool.as_str()));
        }
        if let Some(model) = &filter.model {
            all = all.add(Expr::col(("c", "model")).eq(model.as_str()));
        }
        if let Some(role) = filter.role {
            all = all.add(call_role().eq(role.as_str()));
        }
        if let Some(since) = &filter.since {
            all = all.add(
                self.instant("c.at")
                    .gte(self.instant_value("since", since)?),
            );
        }
        if let Some(until) = &filter.until {
            all = all.add(self.instant("c.at").lt(self.instant_value("until", until)?));
        }
        Ok(all)
    }

    /// `tool_calls c LEFT JOIN sessions s`: the role of the session that made a call.
    fn calls(query: &mut SelectStatement) {
        query.from_as("tool_calls", "c").join_as(
            JoinType::LeftJoin,
            "sessions",
            "s",
            Condition::all()
                .add(Expr::col(("s", "run_id")).equals(("c", "run_id")))
                .add(Expr::col(("s", "name")).equals(("c", "session"))),
        );
    }

    /// The order an outcome list is read in: recording order. RAW: SQLite's
    /// outcomes have no key column, only the implicit `rowid`.
    fn outcome_order(&self) -> Expr {
        Expr::cust(if self.sqlite() { "rowid" } else { "id" })
    }

    fn inbound_select() -> SelectStatement {
        let mut query = Query::select();
        query
            .columns([
                "id",
                "received_at",
                "source",
                "kind",
                "repo",
                "target",
                "payload",
                "requester",
            ])
            .from("inbound_events");
        query
    }

    // --- the ported methods (inherent, plain async fn) -----------------------

    async fn run(&self, id: &RunId) -> Result<Option<RunRecord>, StoreError> {
        let mut query = run_select();
        query
            .from("runs")
            .and_where(Expr::col("id").eq(id.as_str()));
        self.first(&query)
            .await?
            .map(|row| raw_run(&row)?.into_record())
            .transpose()
    }

    /// The query of a run listing: newest first as an instant, then by id.
    fn list_runs_query(
        &self,
        filter: &RunFilter,
        page: Page,
    ) -> Result<SelectStatement, StoreError> {
        let mut query = run_select();
        query
            .from("runs")
            .cond_where(self.run_conditions(filter)?)
            .order_by_expr(self.instant("started_at"), Order::Desc)
            .order_by("id", Order::Desc)
            .limit(page.limit().into())
            .offset(page.offset().into());
        Ok(query)
    }

    async fn list_runs(
        &self,
        filter: &RunFilter,
        page: Page,
    ) -> Result<Vec<RunRecord>, StoreError> {
        let query = self.list_runs_query(filter, page)?;
        self.all(&query)
            .await?
            .iter()
            .map(|row| raw_run(row)?.into_record())
            .collect()
    }

    async fn count_runs(&self, filter: &RunFilter) -> Result<u64, StoreError> {
        let mut query = Query::select();
        query
            .expr(Func::count(Expr::col(Asterisk)))
            .from("runs")
            .cond_where(self.run_conditions(filter)?);
        let row = self.first(&query).await?;
        let count: i64 = row.map_or(Ok(0), |row| get(&row, 0))?;
        Ok(u64::try_from(count).unwrap_or_default())
    }

    async fn orphaned_runs(
        &self,
        stale_before: OffsetDateTime,
    ) -> Result<Vec<RunRecord>, StoreError> {
        let cutoff = self.instant_value("stale_before", &rfc3339(stale_before))?;
        let mut query = run_select();
        query
            .from("runs")
            .and_where(Expr::col("status").eq(RunStatus::Running.as_str()))
            .cond_where(
                Condition::any()
                    .add(Expr::col("heartbeat_at").is_null())
                    .add(self.instant("heartbeat_at").lt(cutoff)),
            )
            .order_by_expr(self.instant("started_at"), Order::Asc)
            .order_by("id", Order::Asc);
        self.all(&query)
            .await?
            .iter()
            .map(|row| raw_run(row)?.into_record())
            .collect()
    }

    /// The newest review of each pull request that started at or after
    /// `since`, as instants.
    async fn latest_reviews(&self, since: OffsetDateTime) -> Result<Vec<RunRecord>, StoreError> {
        let since = self.instant_value("since", &rfc3339(since))?;
        let newer = Query::select()
            .expr(Expr::val(1))
            .from_as("runs", "n")
            .and_where(Expr::col(("n", "kind")).equals(("r", "kind")))
            .and_where(Expr::col(("n", "platform")).equals(("r", "platform")))
            .and_where(Expr::col(("n", "repo")).equals(("r", "repo")))
            .and_where(Expr::col(("n", "target")).equals(("r", "target")))
            .cond_where(
                Condition::any()
                    .add(
                        self.instant("n.started_at")
                            .gt(self.instant("r.started_at")),
                    )
                    .add(
                        Condition::all()
                            .add(
                                self.instant("n.started_at")
                                    .eq(self.instant("r.started_at")),
                            )
                            .add(Expr::col(("n", "id")).gt(Expr::col(("r", "id")))),
                    ),
            )
            .take();
        let mut query = run_select();
        query
            .from_as("runs", "r")
            .and_where(Expr::col(("r", "kind")).eq(kind_str(RunKind::Review)))
            .and_where(self.instant("r.started_at").gte(since))
            .and_where(Expr::not_exists(newer))
            .order_by_expr(self.instant("r.started_at"), Order::Asc)
            .order_by(("r", "id"), Order::Asc);
        self.all(&query)
            .await?
            .iter()
            .map(|row| raw_run(row)?.into_record())
            .collect()
    }

    async fn sessions(&self, run: &RunId) -> Result<Vec<SessionRecord>, StoreError> {
        let mut query = Query::select();
        query
            .columns([
                "name",
                "model",
                "status",
                "turns",
                "input_tokens",
                "output_tokens",
                "error",
                "started_at",
                "finished_at",
                "cache_read_tokens",
                "cache_write_tokens",
                "role",
                "reasoning_tokens",
            ])
            .from("sessions")
            .and_where(Expr::col("run_id").eq(run.as_str()))
            .order_by("name", Order::Asc);
        self.all(&query)
            .await?
            .iter()
            .map(|row| {
                Ok(SessionRecord {
                    name: get(row, 0)?,
                    role: session_role(get(row, 11)?)?,
                    model: get(row, 1)?,
                    status: session_status(get(row, 2)?)?,
                    turns: to_u64("sessions.turns", get(row, 3)?)?,
                    input_tokens: to_u64("sessions.input_tokens", get(row, 4)?)?,
                    output_tokens: to_u64("sessions.output_tokens", get(row, 5)?)?,
                    cache_read_tokens: to_u64("sessions.cache_read_tokens", get(row, 9)?)?,
                    cache_write_tokens: to_u64("sessions.cache_write_tokens", get(row, 10)?)?,
                    reasoning_tokens: to_u64("sessions.reasoning_tokens", get(row, 12)?)?,
                    error: get(row, 6)?,
                    started_at: at(row, 7)?,
                    finished_at: at_opt(row, 8)?,
                })
            })
            .collect()
    }

    async fn sessions_of(
        &self,
        runs: &[RunId],
    ) -> Result<Vec<(RunId, String, SessionRole, SessionStatus)>, StoreError> {
        if runs.is_empty() {
            return Ok(Vec::new());
        }
        let mut query = Query::select();
        query
            .columns(["run_id", "name", "role", "status"])
            .from("sessions")
            .and_where(Expr::col("run_id").is_in(runs.iter().map(RunId::as_str)))
            .order_by("run_id", Order::Asc)
            .order_by("name", Order::Asc);
        let rows = self
            .all(&query)
            .await?
            .iter()
            .map(|row| Ok((get(row, 0)?, get(row, 1)?, get(row, 2)?, get(row, 3)?)))
            .collect::<Result<Vec<_>, StoreError>>()?;
        session_dots(rows)
    }

    async fn stages(&self, run: &RunId) -> Result<Vec<StageRecord>, StoreError> {
        let mut query = Query::select();
        query
            .columns(["stage", "state", "started_at", "ended_at", "detail"])
            .from("stages")
            .and_where(Expr::col("run_id").eq(run.as_str()));
        let rows = self
            .all(&query)
            .await?
            .iter()
            .map(|row| {
                Ok((
                    get(row, 0)?,
                    get(row, 1)?,
                    at(row, 2)?,
                    at_opt(row, 3)?,
                    get(row, 4)?,
                ))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        stage_records(rows)
    }

    async fn stages_of(
        &self,
        runs: &[RunId],
    ) -> Result<Vec<(RunId, Stage, StageState)>, StoreError> {
        if runs.is_empty() {
            return Ok(Vec::new());
        }
        let mut query = Query::select();
        query
            .columns(["run_id", "stage", "state"])
            .from("stages")
            .and_where(Expr::col("run_id").is_in(runs.iter().map(RunId::as_str)));
        let rows = self
            .all(&query)
            .await?
            .iter()
            .map(|row| Ok((get(row, 0)?, get(row, 1)?, get(row, 2)?)))
            .collect::<Result<Vec<_>, StoreError>>()?;
        stage_dots(rows)
    }

    async fn children(&self, parent: &RunId) -> Result<Vec<(Stage, RunId)>, StoreError> {
        let mut query = Query::select();
        query
            .columns(["stage", "child"])
            .from("run_children")
            .and_where(Expr::col("parent").eq(parent.as_str()));
        let rows = self
            .all(&query)
            .await?
            .iter()
            .map(|row| Ok((get(row, 0)?, get(row, 1)?)))
            .collect::<Result<Vec<_>, StoreError>>()?;
        child_records(rows)
    }

    async fn rounds(&self, run: &RunId) -> Result<Vec<RoundRecord>, StoreError> {
        let mut query = Query::select();
        query
            .columns(["session", "role", "round"])
            .column("started_at")
            .column("ended_at")
            .columns(["first_turn", "turns", "handover", "handover_len"])
            .from("session_rounds")
            .and_where(Expr::col("run_id").eq(run.as_str()))
            .order_by_expr(self.instant("started_at"), Order::Asc)
            .order_by("session", Order::Asc)
            .order_by("round", Order::Asc);
        self.all(&query)
            .await?
            .iter()
            .map(|row| {
                round_record((
                    get(row, 0)?,
                    get(row, 1)?,
                    get(row, 2)?,
                    at(row, 3)?,
                    at_opt(row, 4)?,
                    get(row, 5)?,
                    get(row, 6)?,
                    get(row, 7)?,
                    get(row, 8)?,
                ))
            })
            .collect()
    }

    async fn findings(&self, run: &RunId) -> Result<Vec<FindingRecord>, StoreError> {
        let mut query = Query::select();
        query
            .columns([
                "created_at",
                "session",
                "path",
                "line",
                "comment_id",
                "action",
            ])
            .from("findings")
            .and_where(Expr::col("run_id").eq(run.as_str()))
            .order_by("id", Order::Asc);
        self.all(&query)
            .await?
            .iter()
            .map(|row| {
                Ok(FindingRecord {
                    at: at(row, 0)?,
                    session: get(row, 1)?,
                    path: get(row, 2)?,
                    line: to_u32("findings.line", get(row, 3)?)?,
                    comment_id: get(row, 4)?,
                    action: get(row, 5)?,
                })
            })
            .collect()
    }

    async fn drafts(&self, run: &RunId) -> Result<Vec<DraftRecord>, StoreError> {
        let mut query = Query::select();
        query
            .columns([
                "created_at",
                "draft",
                "session",
                "model",
                "kind",
                "path",
                "line",
                "target",
                "body",
                "verdict",
                "checker",
                "reason",
                "same_as",
                "comment_id",
                "decided_at",
            ])
            .from("drafts")
            .and_where(Expr::col("run_id").eq(run.as_str()))
            .order_by("id", Order::Asc);
        self.all(&query)
            .await?
            .iter()
            .map(|row| {
                let verdict: Option<String> = get(row, 9)?;
                let decision = match verdict {
                    Some(verdict) => Some(DraftDecision {
                        at: at_opt(row, 14)?.unwrap_or_default(),
                        verdict: draft_verdict(&verdict)?,
                        checker: get(row, 10)?,
                        reason: get(row, 11)?,
                        same_as: get(row, 12)?,
                        comment_id: get(row, 13)?,
                    }),
                    None => None,
                };
                Ok(DraftRecord {
                    at: at(row, 0)?,
                    draft: get(row, 1)?,
                    session: get(row, 2)?,
                    model: get(row, 3)?,
                    kind: get(row, 4)?,
                    path: get(row, 5)?,
                    line: to_u32("drafts.line", get(row, 6)?)?,
                    target: get(row, 7)?,
                    body: get(row, 8)?,
                    decision,
                })
            })
            .collect()
    }

    async fn events(&self, run: &RunId) -> Result<Vec<EventRecord>, StoreError> {
        let mut query = Query::select();
        query
            .columns(["at", "level", "message"])
            .from("events")
            .and_where(Expr::col("run_id").eq(run.as_str()))
            .order_by("id", Order::Asc);
        self.all(&query)
            .await?
            .iter()
            .map(|row| {
                Ok(EventRecord {
                    at: at(row, 0)?,
                    level: get(row, 1)?,
                    message: get(row, 2)?,
                })
            })
            .collect()
    }

    async fn tool_calls(&self, run: &RunId) -> Result<Vec<ToolCallRecord>, StoreError> {
        let mut query = Query::select();
        query
            .columns([
                "at",
                "session",
                "model",
                "turn",
                "tool",
                "origin",
                "outcome",
                "arguments",
                "arguments_len",
                "result_chars",
                "elapsed_ms",
            ])
            .from("tool_calls")
            .and_where(Expr::col("run_id").eq(run.as_str()))
            .order_by("id", Order::Asc);
        self.all(&query)
            .await?
            .iter()
            .map(|row| {
                Ok(ToolCallRecord {
                    at: at(row, 0)?,
                    session: get(row, 1)?,
                    model: get(row, 2)?,
                    turn: to_u32("tool_calls.turn", get(row, 3)?)?,
                    tool: get(row, 4)?,
                    origin: get(row, 5)?,
                    outcome: get(row, 6)?,
                    arguments: get(row, 7)?,
                    arguments_len: count(row, 8)?,
                    result_chars: count(row, 9)?,
                    elapsed_ms: count(row, 10)?,
                })
            })
            .collect()
    }

    async fn transcripts(&self, run: &RunId) -> Result<Vec<TranscriptSummary>, StoreError> {
        let mut query = Query::select();
        query
            .columns(["recorded_at", "session", "model", "stop", "turns", "bytes"])
            .from("transcripts")
            .and_where(Expr::col("run_id").eq(run.as_str()))
            .order_by("id", Order::Asc);
        self.all(&query)
            .await?
            .iter()
            .map(|row| {
                Ok(TranscriptSummary {
                    at: at(row, 0)?,
                    session: get(row, 1)?,
                    model: get(row, 2)?,
                    stop: get(row, 3)?,
                    turns: to_u32("transcripts.turns", get(row, 4)?)?,
                    bytes: count(row, 5)?,
                })
            })
            .collect()
    }

    async fn transcript(
        &self,
        run: &RunId,
        session: &str,
    ) -> Result<Option<TranscriptRecord>, StoreError> {
        let mut query = Query::select();
        query
            .columns([
                "recorded_at",
                "session",
                "model",
                "stop",
                "turns",
                "bytes",
                "body",
            ])
            .from("transcripts")
            .and_where(Expr::col("run_id").eq(run.as_str()))
            .and_where(Expr::col("session").eq(session))
            .order_by("id", Order::Desc)
            .limit(1);
        self.first(&query)
            .await?
            .map(|row| {
                Ok(TranscriptRecord {
                    at: at(&row, 0)?,
                    session: get(&row, 1)?,
                    model: get(&row, 2)?,
                    stop: get(&row, 3)?,
                    turns: to_u32("transcripts.turns", get(&row, 4)?)?,
                    bytes: count(&row, 5)?,
                    body: get(&row, 6)?,
                })
            })
            .transpose()
    }

    async fn event_facets(&self) -> Result<EventFacets, StoreError> {
        let mut facets = EventFacets::default();
        for (column, values) in [("source", &mut facets.sources), ("kind", &mut facets.kinds)] {
            let mut query = Query::select();
            query
                .distinct()
                .column(column)
                .from("inbound_events")
                .order_by(column, Order::Asc);
            *values = self
                .all(&query)
                .await?
                .iter()
                .map(|row| get(row, 0))
                .collect::<Result<_, _>>()?;
        }
        Ok(facets)
    }

    async fn inbound_event(&self, id: &EventId) -> Result<Option<InboundEvent>, StoreError> {
        let mut query = Self::inbound_select();
        query.and_where(Expr::col("id").eq(id.as_str()));
        self.first(&query)
            .await?
            .map(|row| inbound_event(&row))
            .transpose()
    }

    async fn outcomes(&self, id: &EventId) -> Result<Vec<OutcomeRecord>, StoreError> {
        let mut query = Query::select();
        query
            .columns(["listener", "outcome", "detail", "run_id"])
            .column("at")
            .from("event_outcomes")
            .and_where(Expr::col("event_id").eq(id.as_str()))
            .order_by_expr(self.outcome_order(), Order::Asc);
        self.all(&query)
            .await?
            .iter()
            .map(|row| {
                Ok(OutcomeRecord {
                    event_id: id.clone(),
                    listener: get(row, 0)?,
                    outcome: get(row, 1)?,
                    detail: get(row, 2)?,
                    run_id: get(row, 3)?,
                    at: at(row, 4)?,
                })
            })
            .collect()
    }

    async fn inbound_events_for_run(&self, run: &RunId) -> Result<Vec<InboundEvent>, StoreError> {
        let mut outcomes = Query::select();
        outcomes
            .column("event_id")
            .from("event_outcomes")
            .and_where(Expr::col("run_id").eq(run.as_str()));
        let mut query = Self::inbound_select();
        query
            .and_where(Expr::col("id").in_subquery(outcomes))
            .order_by("received_at", Order::Asc)
            .order_by("id", Order::Asc);
        self.all(&query).await?.iter().map(inbound_event).collect()
    }

    async fn list_inbound_events(
        &self,
        filter: &EventFilter,
        page: Page,
    ) -> Result<Vec<EventWithOutcomes>, StoreError> {
        let mut all = Condition::all();
        if let Some(source) = &filter.source {
            all = all.add(Expr::col("source").eq(source.as_str()));
        }
        if let Some(kind) = &filter.kind {
            all = all.add(Expr::col("kind").eq(kind.as_str()));
        }
        if let Some(repo) = &filter.repo {
            all = all.add(Expr::col("repo").eq(repo.as_str()));
        }
        if let Some(key) = &filter.before {
            let at = self.time_value("received_at", &key.received_at)?;
            all = all.add(
                Condition::any()
                    .add(Expr::col("received_at").lt(at.clone()))
                    .add(
                        Condition::all()
                            .add(Expr::col("received_at").eq(at))
                            .add(Expr::col("id").lt(key.id.as_str())),
                    ),
            );
        }
        let mut query = Self::inbound_select();
        query
            .cond_where(all)
            .order_by("received_at", Order::Desc)
            .order_by("id", Order::Desc)
            .limit(page.limit().into())
            .offset(page.offset().into());
        let events = self
            .all(&query)
            .await?
            .iter()
            .map(inbound_event)
            .collect::<Result<Vec<_>, _>>()?;
        if events.is_empty() {
            return Ok(Vec::new());
        }
        // One query for every outcome of the page, not one per event.
        let mut query = Query::select();
        query
            .columns(["event_id", "listener", "outcome", "detail", "run_id"])
            .column("at")
            .from("event_outcomes")
            .and_where(Expr::col("event_id").is_in(events.iter().map(|e| e.id.as_str())))
            .order_by_expr(self.outcome_order(), Order::Asc);
        let outcomes = self
            .all(&query)
            .await?
            .iter()
            .map(|row| {
                Ok(OutcomeRow {
                    event_id: get(row, 0)?,
                    listener: get(row, 1)?,
                    outcome: get(row, 2)?,
                    detail: get(row, 3)?,
                    run_id: get(row, 4)?,
                    at: at(row, 5)?,
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        Ok(attach_outcomes(events, &outcomes))
    }

    async fn daily_stats(&self, since: OffsetDateTime) -> Result<Vec<DayCounts>, StoreError> {
        let since = self.instant_value("since", &rfc3339(since))?;
        // RAW: the day of a stored time, per database.
        let day = |column: &str| {
            Expr::cust(if self.sqlite() {
                format!("substr({column}, 1, 10)")
            } else {
                format!("to_char({column} AT TIME ZONE 'UTC', 'YYYY-MM-DD')")
            })
        };
        let status_is = |status: &'static str| {
            Expr::case(Expr::col("status").eq(status), 1)
                .finally(0)
                .sum()
        };
        let mut runs = Query::select();
        runs.expr(day("started_at"))
            .expr(Func::count(Expr::col(Asterisk)))
            .expr(status_is("finished"))
            .expr(status_is("failed"))
            .from("runs")
            .and_where(self.instant("started_at").gte(since.clone()))
            .add_group_by([Expr::cust("1")]);
        let runs = self
            .all(&runs)
            .await?
            .iter()
            .map(|row| Ok((get(row, 0)?, get(row, 1)?, get(row, 2)?, get(row, 3)?)))
            .collect::<Result<Vec<_>, StoreError>>()?;

        let mut findings = Query::select();
        findings
            .expr(day("created_at"))
            .expr(Func::count(Expr::col(Asterisk)))
            .from("findings")
            .and_where(Expr::col("action").is_in(["posted", "unverified"]))
            .and_where(self.instant("created_at").gte(since.clone()))
            .add_group_by([Expr::cust("1")]);
        let findings = self
            .all(&findings)
            .await?
            .iter()
            .map(|row| Ok((get(row, 0)?, get(row, 1)?)))
            .collect::<Result<Vec<_>, StoreError>>()?;

        // A review loop's findings are left out of the drafts (#285).
        let mut drafts = Query::select();
        drafts
            .expr(day("created_at"))
            .expr(Func::count(Expr::col(Asterisk)))
            .from("drafts")
            .and_where(self.instant("created_at").gte(since))
            .and_where(Expr::col("kind").is_not_in([
                "loop_finding",
                "plan_point",
                "requirement_point",
            ]))
            .add_group_by([Expr::cust("1")]);
        let drafts = self
            .all(&drafts)
            .await?
            .iter()
            .map(|row| Ok((get(row, 0)?, get(row, 1)?)))
            .collect::<Result<Vec<_>, StoreError>>()?;
        Ok(merge_days(runs, findings, drafts))
    }

    async fn session_endings(
        &self,
        since: Option<OffsetDateTime>,
        reviews: u32,
    ) -> Result<Vec<SessionEnding>, StoreError> {
        let reviews = u64::from(Ord::min(reviews, MOST_SESSION_REVIEWS));
        // The newest `reviews` reviews that ended, ordered as instants.
        let mut newest = Query::select();
        newest
            .columns(["id", "started_at"])
            .expr_as(self.instant("started_at"), "at")
            .from("runs")
            .and_where(Expr::col("kind").eq(kind_str(RunKind::Review)))
            .and_where(Expr::col("status").is_not_in(["running", "superseded", "cancelled"]))
            .order_by_expr(self.instant("started_at"), Order::Desc)
            .order_by("id", Order::Desc)
            .limit(reviews);
        if let Some(since) = since {
            newest.and_where(
                self.instant("started_at")
                    .gte(self.instant_value("since", &rfc3339(since))?),
            );
        }
        let mut query = Query::select();
        query
            .column(("r", "id"))
            .column(("r", "started_at"))
            .columns([
                ("s", "name"),
                ("s", "role"),
                ("s", "model"),
                ("s", "status"),
                ("s", "error"),
            ])
            .from_subquery(newest, "r")
            .join_as(
                JoinType::InnerJoin,
                "sessions",
                "s",
                Expr::col(("s", "run_id")).equals(("r", "id")),
            )
            .order_by(("r", "at"), Order::Desc)
            .order_by(("r", "id"), Order::Desc)
            .order_by(("s", "name"), Order::Asc);
        let rows = self
            .all(&query)
            .await?
            .iter()
            .map(|row| {
                Ok((
                    get(row, 0)?,
                    at(row, 1)?,
                    get(row, 2)?,
                    get(row, 3)?,
                    get(row, 4)?,
                    get(row, 5)?,
                    get(row, 6)?,
                ))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        session_endings(rows)
    }

    async fn tool_usage(&self, filter: &ToolCallFilter) -> Result<Vec<ToolUsage>, StoreError> {
        let mut query = Query::select();
        Self::calls(&mut query);
        query
            .column(("c", "model"))
            .expr(call_role())
            .expr(call_tool())
            .column(("c", "outcome"))
            .expr(Func::count(Expr::col(Asterisk)))
            .expr(
                Func::coalesce([Expr::col(("c", "elapsed_ms")).sum(), Expr::val(0)])
                    .cast_as("bigint"),
            )
            .cond_where(self.call_conditions(filter)?)
            .add_group_by([
                Expr::col(("c", "model")),
                Expr::cust("2"),
                Expr::cust("3"),
                Expr::col(("c", "outcome")),
            ]);
        let rows = self
            .all(&query)
            .await?
            .iter()
            .map(|row| {
                Ok((
                    get(row, 0)?,
                    get(row, 1)?,
                    get(row, 2)?,
                    get(row, 3)?,
                    count(row, 4)?,
                    count(row, 5)?,
                ))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        Ok(ToolUsage::across_runs(rows))
    }

    async fn list_tool_calls(
        &self,
        filter: &ToolCallFilter,
        page: Page,
    ) -> Result<Vec<ToolCallListing>, StoreError> {
        let mut conditions = self.call_conditions(filter)?;
        match filter.outcome.as_ref() {
            Some(OutcomeFilter::Is(outcome)) => {
                conditions = conditions.add(Expr::col(("c", "outcome")).eq(outcome.as_str()));
            }
            Some(OutcomeFilter::Problems) => {
                conditions = conditions.add(Expr::col(("c", "outcome")).ne("ok"));
            }
            None => {}
        }
        if let Some(key) = &filter.before {
            let at = self.time_value("at", &key.at)?;
            conditions = conditions.add(
                Condition::any()
                    .add(Expr::col(("c", "at")).lt(at.clone()))
                    .add(
                        Condition::all()
                            .add(Expr::col(("c", "at")).eq(at))
                            .add(Expr::col(("c", "id")).lt(key.id)),
                    ),
            );
        }
        let kept = Query::select()
            .expr(Expr::val(1))
            .from_as("transcripts", "t")
            .and_where(Expr::col(("t", "run_id")).equals(("c", "run_id")))
            .and_where(Expr::col(("t", "session")).equals(("c", "session")))
            .take();
        let mut query = Query::select();
        Self::calls(&mut query);
        query
            .join_as(
                JoinType::InnerJoin,
                "runs",
                "r",
                Expr::col(("r", "id")).equals(("c", "run_id")),
            )
            .columns([
                ("c", "id"),
                ("c", "run_id"),
                ("r", "repo"),
                ("r", "target"),
                ("r", "platform"),
                ("c", "at"),
                ("c", "session"),
                ("c", "model"),
                ("c", "turn"),
                ("c", "tool"),
                ("c", "origin"),
                ("c", "outcome"),
                ("c", "arguments"),
                ("c", "arguments_len"),
                ("c", "result_chars"),
                ("c", "elapsed_ms"),
            ])
            .expr(Expr::exists(kept))
            .column(("r", "kind"))
            .cond_where(conditions)
            .order_by(("c", "at"), Order::Desc)
            .order_by(("c", "id"), Order::Desc)
            .limit(page.limit().into())
            .offset(page.offset().into());
        self.all(&query)
            .await?
            .iter()
            .map(|row| {
                Ok(ToolCallListing {
                    id: get(row, 0)?,
                    run_id: get(row, 1)?,
                    repo: get(row, 2)?,
                    target: to_u64("runs.target", get(row, 3)?)?,
                    platform: run_platform(get(row, 4)?)?,
                    kind: run_kind(get(row, 17)?)?,
                    call: ToolCallRecord {
                        at: at(row, 5)?,
                        session: get(row, 6)?,
                        model: get(row, 7)?,
                        turn: to_u32("tool_calls.turn", get(row, 8)?)?,
                        tool: get(row, 9)?,
                        origin: get(row, 10)?,
                        outcome: get(row, 11)?,
                        arguments: get(row, 12)?,
                        arguments_len: count(row, 13)?,
                        result_chars: count(row, 14)?,
                        elapsed_ms: count(row, 15)?,
                    },
                    transcript_kept: get(row, 16)?,
                })
            })
            .collect()
    }

    // --- a few writes, to see inserts, updates and upserts in the builder ----

    async fn create_run(&self, run: &NewRun) -> Result<(), StoreError> {
        let started = self.time_value("started_at", "")?;
        let mut insert = Query::insert();
        insert
            .into_table("runs")
            .columns([
                "id",
                "kind",
                "platform",
                "repo",
                "target",
                "commit_sha",
                "requester",
                "trigger",
                "status",
                "started_at",
                "link",
                "heartbeat_at",
            ])
            .values([
                run.id.as_str().into(),
                kind_str(run.kind).into(),
                platform_str(run.platform).into(),
                run.repo.as_str().into(),
                to_i64("runs.target", run.target)?.into(),
                run.commit.clone().into(),
                run.requester.clone().into(),
                run.trigger.as_str().into(),
                RunStatus::Running.as_str().into(),
                started.clone().into(),
                run.link.as_str().into(),
                started.into(),
            ])
            .map_err(|e| StoreError::Corrupt {
                column: "runs",
                value: e.to_string(),
            })?;
        self.exec(&insert).await.map(|_| ())
    }

    async fn finish_run(
        &self,
        id: &RunId,
        status: RunStatus,
        summary: Option<&str>,
        error: Option<&str>,
    ) -> Result<(), StoreError> {
        let finished = self.time_value("finished_at", "")?;
        let mut update = Query::update();
        update
            .table("runs")
            .values([
                ("status", status.as_str().into()),
                ("finished_at", finished.into()),
                ("summary", summary.map(str::to_owned).into()),
                ("error", error.map(str::to_owned).into()),
            ])
            .and_where(Expr::col("id").eq(id.as_str()));
        self.exec(&update).await.map(|_| ())
    }

    async fn event(&self, run: &RunId, level: &str, message: &str) -> Result<(), StoreError> {
        let mut insert = Query::insert();
        insert
            .into_table("events")
            .columns(["run_id", "at", "level", "message"])
            .values([
                run.as_str().into(),
                self.time_value("at", "")?.into(),
                level.into(),
                message.into(),
            ])
            .map_err(|e| StoreError::Corrupt {
                column: "events",
                value: e.to_string(),
            })?;
        self.exec(&insert).await.map(|_| ())
    }

    async fn link_child(
        &self,
        parent: &RunId,
        stage: Stage,
        child: &RunId,
    ) -> Result<(), StoreError> {
        let mut insert = Query::insert();
        insert
            .into_table("run_children")
            .columns(["parent", "stage", "child"])
            .values([
                parent.as_str().into(),
                stage.as_str().into(),
                child.as_str().into(),
            ])
            .map_err(|e| StoreError::Corrupt {
                column: "run_children",
                value: e.to_string(),
            })?
            .on_conflict(
                OnConflict::columns(["parent", "stage"])
                    .update_column("child")
                    .to_owned(),
            );
        self.exec(&insert).await.map(|_| ())
    }
}

// --- reading values -----------------------------------------------------------

fn get<T: TryGetable>(row: &QueryResult, index: usize) -> Result<T, StoreError> {
    row.try_get_by_index(index).map_err(StoreError::from)
}

/// A count or size column, read as the unsigned number it stands for.
fn count(row: &QueryResult, index: usize) -> Result<u64, StoreError> {
    Ok(u64::try_from(get::<i64>(row, index)?).unwrap_or_default())
}

/// A time column as RFC 3339 text, whichever database it came from: SQLite's
/// text and `PostgreSQL`'s `TIMESTAMPTZ` both read as an `OffsetDateTime`.
fn at(row: &QueryResult, index: usize) -> Result<String, StoreError> {
    Ok(rfc3339(get(row, index)?))
}

fn at_opt(row: &QueryResult, index: usize) -> Result<Option<String>, StoreError> {
    Ok(get::<Option<OffsetDateTime>>(row, index)?.map(rfc3339))
}

fn rfc3339(at: OffsetDateTime) -> String {
    at.format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

fn parse_time(column: &'static str, value: &str) -> Result<OffsetDateTime, StoreError> {
    if value.is_empty() {
        return Ok(OffsetDateTime::now_utc());
    }
    OffsetDateTime::parse(value, &Rfc3339)
        .map(|at| at.to_offset(time::UtcOffset::UTC))
        .map_err(|_| StoreError::Corrupt {
            column,
            value: value.to_owned(),
        })
}

/// Every database error as text, whichever database it came from (the driver
/// errors are sqlx's, behind sea-orm's `DbErr`).
impl From<DbErr> for StoreError {
    fn from(error: DbErr) -> Self {
        Self::Sqlite(error.to_string())
    }
}

/// The columns [`raw_run`] reads, in order. `loop_rounds` is `INTEGER` (an
/// `i32` to the driver) on `PostgreSQL` and 64 bits on SQLite: cast, so one
/// type reads both.
fn run_select() -> SelectStatement {
    let mut query = Query::select();
    query
        .columns([
            "id",
            "kind",
            "platform",
            "repo",
            "target",
            "commit_sha",
            "requester",
            "trigger",
            "status",
            "started_at",
            "finished_at",
            "link",
            "summary",
            "error",
            "heartbeat_at",
            "check_id",
            "superseded_by",
            "loop_stop",
        ])
        .expr(Expr::col("loop_rounds").cast_as("bigint"));
    query
}

fn raw_run(row: &QueryResult) -> Result<RawRun, StoreError> {
    Ok(RawRun {
        id: get(row, 0)?,
        kind: get(row, 1)?,
        platform: get(row, 2)?,
        repo: get(row, 3)?,
        target: get(row, 4)?,
        commit: get(row, 5)?,
        requester: get(row, 6)?,
        trigger: get(row, 7)?,
        status: get(row, 8)?,
        started_at: at(row, 9)?,
        finished_at: at_opt(row, 10)?,
        link: get(row, 11)?,
        summary: get(row, 12)?,
        error: get(row, 13)?,
        heartbeat_at: at_opt(row, 14)?,
        check_id: get(row, 15)?,
        superseded_by: get(row, 16)?,
        loop_stop: get(row, 17)?,
        loop_rounds: get(row, 18)?,
    })
}

fn inbound_event(row: &QueryResult) -> Result<InboundEvent, StoreError> {
    let target: Option<i64> = get(row, 5)?;
    Ok(InboundEvent {
        id: event_id(get(row, 0)?)?,
        received_at: at(row, 1)?,
        source: get(row, 2)?,
        kind: get(row, 3)?,
        repo: get(row, 4)?,
        target: target
            .map(|t| to_u64("inbound_events.target", t))
            .transpose()?,
        payload: get(row, 6)?,
        requester: get(row, 7)?,
    })
}

/// The tool a call `c` counts under, as [`crate::types::counted_as`] says:
/// every tool that does not exist, which is every call with no origin, as one
/// (#341).
fn call_tool() -> Expr {
    Expr::case(Expr::col(("c", "origin")).eq(""), "(unknown)")
        .finally(Expr::col(("c", "tool")))
        .into()
}

/// The role of the session that made a call `c`; a call whose session has no
/// row counts as a lane's.
fn call_role() -> Expr {
    Func::coalesce([Expr::col(("s", "role")), Expr::val("lane")]).into()
}

// --- the trait ------------------------------------------------------------------

/// What an unported method answers.
fn not_ported<T>() -> Result<T, StoreError> {
    Err(StoreError::Postgres("not ported in the spike".into()))
}

/// Writes `impl RunStore for Candidate` once: ported methods call the
/// inherent method of the same name, the others answer "not ported".
/// (`#[async_trait]` cannot see methods a macro produces inside the impl,
/// so the macro wraps the whole impl.)
macro_rules! run_store {
    (
        ported { $( fn $p:ident(&self $(, $pa:ident: $pt:ty)*) -> $pr:ty; )* }
        unported { $( fn $u:ident(&self $(, $ua:ident: $ut:ty)*) -> $ur:ty; )* }
    ) => {
        #[async_trait]
        impl RunStore for Candidate {
            $( async fn $p(&self $(, $pa: $pt)*) -> $pr { Candidate::$p(self $(, $pa)*).await } )*
            $( async fn $u(&self $(, $ua: $ut)*) -> $ur { let _ = ($(&$ua,)*); not_ported() } )*
        }
    };
}

run_store! {
    ported {
        fn create_run(&self, run: &NewRun) -> Result<(), StoreError>;
        fn finish_run(&self, id: &RunId, status: RunStatus, summary: Option<&str>, error: Option<&str>) -> Result<(), StoreError>;
        fn run(&self, id: &RunId) -> Result<Option<RunRecord>, StoreError>;
        fn orphaned_runs(&self, stale_before: OffsetDateTime) -> Result<Vec<RunRecord>, StoreError>;
        fn latest_reviews(&self, since: OffsetDateTime) -> Result<Vec<RunRecord>, StoreError>;
        fn stages(&self, run: &RunId) -> Result<Vec<StageRecord>, StoreError>;
        fn link_child(&self, parent: &RunId, stage: Stage, child: &RunId) -> Result<(), StoreError>;
        fn children(&self, parent: &RunId) -> Result<Vec<(Stage, RunId)>, StoreError>;
        fn sessions(&self, run: &RunId) -> Result<Vec<SessionRecord>, StoreError>;
        fn findings(&self, run: &RunId) -> Result<Vec<FindingRecord>, StoreError>;
        fn drafts(&self, run: &RunId) -> Result<Vec<DraftRecord>, StoreError>;
        fn tool_calls(&self, run: &RunId) -> Result<Vec<ToolCallRecord>, StoreError>;
        fn tool_usage(&self, filter: &ToolCallFilter) -> Result<Vec<ToolUsage>, StoreError>;
        fn list_tool_calls(&self, filter: &ToolCallFilter, page: Page) -> Result<Vec<ToolCallListing>, StoreError>;
        fn rounds(&self, run: &RunId) -> Result<Vec<RoundRecord>, StoreError>;
        fn transcripts(&self, run: &RunId) -> Result<Vec<TranscriptSummary>, StoreError>;
        fn transcript(&self, run: &RunId, session: &str) -> Result<Option<TranscriptRecord>, StoreError>;
        fn event(&self, run: &RunId, level: &str, message: &str) -> Result<(), StoreError>;
        fn events(&self, run: &RunId) -> Result<Vec<EventRecord>, StoreError>;
        fn inbound_event(&self, id: &EventId) -> Result<Option<InboundEvent>, StoreError>;
        fn event_facets(&self) -> Result<EventFacets, StoreError>;
        fn outcomes(&self, id: &EventId) -> Result<Vec<OutcomeRecord>, StoreError>;
        fn inbound_events_for_run(&self, run: &RunId) -> Result<Vec<InboundEvent>, StoreError>;
        fn list_runs(&self, filter: &RunFilter, page: Page) -> Result<Vec<RunRecord>, StoreError>;
        fn daily_stats(&self, since: OffsetDateTime) -> Result<Vec<DayCounts>, StoreError>;
        fn sessions_of(&self, runs: &[RunId]) -> Result<Vec<(RunId, String, SessionRole, SessionStatus)>, StoreError>;
        fn session_endings(&self, since: Option<OffsetDateTime>, reviews: u32) -> Result<Vec<SessionEnding>, StoreError>;
        fn stages_of(&self, runs: &[RunId]) -> Result<Vec<(RunId, Stage, StageState)>, StoreError>;
        fn count_runs(&self, filter: &RunFilter) -> Result<u64, StoreError>;
        fn list_inbound_events(&self, filter: &EventFilter, page: Page) -> Result<Vec<EventWithOutcomes>, StoreError>;
    }
    unported {
        fn supersede_run(&self, id: &RunId, by: &RunId, reason: &str) -> Result<(), StoreError>;
        fn heartbeat(&self, id: &RunId) -> Result<(), StoreError>;
        fn end_loop(&self, id: &RunId, stop: &str, rounds: u32) -> Result<(), StoreError>;
        fn set_check(&self, id: &RunId, check_id: &str) -> Result<(), StoreError>;
        fn claim_resume(&self, run: &RunId, interrupted: &str, resumed: &str) -> Result<bool, StoreError>;
        fn drop_running_sessions(&self, run: &RunId, reason: &str) -> Result<(), StoreError>;
        fn stage(&self, run: &RunId, write: &StageWrite) -> Result<(), StoreError>;
        fn fail_running_stages(&self, run: &RunId, reason: &str) -> Result<(), StoreError>;
        fn start_session(&self, run: &RunId, name: &str, role: SessionRole, model: &str) -> Result<(), StoreError>;
        fn finish_session(&self, run: &RunId, name: &str, status: SessionStatus, turns: u64, tokens: TokenUsage, error: Option<&str>) -> Result<(), StoreError>;
        fn record_finding(&self, run: &RunId, session: &str, path: &str, line_number: u32, comment_id: &str, action: FindingAction) -> Result<(), StoreError>;
        fn record_draft(&self, run: &RunId, draft: &DraftRecord) -> Result<(), StoreError>;
        fn decide_draft(&self, run: &RunId, draft: &str, decision: &DraftDecision) -> Result<(), StoreError>;
        fn record_tool_call(&self, run: &RunId, call: &ToolCallRecord) -> Result<(), StoreError>;
        fn start_round(&self, run: &RunId, round: &RoundRecord) -> Result<(), StoreError>;
        fn end_round(&self, run: &RunId, session: &str, round: u32, turns: u32) -> Result<(), StoreError>;
        fn record_transcript(&self, run: &RunId, transcript: &TranscriptRecord) -> Result<(), StoreError>;
        fn joined(&self, run: &RunId, source: &str) -> Result<(), StoreError>;
        fn record_event(&self, event: &InboundEvent) -> Result<(), StoreError>;
        fn record_outcome(&self, outcome: &OutcomeRecord) -> Result<(), StoreError>;
        fn prune_events(&self, older_than: OffsetDateTime) -> Result<PruneCounts, StoreError>;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::print_stderr)]

    use henk_domain::allowlist::Platform;

    use super::*;
    use crate::types::RunKey;

    fn candidate(backend: DbBackend) -> Candidate {
        Candidate {
            db: DatabaseConnection::default(),
            backend,
        }
    }

    /// #216: the unfiltered run listing walks the instant index instead of
    /// sorting, so the SQL built here and the index of migration 002 stay the
    /// same expression.
    #[tokio::test]
    async fn the_run_listing_walks_the_instant_index_instead_of_sorting() {
        let path = std::env::temp_dir().join(format!("henk-plan-{}.db", std::process::id()));
        let store = Candidate::open(&path).await.unwrap();
        let query = store
            .list_runs_query(&RunFilter::default(), Page::new(50, 0))
            .unwrap();
        let statement = store.backend.build(&query);
        let plan = store
            .db
            .query_all_raw(sea_orm::Statement::from_sql_and_values(
                store.backend,
                format!("EXPLAIN QUERY PLAN {}", statement.sql),
                statement.values.map_or_else(Vec::new, |v| v.0),
            ))
            .await
            .unwrap()
            .iter()
            .map(|row| row.try_get_by_index::<String>(3).unwrap())
            .collect::<Vec<_>>();
        drop(store);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
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

    /// The SQL of a full run listing on each database, without a database.
    #[test]
    fn the_run_listing_is_one_builder_and_two_dialects() {
        let filter = RunFilter {
            kind: Some(RunKind::Review),
            status: Some(RunStatus::Finished),
            platform: Some(Platform::GitHub),
            repo: Some("o/r".into()),
            target: Some(7),
            since: Some("2026-10-01T10:00:00Z".into()),
            until: Some("2026-10-02T10:00:00.5Z".into()),
            before: Some(RunKey {
                started_at: "2026-10-01T10:00:00.4Z".into(),
                id: "r-003".into(),
            }),
        };
        let sql = |backend| {
            let query = candidate(backend)
                .list_runs_query(&filter, Page::new(50, 0))
                .unwrap();
            backend.build(&query).sql
        };
        // One builder: `?` placeholders and a `substr` instant on SQLite,
        // `$n` placeholders and the plain timestamp on PostgreSQL.
        let sqlite = sql(DbBackend::Sqlite);
        assert!(
            sqlite.contains("\"kind\" = ? AND \"status\" = ?"),
            "{sqlite}"
        );
        assert!(
            sqlite.contains("ORDER BY substr(started_at, 1, 19)"),
            "{sqlite}"
        );
        let postgres = sql(DbBackend::Postgres);
        assert!(
            postgres.contains("\"kind\" = $1 AND \"status\" = $2"),
            "{postgres}"
        );
        assert!(
            postgres.contains("ORDER BY started_at DESC, \"id\" DESC LIMIT $11 OFFSET $12"),
            "{postgres}"
        );
    }
}
