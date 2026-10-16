//! The migrator (#604): the schema as one sequence of steps, and the adoption
//! of databases the previous migrator made.
//!
//! The old migrations are the reference. They make the database that is adopted
//! (`SqliteStore::open_at`, `PgStore::connect_at`, at their last version, which
//! leaves the new migrator out of it), and what they leave is compared
//! with what the new sequence makes on an empty database: tables, columns in
//! order, types, nullability, defaults, keys, foreign keys and indexes. The
//! only differences are the ones the sequence adds on purpose, and they are
//! named below.
//!
//! SQLite runs in the plain test run. `PostgreSQL` has the same tests,
//! `#[ignore]`d, for a server named by `HENK_TEST_DATABASE_URL`
//! (`cargo test -p henk-store --test migrator -- --ignored`). Nothing in CI
//! starts a server (#598); run them by hand, as `src/migrator/README.md` says.

#![allow(
    clippy::panic,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::too_many_lines
)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use henk_store::migrator::{Migrator, migrate};
use sea_orm::{
    ConnectOptions, ConnectionTrait, Database, DatabaseConnection, DbBackend, DbErr, Statement,
};
use sea_orm_migration::prelude::MigratorTrait as _;

/// Every table the sequence makes, besides the migrator's own.
const TABLES: [&str; 13] = [
    "drafts",
    "event_outcomes",
    "events",
    "findings",
    "inbound_events",
    "requests",
    "run_children",
    "runs",
    "session_rounds",
    "sessions",
    "stages",
    "tool_calls",
    "transcripts",
];

/// The rows of the golden fixtures: every table but `requests`.
const SEED: &str = include_str!("golden/seed.sql");

// --- reading a schema --------------------------------------------------------------

/// Rows of a query whose columns all come out as text (`NULL` as `-`).
async fn rows(db: &DatabaseConnection, sql: &str) -> Vec<Vec<String>> {
    let result = db
        .query_all_raw(Statement::from_string(db.get_database_backend(), sql))
        .await
        .unwrap();
    result
        .iter()
        .map(|row| {
            (0..)
                .map_while(|index| {
                    row.try_get_by_index::<Option<String>>(index)
                        .ok()
                        .map(|value| value.unwrap_or_else(|| "-".to_owned()))
                })
                .collect()
        })
        .collect()
}

async fn texts(db: &DatabaseConnection, sql: &str) -> Vec<String> {
    rows(db, sql)
        .await
        .into_iter()
        .map(|row| row.join(" "))
        .collect()
}

async fn number(db: &DatabaseConnection, sql: &str) -> i64 {
    db.query_one_raw(Statement::from_string(db.get_database_backend(), sql))
        .await
        .unwrap()
        .unwrap()
        .try_get_by_index(0)
        .unwrap()
}

/// What the schema is, one line per fact, in an order that carries meaning
/// (columns in table order). The migrator's table is left out, and so is the
/// old `PostgreSQL` bookkeeping table, which an adopted database keeps and a
/// fresh one never had.
///
/// A SQLite primary key counts as `NOT NULL`. The old files declared
/// `id TEXT PRIMARY KEY` and `INTEGER PRIMARY KEY AUTOINCREMENT` and SQLite
/// reports those as nullable (a historical quirk: only a rowid alias cannot
/// hold `NULL`), where the baseline says `NOT NULL` outright, as `PostgreSQL`
/// does for any key. A database that was adopted keeps the looser column; no
/// code writes a `NULL` key.
async fn schema(db: &DatabaseConnection) -> Vec<String> {
    if db.get_database_backend() == DbBackend::Sqlite {
        sqlite_schema(db).await
    } else {
        postgres_schema(db).await
    }
}

