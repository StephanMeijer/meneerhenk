//! The run store on sqlx's `Any` driver (spike #601, candidate "sqlx"): one
//! implementation over one pool type for `SQLite` and `PostgreSQL`, queries as
//! plain SQL through `sqlx::query` (no macros, so no database at build time),
//! and sqlx's own migrator over one numbered sequence of steps.
//!
//! What the `Any` driver cannot carry, this file works around, and says so:
//!
//! - It has no time type, so a time goes in and out as RFC 3339 text. On
//!   `SQLite` that is the column as it is; on `PostgreSQL` the column is a
//!   `TIMESTAMPTZ`, so every read goes through `to_char` ([`Dialect::out`])
//!   and every write through a `::timestamptz` cast ([`Dialect::ts`]).
//! - It has no `u64`/`u32`: numbers are bound and read as `i64`.
//! - It passes SQL text to the driver as it is, so `$1` placeholders are used
//!   for both (`SQLite` accepts `$NNN` as a numbered parameter) and the few
//!   statements whose text differs are built from [`Dialect`] fragments.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::path::Path;

use async_trait::async_trait;
use henk_domain::run::{EventId, RunId, RunKind};
use sqlx::any::{AnyArguments, AnyPoolOptions, AnyRow};
use sqlx::migrate::{Migration, MigrationType, Migrator};
use sqlx::query::Query;
use sqlx::{Any, AnyPool, AssertSqlSafe, Row as _, SqlSafeStr as _};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::store::RunStore;
use crate::types::{
    DayCounts, DraftDecision, DraftRecord, EventFacets, EventFilter, EventRecord,
    EventWithOutcomes, FindingAction, FindingRecord, InboundEvent,
    MOST_SESSION_REVIEWS, NewRun, OutcomeFilter, OutcomeRecord, OutcomeRow, Page, PruneCounts,
    RawRun, RoundRecord, RunFilter, RunRecord, RunStatus, SessionEnding, SessionRecord,
    SessionRole, SessionStatus, Stage, StageRecord, StageState, StageWrite, StoreError, TokenUsage,
    ToolCallFilter, ToolCallListing, ToolCallRecord, ToolUsage, TranscriptRecord,
    TranscriptSummary, attach_outcomes, child_records, draft_verdict, event_id, kind_str, merge_days,
    now, platform_str, round_record, run_kind, run_platform, session_dots, session_endings,
    session_role, session_status, stage_dots, stage_records, to_i64, to_u32, to_u64,
};

/// Connections kept open at most on `PostgreSQL`, as in the `PostgreSQL` store.
const PG_POOL_SIZE: u32 = 8;

/// Connections kept open at most for reading a `SQLite` file. WAL lets
/// readers run beside the one writer.
const SQLITE_READERS: u32 = 4;

/// The database a [`Candidate`] talks to: where the SQL differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dialect {
    Sqlite,
    Postgres,
}

impl Dialect {
    /// A time column read back as RFC 3339 text, as the other stores return
    /// it: UTC, `Z`, the fraction without trailing zeros.
    fn out(self, column: &str) -> String {
        match self {
            Self::Sqlite => column.to_owned(),
            Self::Postgres => format!(
                "rtrim(rtrim(to_char({column} AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US'), '0'), '.') || 'Z'"
            ),
        }
    }

    /// A time column as something that orders and compares as the instant it
    /// is: `SQLite` keeps text, and "12:00:00.4Z" sorts before "12:00:00Z"
    /// as text (#216).
    fn instant(self, column: &str) -> String {
        match self {
            Self::Sqlite => format!(
                "substr({column}, 1, 19) || substr(CASE WHEN substr({column}, 20, 1) = '.' THEN substr({column}, 21, length({column}) - 21) ELSE '' END || '000000000', 1, 9)"
            ),
            Self::Postgres => column.to_owned(),
        }
    }

    /// Parameter `n` where a time is stored or compared with
    /// [`Dialect::instant`]: the text is a `TIMESTAMPTZ` on `PostgreSQL`, and
    /// the `Any` driver binds text.
    fn ts(self, n: u8) -> String {
        match self {
            Self::Sqlite => format!("${n}"),
            Self::Postgres => format!("${n}::timestamptz"),
        }
    }

    /// The UTC day of a time column, `YYYY-MM-DD`.
    fn day(self, column: &str) -> String {
        match self {
            Self::Sqlite => format!("substr({column}, 1, 10)"),
            Self::Postgres => format!("to_char({column} AT TIME ZONE 'UTC', 'YYYY-MM-DD')"),
        }
    }

    /// The column that keeps rows in recording order in a table with no
    /// identity column of its own on `SQLite`.
    fn recorded(self) -> &'static str {
        match self {
            Self::Sqlite => "rowid",
            Self::Postgres => "id",
        }
    }

    /// A time the caller gave, as the text to store: UTC, RFC 3339. Empty
    /// means now.
    fn stored(self, column: &'static str, value: &str) -> Result<String, StoreError> {
        if value.is_empty() {
            return Ok(now());
        }
        Ok(rfc3339(parse_time(column, value)?))
    }

    /// A time as the text to bind where it is compared with
    /// [`Dialect::instant`].
    fn bound(self, column: &'static str, value: &str) -> Result<String, StoreError> {
        let at = parse_time(column, value)?;
        Ok(match self {
            Self::Sqlite => instant_text(at),
            Self::Postgres => rfc3339(at),
        })
    }

    /// [`Dialect::bound`] for a time already parsed.
    fn bound_at(self, at: OffsetDateTime) -> String {
        match self {
            Self::Sqlite => instant_text(at),
            Self::Postgres => rfc3339(at),
        }
    }
}

fn parse_time(column: &'static str, value: &str) -> Result<OffsetDateTime, StoreError> {
    OffsetDateTime::parse(value, &Rfc3339)
        .map(|at| at.to_offset(time::UtcOffset::UTC))
        .map_err(|_| StoreError::Corrupt {
            column,
            value: value.to_owned(),
        })
}

