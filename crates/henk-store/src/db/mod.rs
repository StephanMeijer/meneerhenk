//! The run store on sea-orm (#598): one implementation for SQLite and
//! `PostgreSQL`, written once. The query builder (`sea-query`, re-exported by
//! sea-orm) makes the SQL; the connection picks the dialect, the placeholders
//! and the pool. There are no entities: the tables are named by strings and
//! rows are read by position, like the two stores this replaces.
//!
//! What the builder cannot say is written as raw SQL and marked `RAW`, and
//! what differs between the databases is in [`dialect`], not in the methods.
//!
//! The methods move here from `sqlite.rs` and `postgres.rs` in groups (#605
//! the runs and stages, #606 and #607 the rest). Until the last group is
//! here, `SqliteStore` and `PgStore` implement the trait and hand the moved
//! methods to a [`Store`] that opens the same database.

mod calls;
mod dialect;
mod findings;
mod inbound;
mod runs;
mod sessions;
mod stages;
mod stats;
mod timeline;

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sea_orm::sea_query::SelectStatement;
use sea_orm::{
    ConnectOptions, ConnectionTrait, Database, DatabaseConnection, DbErr, QueryResult,
    SqlxSqliteConnector, StatementBuilder, TryGetable,
};
use sqlx::ConnectOptions as _;
use sqlx::postgres::{PgConnectOptions, PgSslMode};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use time::OffsetDateTime;
use tokio::sync::OnceCell;

use crate::migrator;
use crate::types::StoreError;

use dialect::{Dialect, rfc3339};

/// Connections the pool keeps at most. Henk's writes are small and few.
const POOL_SIZE: u32 = 8;

/// How long a statement waits for SQLite's write lock before it fails.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a new connection may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Where the database is. A `PostgreSQL` URL holds a password, so it is
/// never printed.
enum Source {
    Sqlite(PathBuf),
    Postgres(String),
}

impl fmt::Debug for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(path) => f.debug_tuple("Sqlite").field(path).finish(),
            Self::Postgres(_) => f.write_str("Postgres(..)"),
        }
    }
}

/// The run store on sea-orm. Opening is lazy: the constructors need no
/// runtime, and the first query connects, sets the connection up and brings
/// the schema up to date (see [`migrator::migrate`]). The pool belongs to the
/// runtime that first uses it.
#[derive(Debug)]
pub(crate) struct Store {
    source: Source,
    /// Whether the first use migrates. Off only for a database that is
    /// deliberately left at an old schema (the `testing` constructors).
    migrate: bool,
    db: OnceCell<DatabaseConnection>,
}

impl Store {
    /// The store over the SQLite file at `path`, created if it is missing.
    pub(crate) fn sqlite(path: &Path) -> Self {
        Self {
            source: Source::Sqlite(path.to_owned()),
            migrate: true,
            db: OnceCell::new(),
        }
    }

    /// Connects to the `PostgreSQL` database at `url` now, so a bad URL, a
    /// server that cannot be reached or a failed migration is an error here
    /// and not at the first query. The URL never appears in an error.
    pub(crate) async fn postgres(url: &str, migrate: bool) -> Result<Self, StoreError> {
        let store = Self {
            source: Source::Postgres(url.to_owned()),
            migrate,
            db: OnceCell::new(),
        };
        store.connection().await?;
        Ok(store)
    }

    /// The same store, left at whatever schema its database has.
    #[cfg(feature = "testing")]
    pub(crate) fn unmigrated(mut self) -> Self {
        self.migrate = false;
        self
    }

    /// The connection, opened on first use.
    pub(crate) async fn connection(&self) -> Result<&DatabaseConnection, StoreError> {
        // Boxed: connecting and migrating is a large future, and it would
        // otherwise be part of every future that can reach the first query.
        self.db.get_or_try_init(|| Box::pin(self.open())).await
    }

    async fn open(&self) -> Result<DatabaseConnection, StoreError> {
        let db = match &self.source {
            Source::Sqlite(path) => connect_sqlite(path).await?,
            Source::Postgres(url) => connect_postgres(url).await?,
        };
        if self.migrate {
            migrator::migrate(&db)
                .await
                .map_err(|error| StoreError::Migration(error.to_string()))?;
        }
        Ok(db)
    }

    /// The dialect of the database, which opens it if it is not yet.
    async fn dialect(&self) -> Result<Dialect, StoreError> {
        Ok(self.connection().await?.get_database_backend().into())
    }

    // --- running statements ----------------------------------------------------

    async fn all(&self, query: &SelectStatement) -> Result<Vec<QueryResult>, StoreError> {
        Ok(self.connection().await?.query_all(query).await?)
    }