async fn sqlite_schema(db: &DatabaseConnection) -> Vec<String> {
    let mut lines = Vec::new();
    let tables = texts(
        db,
        "SELECT name FROM sqlite_master WHERE type = 'table' \
         AND name NOT LIKE 'sqlite_%' AND name <> 'seaql_migrations' ORDER BY name",
    )
    .await;
    for table in &tables {
        for column in texts(
            db,
            &format!(
                "SELECT '{table}.' || name, upper(type), \
                 'notnull=' || CASE WHEN pk > 0 THEN 1 ELSE \"notnull\" END, \
                 'default=' || COALESCE(dflt_value, '-'), 'pk=' || pk \
                 FROM pragma_table_info('{table}') ORDER BY cid"
            ),
        )
        .await
        {
            lines.push(format!("column {column}"));
        }
        for key in texts(
            db,
            &format!(
                "SELECT '{table}', \"from\" || ' -> ' || \"table\" || '.' || COALESCE(\"to\", '-'), \
                 on_update || '/' || on_delete FROM pragma_foreign_key_list('{table}') \
                 ORDER BY id, seq"
            ),
        )
        .await
        {
            lines.push(format!("foreign key {key}"));
        }
        for index in rows(
            db,
            &format!(
                "SELECT name, CAST(\"unique\" AS TEXT), CAST(partial AS TEXT) \
                 FROM pragma_index_list('{table}') ORDER BY name"
            ),
        )
        .await
        {
            let columns = texts(
                db,
                &format!(
                    "SELECT COALESCE(name, '<expression>') || CASE \"desc\" WHEN 1 THEN ' DESC' ELSE ' ASC' END \
                     FROM pragma_index_xinfo('{}') WHERE key = 1 ORDER BY seqno",
                    index[0]
                ),
            )
            .await;
            lines.push(format!(
                "index {}.{} unique={} partial={} ({})",
                table,
                index[0],
                index[1],
                index[2],
                columns.join(", ")
            ));
        }
    }
    lines
}

async fn postgres_schema(db: &DatabaseConnection) -> Vec<String> {
    let mut lines = Vec::new();
    let left_out = "('seaql_migrations', 'henk_schema_migrations')";
    for column in texts(
        db,
        &format!(
            "SELECT (table_name || '.' || column_name)::text, data_type::text, \
             ('nullable=' || is_nullable)::text, \
             ('default=' || COALESCE(column_default, '-'))::text, \
             ('identity=' || is_identity || '/' || COALESCE(identity_generation, '-'))::text \
             FROM information_schema.columns \
             WHERE table_schema = current_schema() AND table_name NOT IN {left_out} \
             ORDER BY table_name, ordinal_position"
        ),
    )
    .await
    {
        lines.push(format!("column {column}"));
    }
    for constraint in texts(
        db,
        &format!(
            "SELECT cl.relname::text, con.conname::text, con.contype::text, \
             pg_get_constraintdef(con.oid)::text \
             FROM pg_constraint con JOIN pg_class cl ON cl.oid = con.conrelid \
             JOIN pg_namespace ns ON ns.oid = cl.relnamespace \
             WHERE ns.nspname = current_schema() AND cl.relname NOT IN {left_out} \
             ORDER BY 1, 2"
        ),
    )
    .await
    {
        lines.push(format!("constraint {constraint}"));
    }
    for index in texts(
        db,
        &format!(
            "SELECT tablename::text, indexname::text, replace(indexdef, current_schema() || '.', '') \
             FROM pg_indexes WHERE schemaname = current_schema() \
             AND tablename NOT IN {left_out} ORDER BY 1, 2"
        ),
    )
    .await
    {
        lines.push(format!("index {index}"));
    }
    lines
}

/// Fails with the lines each side has and the other lacks, or, when they
/// have the same lines, with the order that differs.
fn assert_same(what: &str, left: &[String], right: &[String]) {
    let (left_set, right_set): (BTreeSet<_>, BTreeSet<_>) =
        (left.iter().collect(), right.iter().collect());
    let only_left: Vec<_> = left_set.difference(&right_set).collect();
    let only_right: Vec<_> = right_set.difference(&left_set).collect();
    assert!(
        only_left.is_empty() && only_right.is_empty(),
        "{what}\nonly in the first:\n  {}\nonly in the second:\n  {}",
        only_left
            .iter()
            .map(|line| line.as_str())
            .collect::<Vec<_>>()
            .join("\n  "),
        only_right
            .iter()
            .map(|line| line.as_str())
            .collect::<Vec<_>>()
            .join("\n  "),
    );
    assert_eq!(left, right, "{what}: the same lines in another order");
}