fn rfc3339(at: OffsetDateTime) -> String {
    at.to_offset(time::UtcOffset::UTC)
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

/// A time as a fixed-width UTC text that sorts as the time does:
/// `2026-10-07T12:00:00` and nine digits of nanoseconds.
fn instant_text(at: OffsetDateTime) -> String {
    let at = at.to_offset(time::UtcOffset::UTC);
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
}

/// The migrations, one numbered sequence: a step is a file with a section
/// per database ([`step_sql`]).
const STEPS: &[(i64, &str, &str)] = &[
    (
        1,
        "schema",
        include_str!("../migrations/sqlx/001_schema.sql"),
    ),
    (
        2,
        "run_instant",
        include_str!("../migrations/sqlx/002_run_instant.sql"),
    ),
    (
        3,
        "run_children",
        include_str!("../migrations/sqlx/003_run_children.sql"),
    ),
];

/// The step of this sequence a database already is at when its old
/// bookkeeping says `SQLite` `PRAGMA user_version = 20` or `PostgreSQL`
/// `henk_schema_migrations` up to 16: the end of the history these steps
/// restate. A full port maps every older version to its step (the table is
/// the 17-step one) and refuses the rest.
const ADOPTED_AT_STEP: i64 = 3;
const LEGACY_SQLITE_VERSION: i64 = 20;
const LEGACY_POSTGRES_VERSION: i64 = 16;

/// What a step runs on `dialect`: its `@both` sections, then its own, in
/// file order. `None` when the step has nothing for it.
fn step_sql(text: &str, dialect: Dialect) -> Option<String> {
    let want = match dialect {
        Dialect::Sqlite => "-- @sqlite",
        Dialect::Postgres => "-- @postgres",
    };
    let (mut both, mut own) = (String::new(), String::new());
    let mut section: Option<&mut String> = None;
    for line in text.lines() {
        match line.trim() {
            "-- @both" => section = Some(&mut both),
            "-- @sqlite" | "-- @postgres" => {
                section = (line.trim() == want).then_some(&mut own);
            }
            _ => {
                if let Some(section) = section.as_deref_mut() {
                    section.push_str(line);
                    section.push('\n');
                }
            }
        }
    }
    both.push_str(&own);
    (!both.trim().is_empty()).then_some(both)
}

fn migrator(dialect: Dialect) -> Migrator {
    Migrator::with_migrations(
        STEPS
            .iter()
            .filter_map(|&(version, name, text)| {
                step_sql(text, dialect).map(|sql| {
                    Migration::new(
                        version,
                        Cow::Borrowed(name),
                        MigrationType::Simple,
                        AssertSqlSafe(sql).into_sql_str(),
                        false,
                    )
                })
            })
            .collect(),
    )
}

/// The run store over sqlx's `Any` driver.
#[derive(Debug)]
pub struct Candidate {
    /// Where writes (and the migrations) go. On a `SQLite` file this is one
    /// connection: writers queue in the pool, in order, instead of polling
    /// the file's lock against each other (see [`Candidate::connect`]).
    writer: AnyPool,
    /// Where reads go: the same pool, except on a `SQLite` file, where WAL
    /// lets several readers run beside the writer.
    reader: AnyPool,
    dialect: Dialect,
}

impl Candidate {
    /// Opens or creates the `SQLite` database at `path` and migrates it.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the file cannot be opened or migrated.
    pub async fn open(path: &Path) -> Result<Self, StoreError> {
        let encoded = path
            .to_string_lossy()
            .replace('%', "%25")
            .replace('?', "%3F")
            .replace('#', "%23")
            .replace(' ', "%20");
        Self::connect(&format!("sqlite://{encoded}?mode=rwc")).await
    }

    /// An in-memory store, for tests and probes.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when `SQLite` cannot create the database.
    pub async fn in_memory() -> Result<Self, StoreError> {
        Self::connect("sqlite::memory:").await
    }

    /// Connects to the database at `url` (`postgres://`, or `sqlite://` with
    /// a path) and brings its schema up to date. `sslmode` in a `PostgreSQL`
    /// URL decides TLS.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the URL does not parse, the server cannot
    /// be reached, or migrating fails.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        sqlx::any::install_default_drivers();
        let dialect = if url.starts_with("postgres:") || url.starts_with("postgresql:") {
            Dialect::Postgres
        } else if url.starts_with("sqlite:") {
            Dialect::Sqlite
        } else {
            return Err(StoreError::Connect(
                "the database URL is neither postgres: nor sqlite:".to_owned(),
            ));
        };
        let in_memory = url.contains(":memory:");
        let connect = |max: u32| {
            // Every connection to `sqlite::memory:` gets its own database
            // through the `Any` driver, so memory is one connection that is
            // never closed. Foreign keys are on by default in sqlx, and so is
            // a busy timeout (5 s).
            AnyPoolOptions::new()
                .max_connections(max)
                .min_connections(u32::from(in_memory))
                .idle_timeout(None)
                .max_lifetime(None)
                .connect(url)
        };
        let to_error = |error: sqlx::Error| StoreError::Connect(error.to_string());
        let (writer, reader) = match dialect {
            Dialect::Postgres => {
                let pool = connect(PG_POOL_SIZE).await.map_err(to_error)?;
                (pool.clone(), pool)
            }
            Dialect::Sqlite if in_memory => {
                let pool = connect(1).await.map_err(to_error)?;
                (pool.clone(), pool)
            }
            Dialect::Sqlite => {
                // One writer connection and a few readers. Eight tasks
                // writing through a pool of four connections to a file on a
                // disk (an fsync per commit) got "database is locked" after
                // the 5 s busy timeout in about half the runs: SQLite's busy
                // handler polls the lock, it does not queue, and a connection
                // that sleeps between polls starves behind three that
                // take the lock again at once. A pool of one queues them in
                // order, and WAL lets the readers run beside it.
                let writer = connect(1).await.map_err(to_error)?;
                // WAL is a property of the file, so it is set once.
                sqlx::query("PRAGMA journal_mode = WAL")
                    .execute(&writer)
                    .await
                    .map_err(to_error)?;
                (writer, connect(SQLITE_READERS).await.map_err(to_error)?)
            }
        };
        let store = Self {
            writer,
            reader,
            dialect,
        };
        store.migrate().await?;
        Ok(store)
    }

    /// Brings the schema up to date. A database the previous stores made is
    /// adopted first: its old bookkeeping is read, the steps it already has
    /// are recorded in sqlx's `_sqlx_migrations` without running, and the
    /// rest runs. The old bookkeeping (`user_version`, `henk_schema_migrations`)
    /// is left as it is, and nothing writes to it again.
    async fn migrate(&self) -> Result<(), StoreError> {
        let migrator = migrator(self.dialect);
        if let Some(legacy) = self.legacy_version().await? {
            let (adopted, known) = match self.dialect {
                Dialect::Sqlite => (LEGACY_SQLITE_VERSION, "SQLite 020"),
                Dialect::Postgres => (LEGACY_POSTGRES_VERSION, "PostgreSQL 016"),
            };
            if legacy != adopted {
                return Err(StoreError::Schema(format!(
                    "the database is at schema version {legacy}; only {known} is adopted, \
                     open it with the previous release first"
                )));
            }
            migrator.skip(&self.writer, Some(ADOPTED_AT_STEP)).await?;
        }
        migrator.run(&self.writer).await?;
        Ok(())
    }

    /// The schema version the previous stores recorded, when they did.
    async fn legacy_version(&self) -> Result<Option<i64>, StoreError> {
        match self.dialect {
            Dialect::Sqlite => {
                let version: i64 = sqlx::query("SELECT user_version FROM pragma_user_version")
                    .fetch_one(&self.writer)
                    .await?
                    .try_get(0)?;
                Ok((version > 0).then_some(version))
            }
            Dialect::Postgres => {
                let present: i64 = sqlx::query(
                    "SELECT CASE WHEN to_regclass('henk_schema_migrations') IS NULL THEN 0 ELSE 1 END",
                )
                .fetch_one(&self.writer)
                .await?
                .try_get(0)?;
                if present == 0 {
                    return Ok(None);
                }
                let version: i64 = sqlx::query(
                    "SELECT CAST(COALESCE(MAX(version), 0) AS BIGINT) FROM henk_schema_migrations",
                )
                .fetch_one(&self.writer)
                .await?
                .try_get(0)?;
                Ok((version > 0).then_some(version))
            }
        }
    }
}