    async fn first(&self, query: &SelectStatement) -> Result<Option<QueryResult>, StoreError> {
        Ok(self.connection().await?.query_one(query).await?)
    }

    /// Runs an insert, update or delete; the number of rows it changed.
    async fn exec<S: StatementBuilder>(&self, statement: &S) -> Result<u64, StoreError> {
        Ok(self
            .connection()
            .await?
            .execute(statement)
            .await?
            .rows_affected())
    }
}

/// SQLite through sqlx's pool: WAL, foreign keys and a wait for the write
/// lock on every connection, so concurrent writers queue instead of failing.
/// The path goes in as a path, not as a URL, so any file name works.
async fn connect_sqlite(path: &Path) -> Result<DatabaseConnection, StoreError> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(BUSY_TIMEOUT)
        .disable_statement_logging();
    let pool = SqlitePoolOptions::new()
        .max_connections(POOL_SIZE)
        .connect_with(options)
        .await
        .map_err(|error| StoreError::Connect(error.to_string()))?;
    Ok(SqlxSqliteConnector::from_sqlx_sqlite_pool(pool))
}

async fn connect_postgres(url: &str) -> Result<DatabaseConnection, StoreError> {
    // A URL that does not parse is left to the connection, which reports it
    // without quoting the URL.
    if let Ok(parsed) = url.parse::<PgConnectOptions>() {
        refuse_unverified_tls(&parsed)?;
    }
    let mut options = ConnectOptions::new(url);
    options
        .max_connections(POOL_SIZE)
        .connect_timeout(CONNECT_TIMEOUT)
        .sqlx_logging(false)
        .map_sqlx_postgres_opts(harden_tls);
    Database::connect(options)
        .await
        .map_err(|error| StoreError::Connect(error.to_string()))
}

/// Keeps the guarantee the store has always given: once TLS is asked for, the
/// server's certificate is checked against the platform's roots and the host
/// name. sqlx reads `sslmode` the way `psql` does, where `require` only
/// encrypts and `verify-ca` skips the name; both are raised to `verify-full`.
/// `disable` and `verify-full` stay as they are. `allow` and `prefer` (which
/// is also the default) are refused first, see [`refuse_unverified_tls`].
fn harden_tls(options: PgConnectOptions) -> PgConnectOptions {
    match options.get_ssl_mode() {
        PgSslMode::Require | PgSslMode::VerifyCa => options.ssl_mode(PgSslMode::VerifyFull),
        _ => options,
    }
}

/// Refuses the modes that sqlx cannot make safe. Under `prefer` sqlx uses TLS
/// when the server offers it but accepts any certificate and any host name,
/// and `allow` sends plaintext. The old driver checked the certificate under
/// `prefer`, so letting these through would be a silent downgrade for every
/// URL without an `sslmode`. The caller has to say what it wants: `disable`
/// for a server without TLS, or a mode that verifies.
fn refuse_unverified_tls(options: &PgConnectOptions) -> Result<(), StoreError> {
    match options.get_ssl_mode() {
        PgSslMode::Allow | PgSslMode::Prefer => Err(StoreError::Connect(
            "the database URL needs an explicit sslmode: `verify-full` (also `require` and \
             `verify-ca`, which mean the same here) checks the server certificate, \
             `disable` turns TLS off; `prefer`, `allow` and no sslmode are not accepted"
                .to_owned(),
        )),
        _ => Ok(()),
    }
}

// --- reading values -------------------------------------------------------------

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