/// The steps the migrator has recorded as applied, with their time.
async fn applied(db: &DatabaseConnection) -> Vec<(String, i64)> {
    rows(
        db,
        "SELECT version, CAST(applied_at AS TEXT) FROM seaql_migrations ORDER BY version",
    )
    .await
    .into_iter()
    .map(|row| (row[0].clone(), row[1].parse().unwrap()))
    .collect()
}

fn step_names() -> Vec<String> {
    Migrator::migrations()
        .iter()
        .map(|step| step.name().to_owned())
        .collect()
}

/// How many rows each table the seed fills has, and `requests`.
async fn row_counts(db: &DatabaseConnection) -> Vec<(String, i64)> {
    let mut counts = Vec::new();
    for table in TABLES {
        counts.push((
            table.to_owned(),
            number(db, &format!("SELECT count(*) FROM {table}")).await,
        ));
    }
    counts
}

// --- what the steps are -------------------------------------------------------------

#[test]
fn the_steps_are_numbered_without_gaps_and_named_for_what_they_do() {
    let names = step_names();
    assert!(!names.is_empty());
    for (index, name) in names.iter().enumerate() {
        let (number, what) = name
            .strip_prefix('m')
            .and_then(|rest| rest.split_once('_'))
            .unwrap_or_else(|| panic!("{name} is not m<number>_<what>"));
        assert_eq!(
            number,
            format!("{:03}", index + 1),
            "{name} is not step {}",
            index + 1
        );
        assert!(
            !what.is_empty()
                && what
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "{name}: the name after the number is lower_snake_case"
        );
    }
    let unique: BTreeSet<_> = names.iter().collect();
    assert_eq!(unique.len(), names.len(), "step names are unique");
    // The ones an adopted database already has.
    assert_eq!(
        names[..3],
        ["m001_baseline", "m002_run_instant", "m003_run_children"]
    );
}

// --- SQLite -----------------------------------------------------------------------------

mod sqlite {
    use henk_store::SqliteStore;
    use sqlx::sqlite::SqliteJournalMode;

    use super::*;

    /// Numbers this process's database files, so no two tests share one.
    static NEXT_FILE: AtomicU64 = AtomicU64::new(0);

    /// A database file under the temp directory, removed with the guard.
    struct File(PathBuf);

    impl File {
        fn new() -> Self {
            let n = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
            Self(std::env::temp_dir().join(format!("henk-migrator-{}-{n}.db", std::process::id())))
        }
    }