/// A query to bind to.
type Q<'q> = Query<'q, Any, AnyArguments>;

/// `$1, $2, ... $n` for an `IN (...)` list.
fn placeholders(from: usize, n: usize) -> String {
    let mut list = String::new();
    for i in from..from + n {
        if !list.is_empty() {
            list.push_str(", ");
        }
        let _ = write!(list, "${i}");
    }
    list
}

/// The columns [`raw_run`] reads, in order, of `runs` as `table` (`""` or
/// `"r."`).
fn run_columns(d: Dialect, table: &str) -> String {
    let c = |name: &str| format!("{table}{name}");
    let t = |name: &str| d.out(&c(name));
    format!(
        "{table}id, {table}kind, {table}platform, {table}repo, {table}target, {table}commit_sha, {table}requester, {table}trigger, {table}status, {}, {}, {table}link, {table}summary, {table}error, {}, {table}check_id, {table}superseded_by, {table}loop_stop, {table}loop_rounds",
        t("started_at"),
        t("finished_at"),
        t("heartbeat_at"),
    )
}

fn raw_run(row: &AnyRow) -> Result<RawRun, StoreError> {
    Ok(RawRun {
        id: row.try_get(0)?,
        kind: row.try_get(1)?,
        platform: row.try_get(2)?,
        repo: row.try_get(3)?,
        target: row.try_get(4)?,
        commit: row.try_get(5)?,
        requester: row.try_get(6)?,
        trigger: row.try_get(7)?,
        status: row.try_get(8)?,
        started_at: row.try_get(9)?,
        finished_at: row.try_get(10)?,
        link: row.try_get(11)?,
        summary: row.try_get(12)?,
        error: row.try_get(13)?,
        heartbeat_at: row.try_get(14)?,
        check_id: row.try_get(15)?,
        superseded_by: row.try_get(16)?,
        loop_stop: row.try_get(17)?,
        loop_rounds: row.try_get(18)?,
    })
}

fn runs_of(rows: &[AnyRow]) -> Result<Vec<RunRecord>, StoreError> {
    rows.iter().map(|row| raw_run(row)?.into_record()).collect()
}

/// The `WHERE` of a run listing, over parameters 1 to 9: kind, status,
/// platform, repo, target, since, until, and the keyset (time, id).
/// Times compare as instants (#216).
fn run_filter(d: Dialect) -> String {
    let at = d.instant("started_at");
    format!(
        "($1 IS NULL OR kind = $1) AND ($2 IS NULL OR status = $2)
         AND ($3 IS NULL OR platform = $3) AND ($4 IS NULL OR repo = $4)
         AND ($5 IS NULL OR target = $5)
         AND ($6 IS NULL OR {at} >= {t6}) AND ($7 IS NULL OR {at} < {t7})
         AND ($8 IS NULL OR {at} < {t8} OR ({at} = {t8} AND id < $9))",
        t6 = d.ts(6),
        t7 = d.ts(7),
        t8 = d.ts(8),
    )
}

/// A run filter as the nine values [`run_filter`] takes.
struct RunBinds<'a> {
    kind: Option<&'static str>,
    status: Option<&'static str>,
    platform: Option<&'static str>,
    repo: Option<&'a str>,
    target: Option<i64>,
    since: Option<String>,
    until: Option<String>,
    before_at: Option<String>,
    before_id: Option<&'a str>,
}

impl<'a> RunBinds<'a> {
    fn of(d: Dialect, filter: &'a RunFilter) -> Result<Self, StoreError> {
        let time = |column, value: &Option<String>| {
            value.as_deref().map(|v| d.bound(column, v)).transpose()
        };
        Ok(Self {
            kind: filter.kind.map(kind_str),
            status: filter.status.map(RunStatus::as_str),
            platform: filter.platform.map(platform_str),
            repo: filter.repo.as_deref(),
            target: filter.target.map(|t| i64::try_from(t).unwrap_or(i64::MAX)),
            since: time("since", &filter.since)?,
            until: time("until", &filter.until)?,
            before_at: filter
                .before
                .as_ref()
                .map(|k| d.bound("before", &k.started_at))
                .transpose()?,
            before_id: filter.before.as_ref().map(|k| k.id.as_str()),
        })
    }

    fn bind<'q>(&'q self, query: Q<'q>) -> Q<'q> {
        query
            .bind(self.kind)
            .bind(self.status)
            .bind(self.platform)
            .bind(self.repo)
            .bind(self.target)
            .bind(self.since.as_deref())
            .bind(self.until.as_deref())
            .bind(self.before_at.as_deref())
            .bind(self.before_id)
    }
}

/// The `WHERE` of a tool call tally or listing over `tool_calls c` and
/// [`CALL_SESSION`], over parameters 1 to 5: tool, model, session role,
/// since and until. A tool is matched as [`CALL_TOOL`] counts it.
fn call_filter(d: Dialect) -> String {
    let at = d.instant("c.at");
    format!(
        "($1 IS NULL OR {CALL_TOOL} = $1) AND ($2 IS NULL OR c.model = $2)
         AND ($3 IS NULL OR {CALL_ROLE} = $3)
         AND ($4 IS NULL OR {at} >= {t4}) AND ($5 IS NULL OR {at} < {t5})",
        t4 = d.ts(4),
        t5 = d.ts(5),
    )
}

/// The session row of a tool call `c`, as `s`, for its role.
const CALL_SESSION: &str = "LEFT JOIN sessions s ON s.run_id = c.run_id AND s.name = c.session";

/// The role of the session that made a tool call `c`, over
/// [`CALL_SESSION`]. A call whose session has no row counts as a lane's.
const CALL_ROLE: &str = "COALESCE(s.role, 'lane')";

/// The tool a call `c` counts under, as [`crate::types::counted_as`] says.
const CALL_TOOL: &str = "CASE WHEN c.origin = '' THEN '(unknown)' ELSE c.tool END";

fn inbound_columns(d: Dialect, table: &str) -> String {
    format!(
        "{table}id, {}, {table}source, {table}kind, {table}repo, {table}target, {table}payload, {table}requester",
        d.out(&format!("{table}received_at"))
    )
}

fn inbound_row(row: &AnyRow) -> Result<InboundEvent, StoreError> {
    let target: Option<i64> = row.try_get(5)?;
    Ok(InboundEvent {
        id: event_id(row.try_get(0)?)?,
        received_at: row.try_get(1)?,
        source: row.try_get(2)?,
        kind: row.try_get(3)?,
        repo: row.try_get(4)?,
        target: target
            .map(|t| to_u64("inbound_events.target", t))
            .transpose()?,
        payload: row.try_get(6)?,
        requester: row.try_get(7)?,
    })
}