impl From<DbErr> for StoreError {
    fn from(error: DbErr) -> Self {
        Self::Database(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::atomic::{AtomicU64, Ordering};

    use henk_domain::allowlist::Platform;
    use henk_domain::run::RunKind;
    use sea_orm::DbBackend;

    use time::format_description::well_known::Rfc3339;

    use super::runs::list_runs_query;
    use super::*;
    use crate::types::{EventFilter, Page, RunFilter, RunKey, RunStatus};

    fn mode(url: &str) -> PgSslMode {
        harden_tls(url.parse().unwrap()).get_ssl_mode()
    }

    #[test]
    fn require_and_verify_ca_are_raised_to_verify_full() {
        for sslmode in ["require", "verify-ca", "verify-full"] {
            assert!(
                matches!(
                    mode(&format!("postgres://u@h/d?sslmode={sslmode}")),
                    PgSslMode::VerifyFull
                ),
                "{sslmode}"
            );
        }
    }

    #[test]
    fn disable_is_left_alone() {
        assert!(matches!(
            mode("postgres://u@h/d?sslmode=disable"),
            PgSslMode::Disable
        ));
    }

    #[test]
    fn prefer_allow_and_no_sslmode_are_refused() {
        for url in [
            "postgres://u@h/d",
            "postgres://u@h/d?sslmode=prefer",
            "postgres://u@h/d?sslmode=allow",
        ] {
            let error = refuse_unverified_tls(&url.parse().unwrap()).unwrap_err();
            assert!(error.to_string().contains("sslmode"), "{url}");
        }
        for sslmode in ["disable", "require", "verify-ca", "verify-full"] {
            let url = format!("postgres://u@h/d?sslmode={sslmode}");
            assert!(
                refuse_unverified_tls(&url.parse().unwrap()).is_ok(),
                "{sslmode}"
            );
        }
    }

    #[test]
    fn a_postgres_url_is_not_printed() {
        let source = Source::Postgres("postgres://user:hunter2@host/db".to_owned());
        assert!(!format!("{source:?}").contains("hunter2"));
    }

    /// The SQL of a full run listing on each database, without a database:
    /// one builder, `?` placeholders and a `substr` instant on SQLite, `$n`
    /// placeholders and the plain timestamp on `PostgreSQL`.
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
        let sql = |backend: DbBackend| {
            let query = list_runs_query(backend.into(), &filter, Page::new(50, 0)).unwrap();
            backend.build(&query).sql
        };

        let sqlite = sql(DbBackend::Sqlite);
        assert!(
            sqlite.contains("\"kind\" = ? AND \"status\" = ? AND \"platform\" = ?"),
            "{sqlite}"
        );
        assert!(sqlite.contains("substr(started_at, 1, 19)"), "{sqlite}");
        assert!(
            sqlite.contains("ORDER BY substr(started_at, 1, 19)")
                && sqlite.contains("DESC, \"id\" DESC"),
            "{sqlite}"
        );

        let postgres = sql(DbBackend::Postgres);
        assert!(
            postgres.contains("\"kind\" = $1 AND \"status\" = $2 AND \"platform\" = $3"),
            "{postgres}"
        );
        assert!(!postgres.contains("substr("), "{postgres}");
        assert!(
            postgres.contains("(started_at) >= $6") && postgres.contains("(started_at) < $7"),
            "{postgres}"
        );
        assert!(
            postgres.contains("ORDER BY started_at DESC, \"id\" DESC"),
            "{postgres}"
        );
    }

    /// The sessions of the newest reviews and the rounds of a run on each
    /// database, without one: the instant orders the review picked and the
    /// rounds listed, and `since` is only there when it was given.
    #[test]
    fn session_endings_and_rounds_are_one_builder_and_two_dialects() {
        use super::sessions::{rounds_query, session_endings_query};

        let since = Some(OffsetDateTime::parse("2026-10-01T10:00:00Z", &Rfc3339).unwrap());
        let sql = |backend: DbBackend, since| {
            let query = session_endings_query(backend.into(), since, 20).unwrap();
            backend.build(&query).sql
        };

        let sqlite = sql(DbBackend::Sqlite, since);
        assert!(sqlite.contains("substr(started_at, 1, 19)"), "{sqlite}");
        assert!(sqlite.contains("AS \"at\""), "{sqlite}");
        assert!(sqlite.contains("\"kind\" = ?"), "{sqlite}");
        assert!(sqlite.contains("ORDER BY \"r\".\"at\" DESC"), "{sqlite}");

        let postgres = sql(DbBackend::Postgres, since);
        assert!(!postgres.contains("substr("), "{postgres}");
        assert!(postgres.contains("\"kind\" = $1"), "{postgres}");
        assert!(postgres.contains("(started_at) >= $"), "{postgres}");

        let without = sql(DbBackend::Postgres, None);
        assert!(!without.contains(">="), "{without}");

        let run = henk_domain::run::RunId::parse("r-1").unwrap();
        for backend in [DbBackend::Sqlite, DbBackend::Postgres] {
            let sql = backend.build(&rounds_query(backend.into(), &run)).sql;
            assert!(
                sql.ends_with("\"session\" ASC, \"round\" ASC"),
                "{backend:?}: {sql}"
            );
        }
        let sqlite = DbBackend::Sqlite
            .build(&rounds_query(Dialect::Sqlite, &run))
            .sql;
        assert!(
            sqlite.contains("ORDER BY substr(started_at, 1, 19)"),
            "{sqlite}"
        );
    }

    /// Numbers this process's database files, so no two tests share one.
    static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

    /// #216: the unfiltered run listing walks the instant index instead of
    /// sorting, so the expression built here and the index of
    /// `m002_run_instant` stay the same text.
    #[tokio::test]
    async fn the_run_listing_walks_the_instant_index_instead_of_sorting() {
        let path = std::env::temp_dir().join(format!(
            "henk-plan-{}-{}.db",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let store = Store::sqlite(&path);
        let db = store.connection().await.unwrap();
        let query =
            list_runs_query(Dialect::Sqlite, &RunFilter::default(), Page::new(50, 0)).unwrap();
        let statement = DbBackend::Sqlite.build(&query);
        let plan: Vec<String> = db
            .query_all_raw(sea_orm::Statement::from_sql_and_values(
                DbBackend::Sqlite,
                format!("EXPLAIN QUERY PLAN {}", statement.sql),
                statement.values.map_or_else(Vec::new, |values| values.0),
            ))
            .await
            .unwrap()
            .iter()
            .map(|row| row.try_get_by_index::<String>(3).unwrap())
            .collect();
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

    /// The events page on each database, without one: the instant orders the
    /// listing on SQLite, and the day of a time is one raw expression per
    /// database.
    #[test]
    fn the_events_listing_and_the_day_of_a_time_are_one_builder_and_two_dialects() {
        use super::inbound::list_inbound_events_query;

        let sql = |backend: DbBackend| {
            let query = list_inbound_events_query(
                backend.into(),
                &EventFilter::default(),
                Page::new(20, 0),
            )
            .unwrap();
            backend.build(&query).sql
        };
        let sqlite = sql(DbBackend::Sqlite);
        assert!(
            sqlite.contains("ORDER BY substr(received_at, 1, 19)")
                && sqlite.contains("DESC, \"id\" DESC"),
            "{sqlite}"
        );
        let postgres = sql(DbBackend::Postgres);
        assert!(
            postgres.contains("ORDER BY received_at DESC, \"id\" DESC")
                && !postgres.contains("substr("),
            "{postgres}"
        );

        let day = |backend: DbBackend| {
            let mut query = sea_orm::sea_query::Query::select();
            query
                .expr(Dialect::from(backend).day("started_at"))
                .from("runs");
            backend.build(&query).sql
        };
        assert!(day(DbBackend::Sqlite).contains("substr(started_at, 1, 10)"));
        assert!(
            day(DbBackend::Postgres)
                .contains("to_char(started_at AT TIME ZONE 'UTC', 'YYYY-MM-DD')")
        );
    }

    /// #216 for inbound events: the listing walks the instant index
    /// (`m006_inbound_events_instant`) instead of sorting the table, so the
    /// expression built here and the index stay the same text.
    #[tokio::test]
    async fn the_events_listing_walks_the_instant_index_instead_of_sorting() {
        use super::inbound::list_inbound_events_query;

        let path = std::env::temp_dir().join(format!(
            "henk-plan-events-{}-{}.db",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let store = Store::sqlite(&path);
        let db = store.connection().await.unwrap();
        // The planner picks by what it expects of the table: give it one that
        // is not empty, as the page's is.
        db.execute_unprepared(
            "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 2000)
             INSERT INTO inbound_events (id, received_at, source, kind)
             SELECT 'e-' || n, strftime('%Y-%m-%dT%H:%M:%SZ', 1790000000 + n, 'unixepoch'), 'github_webhook', 'push'
             FROM c",
        )
        .await
        .unwrap();
        db.execute_unprepared("ANALYZE").await.unwrap();
        let query =
            list_inbound_events_query(Dialect::Sqlite, &EventFilter::default(), Page::new(50, 0))
                .unwrap();
        let statement = DbBackend::Sqlite.build(&query);
        let plan: Vec<String> = db
            .query_all_raw(sea_orm::Statement::from_sql_and_values(
                DbBackend::Sqlite,
                format!("EXPLAIN QUERY PLAN {}", statement.sql),
                statement.values.map_or_else(Vec::new, |values| values.0),
            ))
            .await
            .unwrap()
            .iter()
            .map(|row| row.try_get_by_index::<String>(3).unwrap())
            .collect();
        let indexes: Vec<String> = db
            .query_all_raw(sea_orm::Statement::from_string(
                DbBackend::Sqlite,
                "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'inbound_events'",
            ))
            .await
            .unwrap()
            .iter()
            .map(|row| row.try_get_by_index::<String>(0).unwrap())
            .collect();
        drop(store);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }

        assert!(
            plan.iter()
                .any(|step| step.contains("USING INDEX inbound_events_received_instant")),
            "{plan:?} {indexes:?} for {}",
            statement.sql
        );
        assert!(
            !plan.iter().any(|step| step.contains("TEMP B-TREE")),
            "{plan:?}"
        );
    }
}