    impl Drop for File {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let mut path = self.0.clone().into_os_string();
                path.push(suffix);
                let _ = std::fs::remove_file(path);
            }
        }
    }

    /// A pool on the file, set up as the store will: WAL, foreign keys on,
    /// and a wait for SQLite's write lock before a statement fails. It does
    /// not migrate.
    async fn connect(path: &Path) -> DatabaseConnection {
        let mut options = ConnectOptions::new(format!("sqlite://{}?mode=rwc", path.display()));
        options
            .max_connections(8)
            .sqlx_logging(false)
            .map_sqlx_sqlite_opts(|sqlite| {
                sqlite
                    .journal_mode(SqliteJournalMode::Wal)
                    .foreign_keys(true)
                    .busy_timeout(Duration::from_secs(5))
            });
        Database::connect(options).await.unwrap()
    }

    /// A database the old migrations made at their last schema, and nothing
    /// else: opened at 20, so the new migrator has not looked at it.
    fn made_by_the_old_store(file: &File) {
        drop(SqliteStore::open_at(&file.0, 20).unwrap());
    }

    fn user_version(path: &Path) -> i64 {
        rusqlite::Connection::open(path)
            .unwrap()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }

    #[tokio::test]
    async fn an_empty_database_gets_every_table_and_migrating_again_changes_nothing() {
        let file = File::new();
        let db = connect(&file.0).await;
        migrate(&db).await.unwrap();

        let tables = texts(
            &db,
            "SELECT name FROM sqlite_master WHERE type = 'table' \
             AND name NOT LIKE 'sqlite_%' AND name <> 'seaql_migrations' ORDER BY name",
        )
        .await;
        assert_eq!(tables, TABLES);
        let steps = applied(&db).await;
        assert_eq!(
            steps
                .iter()
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>(),
            step_names()
        );
        let before = schema(&db).await;

        migrate(&db).await.unwrap();
        assert_eq!(applied(&db).await, steps, "no step ran twice");
        assert_same("migrating again", &before, &schema(&db).await);
        assert_eq!(
            number(&db, "PRAGMA user_version").await,
            0,
            "the old bookkeeping is not touched on a new database"
        );
    }

    #[tokio::test]
    async fn an_adopted_database_has_the_schema_of_a_fresh_one() {
        let fresh_file = File::new();
        let fresh = connect(&fresh_file.0).await;
        migrate(&fresh).await.unwrap();

        let old_file = File::new();
        made_by_the_old_store(&old_file);
        let adopted = connect(&old_file.0).await;
        migrate(&adopted).await.unwrap();

        assert_same(
            "adopted against fresh",
            &schema(&adopted).await,
            &schema(&fresh).await,
        );
    }

    #[tokio::test]
    async fn adopting_adds_four_indexes_to_what_the_old_store_made_and_nothing_else() {
        let file = File::new();
        made_by_the_old_store(&file);
        let db = connect(&file.0).await;
        let before = schema(&db).await;
        migrate(&db).await.unwrap();
        let after = schema(&db).await;

        let mut added: Vec<_> = after.iter().filter(|line| !before.contains(line)).collect();
        added.sort();
        assert_eq!(
            added,
            [
                "index events.events_run unique=0 partial=0 (run_id ASC)",
                "index findings.findings_run unique=0 partial=0 (run_id ASC)",
                "index inbound_events.inbound_events_received_instant unique=0 partial=0 (<expression> ASC, id ASC)",
                "index sessions.sessions_run unique=0 partial=0 (run_id ASC)",
            ]
        );
        let removed: Vec<_> = before.iter().filter(|line| !after.contains(line)).collect();
        assert!(
            removed.is_empty(),
            "nothing the old store made is gone: {removed:?}"
        );
    }

    #[tokio::test]
    async fn adopting_keeps_every_row_and_the_old_version_and_runs_only_what_is_pending() {
        let file = File::new();
        made_by_the_old_store(&file);
        rusqlite::Connection::open(&file.0)
            .unwrap()
            .execute_batch(SEED)
            .unwrap();
        let db = connect(&file.0).await;
        let before = row_counts(&db).await;
        assert!(
            before.iter().filter(|(_, count)| *count > 0).count() >= 12,
            "the seed fills the tables: {before:?}"
        );

        migrate(&db).await.unwrap();

        assert_eq!(row_counts(&db).await, before);
        assert_eq!(user_version(&file.0), 20, "the old bookkeeping stays");
        let steps = applied(&db).await;
        assert_eq!(
            steps
                .iter()
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>(),
            step_names()
        );
        for (name, at) in &steps[..3] {
            assert_eq!(*at, 0, "{name} is recorded as adopted, not run");
        }
        for (name, at) in &steps[3..] {
            assert!(*at > 0, "{name} ran");
        }

        // And again: the rows and the record stay.
        migrate(&db).await.unwrap();
        assert_eq!(row_counts(&db).await, before);
        assert_eq!(applied(&db).await, steps);
    }

    #[tokio::test]
    async fn a_half_adopted_database_is_adopted_the_rest_of_the_way() {
        let file = File::new();
        made_by_the_old_store(&file);
        let db = connect(&file.0).await;
        // As if a Henk died after recording the first step and before the rest.
        db.execute_unprepared(
            "CREATE TABLE seaql_migrations (version varchar NOT NULL PRIMARY KEY, \
             applied_at bigint NOT NULL); \
             INSERT INTO seaql_migrations (version, applied_at) VALUES ('m001_baseline', 0)",
        )
        .await
        .unwrap();

        migrate(&db).await.unwrap();

        let steps = applied(&db).await;
        assert_eq!(
            steps
                .iter()
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>(),
            step_names()
        );
        for (name, at) in &steps[..3] {
            assert_eq!(*at, 0, "{name} is recorded as adopted, not run");
        }
    }

    #[tokio::test]
    async fn a_database_at_another_version_of_the_old_history_is_refused_and_left_alone() {
        for version in [1, 10, 19] {
            let file = File::new();
            drop(SqliteStore::open_at(&file.0, version).unwrap());
            let db = connect(&file.0).await;
            let before = schema(&db).await;

            let error = migrate(&db).await.unwrap_err();

            let DbErr::Migration(message) = &error else {
                panic!("version {version}: {error:?}");
            };
            assert!(
                message.contains(&format!("version {version} of the previous migrator"))
                    && message
                        .contains("open it once with a Henk that still has the previous migrator"),
                "{message}"
            );
            assert_eq!(user_version(&file.0), i64::try_from(version).unwrap());
            assert_same("left alone", &before, &schema(&db).await);
            assert_eq!(
                number(
                    &db,
                    "SELECT count(*) FROM sqlite_master WHERE name = 'seaql_migrations'"
                )
                .await,
                0,
                "the migrator made nothing"
            );
        }
    }

    #[tokio::test]
    async fn a_database_from_a_newer_henk_is_refused() {
        let file = File::new();
        made_by_the_old_store(&file);
        rusqlite::Connection::open(&file.0)
            .unwrap()
            .pragma_update(None, "user_version", 21)
            .unwrap();
        let db = connect(&file.0).await;

        let error = migrate(&db).await.unwrap_err();

        assert!(
            matches!(&error, DbErr::Migration(message) if message.contains("newer than this Henk knows")),
            "{error:?}"
        );
        assert_eq!(user_version(&file.0), 21);
    }

    #[tokio::test]
    async fn eight_writers_share_one_file_without_a_locked_database() {
        let file = File::new();
        let db = connect(&file.0).await;
        migrate(&db).await.unwrap();

        let writers: Vec<_> = (0..8)
            .map(|writer| {
                let db = db.clone();
                tokio::spawn(async move {
                    for n in 0..25 {
                        db.execute_unprepared(&format!(
                            "INSERT INTO runs (id, kind, platform, repo, target, trigger, status, \
                             started_at, link) VALUES ('w{writer}-{n}', 'review', 'github', 'o/r', \
                             {n}, 'opened', 'running', '2026-10-01T10:00:00Z', 'https://x/{writer}')"
                        ))
                        .await
                        .unwrap();
                        db.execute_unprepared(&format!(
                            "INSERT INTO events (run_id, at, level, message) VALUES \
                             ('w{writer}-{n}', '2026-10-01T10:00:00Z', 'info', 'm')"
                        ))
                        .await
                        .unwrap();
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.await.unwrap();
        }

        assert_eq!(number(&db, "SELECT count(*) FROM runs").await, 200);
        assert_eq!(number(&db, "SELECT count(*) FROM events").await, 200);
        assert_eq!(
            number(&db, "PRAGMA foreign_keys").await,
            1,
            "foreign keys are on in the pool"
        );
        assert_eq!(texts(&db, "PRAGMA journal_mode").await, ["wal"]);
    }

    #[tokio::test]
    async fn foreign_keys_are_enforced_on_a_migrated_database() {
        let file = File::new();
        let db = connect(&file.0).await;
        migrate(&db).await.unwrap();

        let error = db
            .execute_unprepared(
                "INSERT INTO events (run_id, at, level, message) \
                 VALUES ('no-such-run', '2026-10-01T10:00:00Z', 'info', 'm')",
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("FOREIGN KEY"), "{error}");
    }
}

// --- PostgreSQL ---------------------------------------------------------------------------

/// `PostgreSQL`, when `HENK_TEST_DATABASE_URL` names a server. Every test gets
/// its own schemas, so tests run in parallel and leave nothing behind.
mod postgres {
    use henk_store::PgStore;

    use super::*;

    const URL: &str = "HENK_TEST_DATABASE_URL";

    /// Numbers this process's schemas, so no two tests share one.
    static NEXT_SCHEMA: AtomicU64 = AtomicU64::new(0);

    /// A schema of its own; dropped with the guard.
    struct Schema {
        url: String,
        name: String,
        base: String,
    }

    impl Drop for Schema {
        fn drop(&mut self) {
            let (base, name) = (self.base.clone(), self.name.clone());
            // Drop runs outside the test's runtime: clean up on a thread.
            let _ = std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                runtime.block_on(async {
                    if let Ok(client) = plain_client(&base).await {
                        let _ = client
                            .batch_execute(&format!("DROP SCHEMA {name} CASCADE"))
                            .await;
                    }
                });
            })
            .join();
        }
    }

    async fn plain_client(url: &str) -> Result<tokio_postgres::Client, tokio_postgres::Error> {
        let (client, connection) = tokio_postgres::connect(url, tokio_postgres::NoTls).await?;
        tokio::spawn(connection);
        Ok(client)
    }

    async fn schema() -> Schema {
        let base = std::env::var(URL).unwrap_or_else(|_| panic!("{URL} is not set"));
        // The process id, a counter and the clock keep schemas apart, within
        // one run, across runs at once, and from one a crashed run left.
        let name = format!(
            "henk_migrator_{}_{}_{}",
            std::process::id(),
            NEXT_SCHEMA.fetch_add(1, Ordering::Relaxed),
            time::OffsetDateTime::now_utc()
                .unix_timestamp_nanos()
                .unsigned_abs()
        );
        plain_client(&base)
            .await
            .unwrap()
            .batch_execute(&format!("CREATE SCHEMA {name}"))
            .await
            .unwrap();
        let separator = if base.contains('?') { '&' } else { '?' };
        let url = format!("{base}{separator}options=-c%20search_path%3D{name}");
        Schema { url, name, base }
    }

    /// A pool on the schema. It does not migrate.
    async fn connect(schema: &Schema) -> DatabaseConnection {
        let mut options = ConnectOptions::new(schema.url.clone());
        options.max_connections(4).sqlx_logging(false);
        Database::connect(options).await.unwrap()
    }

    /// A schema the old migrations made at their last version, and nothing
    /// else: connected at 16, so the new migrator has not looked at it.
    async fn made_by_the_old_store() -> Schema {
        let schema = schema().await;
        drop(PgStore::connect_at(&schema.url, 16).await.unwrap());
        schema
    }

    #[tokio::test]
    #[ignore = "needs HENK_TEST_DATABASE_URL"]
    async fn an_empty_database_gets_every_table_and_migrating_again_changes_nothing() {
        let schema = schema().await;
        let db = connect(&schema).await;
        migrate(&db).await.unwrap();

        let tables = texts(
            &db,
            "SELECT table_name::text FROM information_schema.tables \
             WHERE table_schema = current_schema() AND table_name <> 'seaql_migrations' \
             ORDER BY table_name",
        )
        .await;
        assert_eq!(tables, TABLES);
        let steps = applied(&db).await;
        assert_eq!(
            steps
                .iter()
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>(),
            step_names()
        );
        let before = schema_of(&db).await;

        migrate(&db).await.unwrap();
        assert_eq!(applied(&db).await, steps, "no step ran twice");
        assert_same("migrating again", &before, &schema_of(&db).await);
    }

    async fn schema_of(db: &DatabaseConnection) -> Vec<String> {
        super::schema(db).await
    }

    #[tokio::test]
    #[ignore = "needs HENK_TEST_DATABASE_URL"]
    async fn an_adopted_database_has_the_schema_of_a_fresh_one_and_differs_from_the_old_by_two_names()
     {
        let fresh = schema().await;
        let fresh_db = connect(&fresh).await;
        migrate(&fresh_db).await.unwrap();

        let old = made_by_the_old_store().await;
        let adopted = connect(&old).await;
        let before = schema_of(&adopted).await;
        migrate(&adopted).await.unwrap();

        assert_same(
            "adopted against fresh",
            &schema_of(&adopted).await,
            &schema_of(&fresh_db).await,
        );
        // What adopting changes on PostgreSQL: the names the `lanes` era left
        // on the `sessions` constraints (`m005_session_constraint_names`).
        let after = schema_of(&adopted).await;
        let mut changed: Vec<_> = before
            .iter()
            .filter(|line| !after.contains(line))
            .chain(after.iter().filter(|line| !before.contains(line)))
            .collect();
        changed.sort();
        assert_eq!(
            changed,
            [
                "constraint sessions lanes_pkey p PRIMARY KEY (run_id, name)",
                "constraint sessions lanes_run_id_fkey f FOREIGN KEY (run_id) REFERENCES runs(id)",
                "constraint sessions sessions_pkey p PRIMARY KEY (run_id, name)",
                "constraint sessions sessions_run_id_fkey f FOREIGN KEY (run_id) REFERENCES runs(id)",
                "index sessions lanes_pkey CREATE UNIQUE INDEX lanes_pkey ON sessions USING btree (run_id, name)",
                "index sessions sessions_pkey CREATE UNIQUE INDEX sessions_pkey ON sessions USING btree (run_id, name)",
            ]
        );
    }

    #[tokio::test]
    #[ignore = "needs HENK_TEST_DATABASE_URL"]
    async fn adopting_keeps_every_row_and_the_old_version_and_runs_only_what_is_pending() {
        let old = made_by_the_old_store().await;
        plain_client(&old.url)
            .await
            .unwrap()
            .batch_execute(SEED)
            .await
            .unwrap();
        let db = connect(&old).await;
        let before = row_counts(&db).await;
        assert!(
            before.iter().filter(|(_, count)| *count > 0).count() >= 12,
            "the seed fills the tables: {before:?}"
        );

        migrate(&db).await.unwrap();

        assert_eq!(row_counts(&db).await, before);
        assert_eq!(
            number(
                &db,
                "SELECT MAX(version)::bigint FROM henk_schema_migrations"
            )
            .await,
            16,
            "the old bookkeeping stays"
        );
        let steps = applied(&db).await;
        assert_eq!(
            steps
                .iter()
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>(),
            step_names()
        );
        for (name, at) in &steps[..3] {
            assert_eq!(*at, 0, "{name} is recorded as adopted, not run");
        }
        for (name, at) in &steps[3..] {
            assert!(*at > 0, "{name} ran");
        }

        migrate(&db).await.unwrap();
        assert_eq!(row_counts(&db).await, before);
        assert_eq!(applied(&db).await, steps);
    }

    #[tokio::test]
    #[ignore = "needs HENK_TEST_DATABASE_URL"]
    async fn a_database_at_another_version_of_the_old_history_is_refused_and_left_alone() {
        for version in [1, 8, 15] {
            let schema = schema().await;
            drop(PgStore::connect_at(&schema.url, version).await.unwrap());
            let db = connect(&schema).await;
            let before = schema_of(&db).await;

            let error = migrate(&db).await.unwrap_err();

            let DbErr::Migration(message) = &error else {
                panic!("version {version}: {error:?}");
            };
            assert!(
                message.contains(&format!("version {version} of the previous migrator"))
                    && message
                        .contains("open it once with a Henk that still has the previous migrator"),
                "{message}"
            );
            assert_same("left alone", &before, &schema_of(&db).await);
            assert_eq!(
                number(
                    &db,
                    "SELECT count(*) FROM information_schema.tables \
                     WHERE table_schema = current_schema() AND table_name = 'seaql_migrations'"
                )
                .await,
                0,
                "the migrator made nothing"
            );
        }
    }

    #[tokio::test]
    #[ignore = "needs HENK_TEST_DATABASE_URL"]
    async fn a_database_from_a_newer_henk_is_refused() {
        let old = made_by_the_old_store().await;
        plain_client(&old.url)
            .await
            .unwrap()
            .batch_execute("INSERT INTO henk_schema_migrations (version) VALUES (17)")
            .await
            .unwrap();
        let db = connect(&old).await;

        let error = migrate(&db).await.unwrap_err();

        assert!(
            matches!(&error, DbErr::Migration(message) if message.contains("newer than this Henk knows")),
            "{error:?}"
        );
    }

    #[tokio::test]
    #[ignore = "needs HENK_TEST_DATABASE_URL"]
    async fn two_connections_migrating_at_once_end_with_one_set_of_steps() {
        for adopted in [false, true] {
            let schema = if adopted {
                made_by_the_old_store().await
            } else {
                schema().await
            };
            let (first, second) = (connect(&schema).await, connect(&schema).await);

            let (a, b) = tokio::join!(migrate(&first), migrate(&second));

            a.unwrap();
            b.unwrap();
            let steps = applied(&first).await;
            assert_eq!(
                steps
                    .iter()
                    .map(|(name, _)| name.clone())
                    .collect::<Vec<_>>(),
                step_names(),
                "adopted: {adopted}"
            );
        }
    }
}