impl Candidate {
    fn d(&self) -> Dialect {
        self.dialect
    }

    /// Runs `sql` with `bind` applied and returns every row.
    async fn fetch<'q>(
        &self,
        sql: String,
        bind: impl FnOnce(Q<'q>) -> Q<'q>,
    ) -> Result<Vec<AnyRow>, StoreError> {
        Ok(bind(sqlx::query(AssertSqlSafe(sql)))
            .fetch_all(&self.reader)
            .await?)
    }

    /// Runs `sql` with `bind` applied for its effect.
    async fn exec<'q>(
        &self,
        sql: String,
        bind: impl FnOnce(Q<'q>) -> Q<'q>,
    ) -> Result<(), StoreError> {
        bind(sqlx::query(AssertSqlSafe(sql)))
            .execute(&self.writer)
            .await?;
        Ok(())
    }
}

/// Implements [`RunStore`] for [`Candidate`]: the methods ported to sqlx, and
/// the ones that are not yet, which fail the same way.
macro_rules! run_store {
    (
        ported { $($ported:tt)* }
        unported { $( $name:ident ( $($arg:ident : $ty:ty),* ) -> $ret:ty; )* }
    ) => {
        #[async_trait]
        impl RunStore for Candidate {
            $($ported)*
            $(
                async fn $name(&self $(, $arg: $ty)*) -> $ret {
                    $( let _ = $arg; )*
                    Err(StoreError::Postgres("not ported in the spike".into()))
                }
            )*
        }
    };
}

run_store! {
ported {
    async fn create_run(&self, run: &NewRun) -> Result<(), StoreError> {
        let d = self.d();
        let target = to_i64("runs.target", run.target)?;
        let started = now();
        self.exec(
            format!(
                "INSERT INTO runs (id, kind, platform, repo, target, commit_sha, requester, trigger, status, started_at, link, heartbeat_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, {t10}, $11, {t10})",
                t10 = d.ts(10)
            ),
            |q| {
                q.bind(run.id.as_str())
                    .bind(kind_str(run.kind))
                    .bind(platform_str(run.platform))
                    .bind(run.repo.as_str())
                    .bind(target)
                    .bind(run.commit.as_deref())
                    .bind(run.requester.as_deref())
                    .bind(run.trigger.as_str())
                    .bind(RunStatus::Running.as_str())
                    .bind(started.as_str())
                    .bind(run.link.as_str())
            },
        )
        .await
    }

    async fn run(&self, id: &RunId) -> Result<Option<RunRecord>, StoreError> {
        let rows = self
            .fetch(
                format!("SELECT {} FROM runs WHERE id = $1", run_columns(self.d(), "")),
                |q| q.bind(id.as_str()),
            )
            .await?;
        rows.first().map(|row| raw_run(row)?.into_record()).transpose()
    }

    async fn orphaned_runs(
        &self,
        stale_before: OffsetDateTime,
    ) -> Result<Vec<RunRecord>, StoreError> {
        let d = self.d();
        let cutoff = d.bound_at(stale_before);
        let rows = self
            .fetch(
                format!(
                    "SELECT {} FROM runs
                     WHERE status = $1 AND (heartbeat_at IS NULL OR {} < {})
                     ORDER BY {}, id",
                    run_columns(d, ""),
                    d.instant("heartbeat_at"),
                    d.ts(2),
                    d.instant("started_at"),
                ),
                |q| q.bind(RunStatus::Running.as_str()).bind(cutoff.as_str()),
            )
            .await?;
        runs_of(&rows)
    }

    async fn latest_reviews(&self, since: OffsetDateTime) -> Result<Vec<RunRecord>, StoreError> {
        let d = self.d();
        let since = d.bound_at(since);
        let (r, n) = (d.instant("r.started_at"), d.instant("n.started_at"));
        let rows = self
            .fetch(
                format!(
                    "SELECT {} FROM runs r
                     WHERE r.kind = $1 AND {r} >= {} AND NOT EXISTS (
                         SELECT 1 FROM runs n
                         WHERE n.kind = r.kind AND n.platform = r.platform
                           AND n.repo = r.repo AND n.target = r.target
                           AND ({n} > {r} OR ({n} = {r} AND n.id > r.id)))
                     ORDER BY {r}, r.id",
                    run_columns(d, "r."),
                    d.ts(2),
                ),
                |q| q.bind(kind_str(RunKind::Review)).bind(since.as_str()),
            )
            .await?;
        runs_of(&rows)
    }

    async fn stages(&self, run: &RunId) -> Result<Vec<StageRecord>, StoreError> {
        let d = self.d();
        let rows = self
            .fetch(
                format!(
                    "SELECT stage, state, {}, {}, detail FROM stages WHERE run_id = $1",
                    d.out("started_at"),
                    d.out("ended_at")
                ),
                |q| q.bind(run.as_str()),
            )
            .await?;
        let rows = rows
            .iter()
            .map(|row| {
                Ok((
                    row.try_get(0)?,
                    row.try_get(1)?,
                    row.try_get(2)?,
                    row.try_get(3)?,
                    row.try_get(4)?,
                ))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        stage_records(rows)
    }

    async fn link_child(
        &self,
        parent: &RunId,
        stage: Stage,
        child: &RunId,
    ) -> Result<(), StoreError> {
        self.exec(
            "INSERT INTO run_children (parent, stage, child) VALUES ($1, $2, $3)
             ON CONFLICT (parent, stage) DO UPDATE SET child = excluded.child"
                .to_owned(),
            |q| q.bind(parent.as_str()).bind(stage.as_str()).bind(child.as_str()),
        )
        .await
    }

    async fn children(&self, parent: &RunId) -> Result<Vec<(Stage, RunId)>, StoreError> {
        let rows = self
            .fetch(
                "SELECT stage, child FROM run_children WHERE parent = $1".to_owned(),
                |q| q.bind(parent.as_str()),
            )
            .await?;
        let rows = rows
            .iter()
            .map(|row| Ok((row.try_get(0)?, row.try_get(1)?)))
            .collect::<Result<Vec<_>, StoreError>>()?;
        child_records(rows)
    }

    async fn sessions(&self, run: &RunId) -> Result<Vec<SessionRecord>, StoreError> {
        let d = self.d();
        let rows = self
            .fetch(
                format!(
                    "SELECT name, model, status, turns, input_tokens, output_tokens, error, {}, {}, cache_read_tokens, cache_write_tokens, role, reasoning_tokens
                     FROM sessions WHERE run_id = $1 ORDER BY name",
                    d.out("started_at"),
                    d.out("finished_at")
                ),
                |q| q.bind(run.as_str()),
            )
            .await?;
        rows.iter()
            .map(|row| {
                Ok(SessionRecord {
                    name: row.try_get(0)?,
                    role: session_role(row.try_get(11)?)?,
                    model: row.try_get(1)?,
                    status: session_status(row.try_get(2)?)?,
                    turns: to_u64("sessions.turns", row.try_get(3)?)?,
                    input_tokens: to_u64("sessions.input_tokens", row.try_get(4)?)?,
                    output_tokens: to_u64("sessions.output_tokens", row.try_get(5)?)?,
                    cache_read_tokens: to_u64("sessions.cache_read_tokens", row.try_get(9)?)?,
                    cache_write_tokens: to_u64("sessions.cache_write_tokens", row.try_get(10)?)?,
                    reasoning_tokens: to_u64("sessions.reasoning_tokens", row.try_get(12)?)?,
                    error: row.try_get(6)?,
                    started_at: row.try_get(7)?,
                    finished_at: row.try_get(8)?,
                })
            })
            .collect()
    }

    async fn findings(&self, run: &RunId) -> Result<Vec<FindingRecord>, StoreError> {
        let rows = self
            .fetch(
                format!(
                    "SELECT {}, session, path, line, comment_id, action FROM findings WHERE run_id = $1 ORDER BY id",
                    self.d().out("created_at")
                ),
                |q| q.bind(run.as_str()),
            )
            .await?;
        rows.iter()
            .map(|row| {
                Ok(FindingRecord {
                    at: row.try_get(0)?,
                    session: row.try_get(1)?,
                    path: row.try_get(2)?,
                    line: to_u32("findings.line", row.try_get(3)?)?,
                    comment_id: row.try_get(4)?,
                    action: row.try_get(5)?,
                })
            })
            .collect()
    }

    async fn drafts(&self, run: &RunId) -> Result<Vec<DraftRecord>, StoreError> {
        let d = self.d();
        let rows = self
            .fetch(
                format!(
                    "SELECT {}, draft, session, model, kind, path, line, target, body, verdict, checker, reason, same_as, comment_id, {}
                     FROM drafts WHERE run_id = $1 ORDER BY id",
                    d.out("created_at"),
                    d.out("decided_at")
                ),
                |q| q.bind(run.as_str()),
            )
            .await?;
        rows.iter()
            .map(|row| {
                let verdict: Option<String> = row.try_get(9)?;
                let decision = match verdict {
                    Some(verdict) => Some(DraftDecision {
                        at: row.try_get::<Option<String>, _>(14)?.unwrap_or_default(),
                        verdict: draft_verdict(&verdict)?,
                        checker: row.try_get(10)?,
                        reason: row.try_get(11)?,
                        same_as: row.try_get(12)?,
                        comment_id: row.try_get(13)?,
                    }),
                    None => None,
                };
                Ok(DraftRecord {
                    at: row.try_get(0)?,
                    draft: row.try_get(1)?,
                    session: row.try_get(2)?,
                    model: row.try_get(3)?,
                    kind: row.try_get(4)?,
                    path: row.try_get(5)?,
                    line: to_u32("drafts.line", row.try_get(6)?)?,
                    target: row.try_get(7)?,
                    body: row.try_get(8)?,
                    decision,
                })
            })
            .collect()
    }

    async fn record_tool_call(&self, run: &RunId, call: &ToolCallRecord) -> Result<(), StoreError> {
        let d = self.d();
        let at = d.stored("tool_calls.at", &call.at)?;
        let number = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
        self.exec(
            format!(
                "INSERT INTO tool_calls (run_id, session, model, turn, tool, origin, outcome, arguments, arguments_len, result_chars, elapsed_ms, at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, {})",
                d.ts(12)
            ),
            |q| {
                q.bind(run.as_str())
                    .bind(call.session.as_str())
                    .bind(call.model.as_str())
                    .bind(i64::from(call.turn))
                    .bind(call.tool.as_str())
                    .bind(call.origin.as_str())
                    .bind(call.outcome.as_str())
                    .bind(call.arguments.as_str())
                    .bind(number(call.arguments_len))
                    .bind(number(call.result_chars))
                    .bind(number(call.elapsed_ms))
                    .bind(at.as_str())
            },
        )
        .await
    }

    async fn tool_calls(&self, run: &RunId) -> Result<Vec<ToolCallRecord>, StoreError> {
        let number = |n: i64| u64::try_from(n).unwrap_or_default();
        let rows = self
            .fetch(
                format!(
                    "SELECT {}, session, model, turn, tool, origin, outcome, arguments, arguments_len, result_chars, elapsed_ms
                     FROM tool_calls WHERE run_id = $1 ORDER BY id",
                    self.d().out("at")
                ),
                |q| q.bind(run.as_str()),
            )
            .await?;
        rows.iter()
            .map(|row| {
                Ok(ToolCallRecord {
                    at: row.try_get(0)?,
                    session: row.try_get(1)?,
                    model: row.try_get(2)?,
                    turn: to_u32("tool_calls.turn", row.try_get(3)?)?,
                    tool: row.try_get(4)?,
                    origin: row.try_get(5)?,
                    outcome: row.try_get(6)?,
                    arguments: row.try_get(7)?,
                    arguments_len: number(row.try_get(8)?),
                    result_chars: number(row.try_get(9)?),
                    elapsed_ms: number(row.try_get(10)?),
                })
            })
            .collect()
    }

    async fn tool_usage(&self, filter: &ToolCallFilter) -> Result<Vec<ToolUsage>, StoreError> {
        let d = self.d();
        let (since, until) = call_window(d, filter)?;
        let number = |n: i64| u64::try_from(n).unwrap_or_default();
        let rows = self
            .fetch(
                format!(
                    "SELECT c.model, {CALL_ROLE}, {CALL_TOOL}, c.outcome, COUNT(*),
                            CAST(COALESCE(SUM(c.elapsed_ms), 0) AS BIGINT)
                     FROM tool_calls c {CALL_SESSION} WHERE {}
                     GROUP BY c.model, 2, 3, c.outcome",
                    call_filter(d)
                ),
                |q| {
                    q.bind(filter.tool.as_deref())
                        .bind(filter.model.as_deref())
                        .bind(filter.role.map(SessionRole::as_str))
                        .bind(since.as_deref())
                        .bind(until.as_deref())
                },
            )
            .await?;
        let rows = rows
            .iter()
            .map(|row| {
                Ok((
                    row.try_get(0)?,
                    row.try_get(1)?,
                    row.try_get(2)?,
                    row.try_get(3)?,
                    number(row.try_get(4)?),
                    number(row.try_get(5)?),
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
        let d = self.d();
        let (since, until) = call_window(d, filter)?;
        let outcome = OutcomeFilter::param(filter.outcome.as_ref());
        let before_at = filter
            .before
            .as_ref()
            .map(|k| d.bound("before", &k.at))
            .transpose()?;
        let before_id = filter.before.as_ref().map(|k| k.id);
        let at = d.instant("c.at");
        let rows = self
            .fetch(
                format!(
                    "SELECT c.id, c.run_id, r.repo, r.target, r.platform, {}, c.session, c.model,
                            c.turn, c.tool, c.origin, c.outcome, c.arguments, c.arguments_len,
                            c.result_chars, c.elapsed_ms,
                            CASE WHEN EXISTS (SELECT 1 FROM transcripts t
                                              WHERE t.run_id = c.run_id AND t.session = c.session)
                                 THEN 1 ELSE 0 END,
                            r.kind
                     FROM tool_calls c JOIN runs r ON r.id = c.run_id {CALL_SESSION}
                     WHERE {}
                       AND ($6 IS NULL OR ($6 = 'problems' AND c.outcome <> 'ok') OR c.outcome = $6)
                       AND ($7 IS NULL OR {at} < {t7} OR ({at} = {t7} AND c.id < $8))
                     ORDER BY {at} DESC, c.id DESC LIMIT $9 OFFSET $10",
                    d.out("c.at"),
                    call_filter(d),
                    t7 = d.ts(7),
                ),
                |q| {
                    q.bind(filter.tool.as_deref())
                        .bind(filter.model.as_deref())
                        .bind(filter.role.map(SessionRole::as_str))
                        .bind(since.as_deref())
                        .bind(until.as_deref())
                        .bind(outcome)
                        .bind(before_at.as_deref())
                        .bind(before_id)
                        .bind(i64::from(page.limit()))
                        .bind(i64::from(page.offset()))
                },
            )
            .await?;
        let number = |n: i64| u64::try_from(n).unwrap_or_default();
        rows.iter()
            .map(|row| {
                let transcript_kept: i64 = row.try_get(16)?;
                Ok(ToolCallListing {
                    id: row.try_get(0)?,
                    run_id: row.try_get(1)?,
                    repo: row.try_get(2)?,
                    target: to_u64("runs.target", row.try_get(3)?)?,
                    platform: run_platform(row.try_get(4)?)?,
                    kind: run_kind(row.try_get(17)?)?,
                    call: ToolCallRecord {
                        at: row.try_get(5)?,
                        session: row.try_get(6)?,
                        model: row.try_get(7)?,
                        turn: to_u32("tool_calls.turn", row.try_get(8)?)?,
                        tool: row.try_get(9)?,
                        origin: row.try_get(10)?,
                        outcome: row.try_get(11)?,
                        arguments: row.try_get(12)?,
                        arguments_len: number(row.try_get(13)?),
                        result_chars: number(row.try_get(14)?),
                        elapsed_ms: number(row.try_get(15)?),
                    },
                    transcript_kept: transcript_kept != 0,
                })
            })
            .collect()
    }

    async fn rounds(&self, run: &RunId) -> Result<Vec<RoundRecord>, StoreError> {
        let d = self.d();
        let rows = self
            .fetch(
                format!(
                    "SELECT session, role, round, {}, {}, first_turn, turns, handover, handover_len
                     FROM session_rounds WHERE run_id = $1
                     ORDER BY {}, session, round",
                    d.out("started_at"),
                    d.out("ended_at"),
                    d.instant("started_at")
                ),
                |q| q.bind(run.as_str()),
            )
            .await?;
        rows.iter()
            .map(|row| {
                round_record((
                    row.try_get(0)?,
                    row.try_get(1)?,
                    row.try_get(2)?,
                    row.try_get(3)?,
                    row.try_get(4)?,
                    row.try_get(5)?,
                    row.try_get(6)?,
                    row.try_get(7)?,
                    row.try_get(8)?,
                ))
            })
            .collect()
    }

    async fn transcripts(&self, run: &RunId) -> Result<Vec<TranscriptSummary>, StoreError> {
        let rows = self
            .fetch(
                format!(
                    "SELECT {}, session, model, stop, turns, bytes FROM transcripts WHERE run_id = $1 ORDER BY id",
                    self.d().out("recorded_at")
                ),
                |q| q.bind(run.as_str()),
            )
            .await?;
        rows.iter()
            .map(|row| {
                Ok(TranscriptSummary {
                    at: row.try_get(0)?,
                    session: row.try_get(1)?,
                    model: row.try_get(2)?,
                    stop: row.try_get(3)?,
                    turns: to_u32("transcripts.turns", row.try_get(4)?)?,
                    bytes: u64::try_from(row.try_get::<i64, _>(5)?).unwrap_or_default(),
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
            .fetch(
                format!(
                    "SELECT {}, session, model, stop, turns, bytes, body FROM transcripts
                     WHERE run_id = $1 AND session = $2 ORDER BY id DESC LIMIT 1",
                    self.d().out("recorded_at")
                ),
                |q| q.bind(run.as_str()).bind(session),
            )
            .await?;
        rows.first()
            .map(|row| {
                Ok(TranscriptRecord {
                    at: row.try_get(0)?,
                    session: row.try_get(1)?,
                    model: row.try_get(2)?,
                    stop: row.try_get(3)?,
                    turns: to_u32("transcripts.turns", row.try_get(4)?)?,
                    bytes: u64::try_from(row.try_get::<i64, _>(5)?).unwrap_or_default(),
                    body: row.try_get(6)?,
                })
            })
            .transpose()
    }

    async fn event(&self, run: &RunId, level: &str, message: &str) -> Result<(), StoreError> {
        let at = now();
        self.exec(
            format!(
                "INSERT INTO events (run_id, at, level, message) VALUES ($1, {}, $3, $4)",
                self.d().ts(2)
            ),
            |q| q.bind(run.as_str()).bind(at.as_str()).bind(level).bind(message),
        )
        .await
    }

    async fn events(&self, run: &RunId) -> Result<Vec<EventRecord>, StoreError> {
        let rows = self
            .fetch(
                format!(
                    "SELECT {}, level, message FROM events WHERE run_id = $1 ORDER BY id",
                    self.d().out("at")
                ),
                |q| q.bind(run.as_str()),
            )
            .await?;
        rows.iter()
            .map(|row| {
                Ok(EventRecord {
                    at: row.try_get(0)?,
                    level: row.try_get(1)?,
                    message: row.try_get(2)?,
                })
            })
            .collect()
    }

    async fn inbound_event(&self, id: &EventId) -> Result<Option<InboundEvent>, StoreError> {
        let rows = self
            .fetch(
                format!(
                    "SELECT {} FROM inbound_events WHERE id = $1",
                    inbound_columns(self.d(), "")
                ),
                |q| q.bind(id.as_str()),
            )
            .await?;
        rows.first().map(inbound_row).transpose()
    }

    async fn event_facets(&self) -> Result<EventFacets, StoreError> {
        let mut facets = EventFacets::default();
        for (column, values) in [("source", &mut facets.sources), ("kind", &mut facets.kinds)] {
            *values = self
                .fetch(
                    format!("SELECT DISTINCT {column} FROM inbound_events ORDER BY 1"),
                    |q| q,
                )
                .await?
                .iter()
                .map(|row| row.try_get(0))
                .collect::<Result<Vec<String>, _>>()?;
        }
        Ok(facets)
    }

    async fn outcomes(&self, id: &EventId) -> Result<Vec<OutcomeRecord>, StoreError> {
        let d = self.d();
        let rows = self
            .fetch(
                format!(
                    "SELECT listener, outcome, detail, run_id, {} FROM event_outcomes
                     WHERE event_id = $1 ORDER BY {}",
                    d.out("at"),
                    d.recorded()
                ),
                |q| q.bind(id.as_str()),
            )
            .await?;
        rows.iter()
            .map(|row| {
                Ok(OutcomeRecord {
                    event_id: id.clone(),
                    listener: row.try_get(0)?,
                    outcome: row.try_get(1)?,
                    detail: row.try_get(2)?,
                    run_id: row.try_get(3)?,
                    at: row.try_get(4)?,
                })
            })
            .collect()
    }

    async fn prune_events(&self, older_than: OffsetDateTime) -> Result<PruneCounts, StoreError> {
        let d = self.d();
        let cutoff = rfc3339(older_than);
        let mut transaction = self.writer.begin().await?;
        let outcomes = delete(
            &mut transaction,
            format!(
                "DELETE FROM event_outcomes WHERE event_id IN
                 (SELECT id FROM inbound_events WHERE received_at < {})",
                d.ts(1)
            ),
            &cutoff,
        )
        .await?;
        let events = delete(
            &mut transaction,
            format!("DELETE FROM inbound_events WHERE received_at < {}", d.ts(1)),
            &cutoff,
        )
        .await?;
        let transcripts = delete(
            &mut transaction,
            format!("DELETE FROM transcripts WHERE recorded_at < {}", d.ts(1)),
            &cutoff,
        )
        .await?;
        transaction.commit().await?;
        Ok(PruneCounts {
            events,
            outcomes,
            transcripts,
        })
    }

    async fn inbound_events_for_run(&self, run: &RunId) -> Result<Vec<InboundEvent>, StoreError> {
        let d = self.d();
        let rows = self
            .fetch(
                format!(
                    "SELECT {} FROM inbound_events e
                     WHERE e.id IN (SELECT o.event_id FROM event_outcomes o WHERE o.run_id = $1)
                     ORDER BY {}, e.id",
                    inbound_columns(d, "e."),
                    d.instant("e.received_at")
                ),
                |q| q.bind(run.as_str()),
            )
            .await?;
        rows.iter().map(inbound_row).collect()
    }

    async fn list_runs(
        &self,
        filter: &RunFilter,
        page: Page,
    ) -> Result<Vec<RunRecord>, StoreError> {
        let d = self.d();
        let binds = RunBinds::of(d, filter)?;
        let rows = self
            .fetch(
                format!(
                    "SELECT {} FROM runs WHERE {}
                     ORDER BY {} DESC, id DESC LIMIT $10 OFFSET $11",
                    run_columns(d, ""),
                    run_filter(d),
                    d.instant("started_at")
                ),
                |q| {
                    binds
                        .bind(q)
                        .bind(i64::from(page.limit()))
                        .bind(i64::from(page.offset()))
                },
            )
            .await?;
        runs_of(&rows)
    }

    async fn daily_stats(&self, since: OffsetDateTime) -> Result<Vec<DayCounts>, StoreError> {
        let d = self.d();
        let since = d.bound_at(since);
        let runs = self
            .fetch(
                format!(
                    "SELECT {day}, COUNT(*),
                            COUNT(CASE WHEN status = 'finished' THEN 1 END),
                            COUNT(CASE WHEN status = 'failed' THEN 1 END)
                     FROM runs WHERE {at} >= {t1} GROUP BY 1",
                    day = d.day("started_at"),
                    at = d.instant("started_at"),
                    t1 = d.ts(1)
                ),
                |q| q.bind(since.as_str()),
            )
            .await?
            .iter()
            .map(|row| {
                Ok((
                    row.try_get(0)?,
                    row.try_get(1)?,
                    row.try_get(2)?,
                    row.try_get(3)?,
                ))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        let findings = self
            .fetch(
                format!(
                    "SELECT {day}, COUNT(*) FROM findings
                     WHERE action IN ('posted', 'unverified') AND {at} >= {t1} GROUP BY 1",
                    day = d.day("created_at"),
                    at = d.instant("created_at"),
                    t1 = d.ts(1)
                ),
                |q| q.bind(since.as_str()),
            )
            .await?
            .iter()
            .map(|row| Ok((row.try_get(0)?, row.try_get(1)?)))
            .collect::<Result<Vec<_>, StoreError>>()?;
        let drafts = self
            .fetch(
                format!(
                    "SELECT {day}, COUNT(*) FROM drafts
                     WHERE {at} >= {t1}
                       AND kind NOT IN ('loop_finding', 'plan_point', 'requirement_point')
                     GROUP BY 1",
                    day = d.day("created_at"),
                    at = d.instant("created_at"),
                    t1 = d.ts(1)
                ),
                |q| q.bind(since.as_str()),
            )
            .await?
            .iter()
            .map(|row| Ok((row.try_get(0)?, row.try_get(1)?)))
            .collect::<Result<Vec<_>, StoreError>>()?;
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
            .fetch(
                format!(
                    "SELECT run_id, name, role, status FROM sessions
                     WHERE run_id IN ({}) ORDER BY run_id, name",
                    placeholders(1, runs.len())
                ),
                |q| runs.iter().fold(q, |q, run| q.bind(run.as_str())),
            )
            .await?;
        let rows = rows
            .iter()
            .map(|row| {
                Ok((
                    row.try_get(0)?,
                    row.try_get(1)?,
                    row.try_get(2)?,
                    row.try_get(3)?,
                ))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        session_dots(rows)
    }

    async fn session_endings(
        &self,
        since: Option<OffsetDateTime>,
        reviews: u32,
    ) -> Result<Vec<SessionEnding>, StoreError> {
        let d = self.d();
        let since = since.map(|at| d.bound_at(at));
        let reviews = i64::from(reviews.min(MOST_SESSION_REVIEWS));
        let at = d.instant("started_at");
        let rows = self
            .fetch(
                format!(
                    "SELECT r.id, {}, s.name, s.role, s.model, s.status, s.error
                     FROM (SELECT id, started_at, {at} AS at FROM runs
                           WHERE kind = 'review' AND status NOT IN ('running', 'superseded', 'cancelled')
                             AND ($1 IS NULL OR {at} >= {t1})
                           ORDER BY at DESC, id DESC LIMIT $2) r
                     JOIN sessions s ON s.run_id = r.id
                     ORDER BY r.at DESC, r.id DESC, s.name",
                    d.out("r.started_at"),
                    t1 = d.ts(1)
                ),
                |q| q.bind(since.as_deref()).bind(reviews),
            )
            .await?;
        let rows = rows
            .iter()
            .map(|row| {
                Ok((
                    row.try_get(0)?,
                    row.try_get(1)?,
                    row.try_get(2)?,
                    row.try_get(3)?,
                    row.try_get(4)?,
                    row.try_get(5)?,
                    row.try_get(6)?,
                ))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        session_endings(rows)
    }

    async fn stages_of(
        &self,
        runs: &[RunId],
    ) -> Result<Vec<(RunId, Stage, StageState)>, StoreError> {
        if runs.is_empty() {
            return Ok(Vec::new());
        }
        let rows = self
            .fetch(
                format!(
                    "SELECT run_id, stage, state FROM stages WHERE run_id IN ({})",
                    placeholders(1, runs.len())
                ),
                |q| runs.iter().fold(q, |q, run| q.bind(run.as_str())),
            )
            .await?;
        let rows = rows
            .iter()
            .map(|row| Ok((row.try_get(0)?, row.try_get(1)?, row.try_get(2)?)))
            .collect::<Result<Vec<_>, StoreError>>()?;
        stage_dots(rows)
    }

    async fn count_runs(&self, filter: &RunFilter) -> Result<u64, StoreError> {
        let d = self.d();
        let binds = RunBinds::of(d, filter)?;
        let rows = self
            .fetch(
                format!("SELECT COUNT(*) FROM runs WHERE {}", run_filter(d)),
                |q| binds.bind(q),
            )
            .await?;
        let count: i64 = rows
            .first()
            .map_or(Ok(0), |row| row.try_get(0))?;
        Ok(u64::try_from(count).unwrap_or_default())
    }

    async fn list_inbound_events(
        &self,
        filter: &EventFilter,
        page: Page,
    ) -> Result<Vec<EventWithOutcomes>, StoreError> {
        let d = self.d();
        let before_at = filter
            .before
            .as_ref()
            .map(|k| d.bound("received_at", &k.received_at))
            .transpose()?;
        let before_id = filter.before.as_ref().map(|k| k.id.as_str());
        let at = d.instant("received_at");
        let rows = self
            .fetch(
                format!(
                    "SELECT {} FROM inbound_events
                     WHERE ($1 IS NULL OR source = $1) AND ($2 IS NULL OR kind = $2)
                       AND ($3 IS NULL OR repo = $3)
                       AND ($4 IS NULL OR {at} < {t4} OR ({at} = {t4} AND id < $5))
                     ORDER BY {at} DESC, id DESC LIMIT $6 OFFSET $7",
                    inbound_columns(d, ""),
                    t4 = d.ts(4)
                ),
                |q| {
                    q.bind(filter.source.as_deref())
                        .bind(filter.kind.as_deref())
                        .bind(filter.repo.as_deref())
                        .bind(before_at.as_deref())
                        .bind(before_id)
                        .bind(i64::from(page.limit()))
                        .bind(i64::from(page.offset()))
                },
            )
            .await?;
        let events = rows.iter().map(inbound_row).collect::<Result<Vec<_>, _>>()?;
        if events.is_empty() {
            return Ok(Vec::new());
        }
        // One query for every outcome of the page, not one per event.
        let rows = self
            .fetch(
                format!(
                    "SELECT event_id, listener, outcome, detail, run_id, {} FROM event_outcomes
                     WHERE event_id IN ({}) ORDER BY {}",
                    d.out("at"),
                    placeholders(1, events.len()),
                    d.recorded()
                ),
                |q| events.iter().fold(q, |q, event| q.bind(event.id.as_str())),
            )
            .await?;
        let outcomes = rows
            .iter()
            .map(|row| {
                Ok(OutcomeRow {
                    event_id: row.try_get(0)?,
                    listener: row.try_get(1)?,
                    outcome: row.try_get(2)?,
                    detail: row.try_get(3)?,
                    run_id: row.try_get(4)?,
                    at: row.try_get(5)?,
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        Ok(attach_outcomes(events, &outcomes))
    }
}
unported {
    finish_run(_id: &RunId, _status: RunStatus, _summary: Option<&str>, _error: Option<&str>) -> Result<(), StoreError>;
    supersede_run(_id: &RunId, _by: &RunId, _reason: &str) -> Result<(), StoreError>;
    heartbeat(_id: &RunId) -> Result<(), StoreError>;
    end_loop(_id: &RunId, _stop: &str, _rounds: u32) -> Result<(), StoreError>;
    set_check(_id: &RunId, _check_id: &str) -> Result<(), StoreError>;
    claim_resume(_run: &RunId, _interrupted: &str, _resumed: &str) -> Result<bool, StoreError>;
    drop_running_sessions(_run: &RunId, _reason: &str) -> Result<(), StoreError>;
    stage(_run: &RunId, _write: &StageWrite) -> Result<(), StoreError>;
    fail_running_stages(_run: &RunId, _reason: &str) -> Result<(), StoreError>;
    start_session(_run: &RunId, _name: &str, _role: SessionRole, _model: &str) -> Result<(), StoreError>;
    finish_session(_run: &RunId, _name: &str, _status: SessionStatus, _turns: u64, _tokens: TokenUsage, _error: Option<&str>) -> Result<(), StoreError>;
    record_finding(_run: &RunId, _session: &str, _path: &str, _line_number: u32, _comment_id: &str, _action: FindingAction) -> Result<(), StoreError>;
    record_draft(_run: &RunId, _draft: &DraftRecord) -> Result<(), StoreError>;
    decide_draft(_run: &RunId, _draft: &str, _decision: &DraftDecision) -> Result<(), StoreError>;
    start_round(_run: &RunId, _round: &RoundRecord) -> Result<(), StoreError>;
    end_round(_run: &RunId, _session: &str, _round: u32, _turns: u32) -> Result<(), StoreError>;
    record_transcript(_run: &RunId, _transcript: &TranscriptRecord) -> Result<(), StoreError>;
    joined(_run: &RunId, _source: &str) -> Result<(), StoreError>;
    record_event(_event: &InboundEvent) -> Result<(), StoreError>;
    record_outcome(_outcome: &OutcomeRecord) -> Result<(), StoreError>;
}
}

/// Deletes the rows of `sql` older than `cutoff` and says how many.
async fn delete(
    transaction: &mut sqlx::Transaction<'_, Any>,
    sql: String,
    cutoff: &str,
) -> Result<u64, StoreError> {
    Ok(sqlx::query(AssertSqlSafe(sql))
        .bind(cutoff)
        .execute(&mut **transaction)
        .await?
        .rows_affected())
}

/// The time bounds of a tool call tally or listing, bound for
/// [`call_filter`].
fn call_window(
    d: Dialect,
    filter: &ToolCallFilter,
) -> Result<(Option<String>, Option<String>), StoreError> {
    let time = |column, value: &Option<String>| {
        value.as_deref().map(|v| d.bound(column, v)).transpose()
    };
    Ok((time("since", &filter.since)?, time("until", &filter.until)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_step_runs_its_both_part_then_its_own() {
        let text = "-- header\n-- @sqlite\nA;\n-- @both\nB;\n-- @postgres\nC;\n";
        assert_eq!(step_sql(text, Dialect::Sqlite).as_deref(), Some("B;\nA;\n"));
        assert_eq!(step_sql(text, Dialect::Postgres).as_deref(), Some("B;\nC;\n"));
        assert_eq!(step_sql("-- @sqlite\nA;\n", Dialect::Postgres), None);
    }

    #[test]
    fn the_steps_are_one_numbered_sequence() {
        let versions: Vec<i64> = STEPS.iter().map(|&(version, _, _)| version).collect();
        assert_eq!(versions, [1, 2, 3]);
        assert_eq!(migrator(Dialect::Sqlite).iter().count(), 3);
        assert_eq!(migrator(Dialect::Postgres).iter().count(), 2, "no step 2");
    }
}
