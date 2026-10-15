//! The schema as one sequence of sea-orm-migration steps, for SQLite and
//! `PostgreSQL` alike. A step that differs per database asks the
//! `SchemaManager` which one it is running on (`get_database_backend`).
//!
//! Four steps; the first three are the final schema of SQLite 001 to 020 and
//! `PostgreSQL` 001 to 016 squashed, the fourth is new:
//!
//! 1. `m001_baseline`: every table of SQLite 019 / `PostgreSQL` 015, with its
//!    indexes (the DSL, with three helpers for what differs per database).
//! 2. `m002_run_instant`: the expression index of SQLite 013 (raw SQL; the
//!    DSL cannot index an expression, and `PostgreSQL` needs none).
//! 3. `m003_run_children`: SQLite 020 / `PostgreSQL` 016 (shared DSL).
//! 4. `m004_run_indexes`: three indexes only `PostgreSQL` had (shared DSL, a
//!    step that runs on adopted databases).
//!
//! A database made by the old stores (`PRAGMA user_version = 20` or table
//! `henk_schema_migrations` at 16) is adopted by [`adopt`], which records
//! the first three steps as applied before the migrator looks at it.

use async_trait::async_trait;
use sea_orm::{ConnectionTrait, DatabaseConnection, DbBackend, DbErr, Statement, TransactionTrait};
use sea_orm_migration::prelude::*;

/// Serialises migrations between Henk processes starting together ("Henk").
const MIGRATION_LOCK: i64 = 0x4865_6e6b;

/// The steps, in order. Only ever append.
pub struct Migrator;

#[async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(Baseline),
            Box::new(RunInstant),
            Box::new(RunChildren),
            Box::new(RunIndexes),
        ]
    }
}

/// The names of the steps the old stores' last schema (SQLite 020,
/// `PostgreSQL` 016) already contains.
const LEGACY_FINAL: [&str; 3] = ["m001_baseline", "m002_run_instant", "m003_run_children"];

/// The schema version the old stores ended at, per database.
const LEGACY_SQLITE_VERSION: i64 = 20;
const LEGACY_POSTGRES_VERSION: i64 = 16;

/// Brings the database up to date: adopts a database the old stores made,
/// then applies the steps it lacks. On `PostgreSQL` the whole thing runs
/// under an advisory lock, as the old store's migrator did.
pub async fn migrate(db: &DatabaseConnection) -> Result<(), DbErr> {
    if db.get_database_backend() == DbBackend::Postgres {
        let transaction = db.begin().await?;
        transaction
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Postgres,
                "SELECT pg_advisory_xact_lock($1)",
                [MIGRATION_LOCK.into()],
            ))
            .await?;
        adopt(&transaction).await?;
        Migrator::up(&transaction, None).await?;
        transaction.commit().await
    } else {
        adopt(db).await?;
        Migrator::up(db, None).await
    }
}

/// Recognises a database the old stores made and records its steps as applied
/// in `seaql_migrations`, so the migrator does not run them again. Nothing is
/// changed in the old bookkeeping: `user_version` and `henk_schema_migrations`
/// stay as they are.
async fn adopt<C: ConnectionTrait>(db: &C) -> Result<(), DbErr> {
    let backend = db.get_database_backend();
    let (legacy, final_version) = if backend == DbBackend::Sqlite {
        // RAW: the one place SQLite's own bookkeeping is read, a pragma.
        (
            scalar(db, "PRAGMA user_version").await?,
            LEGACY_SQLITE_VERSION,
        )
    } else {
        // RAW: the old store's table, if there is one (the catalogue says).
        let has_table = scalar(
            db,
            "SELECT count(*) FROM information_schema.tables \
             WHERE table_schema = current_schema() AND table_name = 'henk_schema_migrations'",
        )
        .await?
            > 0;
        let version = if has_table {
            scalar(
                db,
                "SELECT COALESCE(MAX(version), 0)::bigint FROM henk_schema_migrations",
            )
            .await?
        } else {
            0
        };
        (version, LEGACY_POSTGRES_VERSION)
    };
    if legacy == 0 {
        // A new database, or one the migrator already owns.
        return Ok(());
    }
    if legacy != final_version {
        return Err(DbErr::Migration(format!(
            "the database is at schema version {legacy} of the previous migrator; \
             open it once with a Henk that still has it, to reach {final_version}"
        )));
    }
    // `install` is what the migrator does first; do it here so the table exists.
    Migrator::install(db).await?;
    if !Migrator::get_migration_models(db).await?.is_empty() {
        return Ok(()); // adopted before
    }
    for name in LEGACY_FINAL {
        db.execute_raw(Statement::from_sql_and_values(
            backend,
            match backend {
                DbBackend::Postgres => {
                    "INSERT INTO seaql_migrations (version, applied_at) VALUES ($1, $2)"
                }
                _ => "INSERT INTO seaql_migrations (version, applied_at) VALUES (?, ?)",
            },
            [name.into(), 0_i64.into()],
        ))
        .await?;
    }
    Ok(())
}

/// One number out of a one-row query (a raw statement: a pragma and the
/// catalogue are not tables the query builder knows).
async fn scalar<C: ConnectionTrait>(db: &C, sql: &str) -> Result<i64, DbErr> {
    let row = db
        .query_one_raw(Statement::from_string(db.get_database_backend(), sql))
        .await?
        .ok_or_else(|| DbErr::Custom(format!("{sql} returned no row")))?;
    row.try_get_by_index(0)
}

// --- the three things that differ per database -------------------------------

/// A time column: `TIMESTAMPTZ` on `PostgreSQL`, RFC 3339 text on SQLite.
fn time(backend: DbBackend, name: &'static str) -> ColumnDef {
    let mut column = ColumnDef::new(name);
    match backend {
        DbBackend::Postgres => column.timestamp_with_time_zone(),
        _ => column.text(),
    };
    column
}

/// A number: `BIGINT` on `PostgreSQL`, `INTEGER` on SQLite (which is 64 bits).
fn number(backend: DbBackend, name: &'static str) -> ColumnDef {
    let mut column = ColumnDef::new(name);
    match backend {
        DbBackend::Postgres => column.big_integer(),
        _ => column.integer(),
    };
    column
}

/// A recording-order key: an identity column on `PostgreSQL`, an
/// `AUTOINCREMENT` rowid alias on SQLite.
fn key(backend: DbBackend) -> ColumnDef {
    let mut column = ColumnDef::new("id");
    match backend {
        DbBackend::Postgres => column
            .big_integer()
            .not_null()
            .extra("GENERATED ALWAYS AS IDENTITY")
            .primary_key(),
        _ => column.integer().not_null().auto_increment().primary_key(),
    };
    column
}

fn text(name: &'static str) -> ColumnDef {
    let mut column = ColumnDef::new(name);
    column.text();
    column
}

/// `REFERENCES runs(id)`.
fn run_fk(table: &'static str) -> ForeignKeyCreateStatement {
    ForeignKey::create()
        .from(table, "run_id")
        .to("runs", "id")
        .take()
}

async fn index(
    manager: &SchemaManager<'_>,
    name: &'static str,
    table: &'static str,
    columns: &[&'static str],
    unique: bool,
) -> Result<(), DbErr> {
    let mut statement = Index::create();
    statement.name(name).table(table);
    for column in columns {
        statement.col(*column);
    }
    if unique {
        statement.unique();
    }
    manager.create_index(statement).await
}

// --- m001 ----------------------------------------------------------------------

struct Baseline;

impl MigrationName for Baseline {
    fn name(&self) -> &'static str {
        "m001_baseline"
    }
}

#[async_trait]
impl MigrationTrait for Baseline {
    // SQLite DDL is transactional: a step that fails halfway leaves nothing.
    fn use_transaction(&self) -> Option<bool> {
        Some(true)
    }

    #[allow(clippy::too_many_lines)] // twelve tables
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let b = manager.get_database_backend();

        // SQLite 016 added `role` with a default it cannot drop again;
        // PostgreSQL 012 dropped it. Both are reproduced.
        let mut role = text("role");
        role.not_null();
        if b == DbBackend::Sqlite {
            role.default("lane");
        }

        manager
            .create_table(
                Table::create()
                    .table("runs")
                    .col(text("id").not_null().primary_key())
                    .col(text("kind").not_null())
                    .col(text("platform").not_null())
                    .col(text("repo").not_null())
                    .col(number(b, "target").not_null())
                    .col(text("commit_sha"))
                    .col(text("requester"))
                    .col(text("trigger").not_null())
                    .col(text("status").not_null())
                    .col(time(b, "started_at").not_null())
                    .col(time(b, "finished_at"))
                    .col(text("link").not_null())
                    .col(text("summary"))
                    .col(text("error"))
                    .col(time(b, "heartbeat_at"))
                    .col(text("check_id"))
                    .col(text("superseded_by"))
                    .col(text("loop_stop"))
                    .col(ColumnDef::new("loop_rounds").integer())
                    .take(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table("sessions")
                    .col(text("run_id").not_null())
                    .col(text("name").not_null())
                    .col(text("model").not_null())
                    .col(text("status").not_null())
                    .col(number(b, "turns").not_null().default(0))
                    .col(number(b, "input_tokens").not_null().default(0))
                    .col(number(b, "output_tokens").not_null().default(0))
                    .col(time(b, "started_at").not_null())
                    .col(time(b, "finished_at"))
                    .col(text("error"))
                    .col(number(b, "cache_read_tokens").not_null().default(0))
                    .col(number(b, "cache_write_tokens").not_null().default(0))
                    .col(&mut role)
                    .col(number(b, "reasoning_tokens").not_null().default(0))
                    .primary_key(Index::create().col("run_id").col("name"))
                    .foreign_key(&mut run_fk("sessions"))
                    .take(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table("findings")
                    .col(key(b))
                    .col(text("run_id").not_null())
                    .col(text("session").not_null())
                    .col(text("path").not_null())
                    .col(number(b, "line").not_null())
                    .col(text("comment_id").not_null())
                    .col(text("action").not_null())
                    .col(time(b, "created_at").not_null())
                    .foreign_key(&mut run_fk("findings"))
                    .take(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table("events")
                    .col(key(b))
                    .col(text("run_id").not_null())
                    .col(time(b, "at").not_null())
                    .col(text("level").not_null())
                    .col(text("message").not_null())
                    .foreign_key(&mut run_fk("events"))
                    .take(),
            )
            .await?;

        // `requests` and `event_outcomes` have an identity key on PostgreSQL
        // only; SQLite orders them by rowid, and the old files never gave
        // them a column. Kept as it is, so a fresh and an adopted database
        // are the same.
        let mut requests = Table::create();
        requests.table("requests");
        if b == DbBackend::Postgres {
            requests.col(key(b));
        }
        requests
            .col(text("run_id").not_null())
            .col(time(b, "at").not_null())
            .col(text("source").not_null())
            .foreign_key(&mut run_fk("requests"));
        manager.create_table(requests).await?;

        manager
            .create_table(
                Table::create()
                    .table("inbound_events")
                    .col(text("id").not_null().primary_key())
                    .col(time(b, "received_at").not_null())
                    .col(text("source").not_null())
                    .col(text("kind").not_null())
                    .col(text("repo"))
                    .col(number(b, "target"))
                    .col(text("payload"))
                    .col(text("requester"))
                    .take(),
            )
            .await?;

        let mut outcomes = Table::create();
        outcomes.table("event_outcomes");
        if b == DbBackend::Postgres {
            outcomes.col(key(b));
        }
        outcomes
            .col(text("event_id").not_null())
            .col(text("listener").not_null())
            .col(text("outcome").not_null())
            .col(text("detail").not_null())
            .col(text("run_id"))
            .col(time(b, "at").not_null())
            .foreign_key(
                ForeignKey::create()
                    .from("event_outcomes", "event_id")
                    .to("inbound_events", "id"),
            );
        manager.create_table(outcomes).await?;

        manager
            .create_table(
                Table::create()
                    .table("tool_calls")
                    .col(key(b))
                    .col(text("run_id").not_null())
                    .col(text("session").not_null())
                    .col(text("model").not_null())
                    .col(number(b, "turn").not_null())
                    .col(text("tool").not_null())
                    .col(text("origin").not_null())
                    .col(text("outcome").not_null())
                    .col(text("arguments").not_null())
                    .col(number(b, "arguments_len").not_null())
                    .col(number(b, "result_chars").not_null())
                    .col(number(b, "elapsed_ms").not_null())
                    .col(time(b, "at").not_null())
                    .foreign_key(&mut run_fk("tool_calls"))
                    .take(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table("transcripts")
                    .col(key(b))
                    .col(text("run_id").not_null())
                    .col(text("session").not_null())
                    .col(text("model").not_null())
                    .col(text("stop").not_null())
                    .col(number(b, "turns").not_null())
                    .col(number(b, "bytes").not_null())
                    .col(text("body").not_null())
                    .col(time(b, "recorded_at").not_null())
                    .foreign_key(&mut run_fk("transcripts"))
                    .take(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table("drafts")
                    .col(key(b))
                    .col(text("run_id").not_null())
                    .col(text("draft").not_null())
                    .col(text("session").not_null())
                    .col(text("model").not_null())
                    .col(text("kind").not_null())
                    .col(text("path").not_null())
                    .col(number(b, "line").not_null())
                    .col(text("target").not_null())
                    .col(text("body").not_null())
                    .col(time(b, "created_at").not_null())
                    .col(text("verdict"))
                    .col(text("checker").not_null().default(""))
                    .col(text("reason").not_null().default(""))
                    .col(text("same_as").not_null().default(""))
                    .col(text("comment_id").not_null().default(""))
                    .col(time(b, "decided_at"))
                    .foreign_key(&mut run_fk("drafts"))
                    .take(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table("stages")
                    .col(text("run_id").not_null())
                    .col(text("stage").not_null())
                    .col(text("state").not_null())
                    .col(time(b, "started_at").not_null())
                    .col(time(b, "ended_at"))
                    .col(text("detail").not_null().default(""))
                    .primary_key(Index::create().col("run_id").col("stage"))
                    .foreign_key(&mut run_fk("stages"))
                    .take(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table("session_rounds")
                    .col(text("run_id").not_null())
                    .col(text("session").not_null())
                    .col(text("role").not_null())
                    .col(number(b, "round").not_null())
                    .col(time(b, "started_at").not_null())
                    .col(time(b, "ended_at"))
                    .col(number(b, "first_turn").not_null())
                    .col(number(b, "turns").not_null().default(0))
                    .col(text("handover").not_null())
                    .col(number(b, "handover_len").not_null())
                    .primary_key(Index::create().col("run_id").col("session").col("round"))
                    .foreign_key(&mut run_fk("session_rounds"))
                    .take(),
            )
            .await?;

        for (name, table, columns, unique) in [
            (
                "runs_repo_target",
                "runs",
                &["platform", "repo", "target", "started_at"][..],
                false,
            ),
            ("runs_started", "runs", &["started_at"][..], false),
            ("runs_status", "runs", &["status", "started_at"][..], false),
            ("findings_created", "findings", &["created_at"][..], false),
            (
                "event_outcomes_run",
                "event_outcomes",
                &["run_id"][..],
                false,
            ),
            (
                "event_outcomes_event",
                "event_outcomes",
                &["event_id"][..],
                false,
            ),
            (
                "inbound_events_received",
                "inbound_events",
                &["received_at"][..],
                false,
            ),
            (
                "tool_calls_run",
                "tool_calls",
                &["run_id", "session"][..],
                false,
            ),
            ("tool_calls_at", "tool_calls", &["at"][..], false),
            (
                "transcripts_run",
                "transcripts",
                &["run_id", "session"][..],
                false,
            ),
            (
                "transcripts_recorded",
                "transcripts",
                &["recorded_at"][..],
                false,
            ),
            ("drafts_run", "drafts", &["run_id", "draft"][..], true),
            ("drafts_created", "drafts", &["created_at"][..], false),
        ] {
            index(manager, name, table, columns, unique).await?;
        }
        Ok(())
    }
}

// --- m002 ----------------------------------------------------------------------

struct RunInstant;

impl MigrationName for RunInstant {
    fn name(&self) -> &'static str {
        "m002_run_instant"
    }
}

#[async_trait]
impl MigrationTrait for RunInstant {
    fn use_transaction(&self) -> Option<bool> {
        Some(true)
    }

    /// SQLite 013: orders and pages `started_at` as an instant, not as text
    /// (#216). The query builder cannot index an expression: raw SQL, the
    /// text of `Candidate::instant("started_at")` written out (SQLite uses an
    /// expression index only when the query's expression is the same).
    /// `PostgreSQL` compares timestamps natively and needs nothing.
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager.get_database_backend() == DbBackend::Sqlite {
            manager
                .get_connection()
                .execute_unprepared(
                    "CREATE INDEX runs_started_instant ON runs(
                        substr(started_at, 1, 19) || substr(CASE WHEN substr(started_at, 20, 1) = '.' THEN substr(started_at, 21, length(started_at) - 21) ELSE '' END || '000000000', 1, 9),
                        id
                    )",
                )
                .await?;
        }
        Ok(())
    }
}

// --- m003 ----------------------------------------------------------------------

struct RunChildren;

impl MigrationName for RunChildren {
    fn name(&self) -> &'static str {
        "m003_run_children"
    }
}

#[async_trait]
impl MigrationTrait for RunChildren {
    fn use_transaction(&self) -> Option<bool> {
        Some(true)
    }

    /// SQLite 020 / `PostgreSQL` 016: the runs a ticket run started. The same
    /// text in both old files, and the same DSL here with no helper.
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table("run_children")
                    .col(text("parent").not_null())
                    .col(text("stage").not_null())
                    .col(text("child").not_null())
                    .primary_key(Index::create().col("parent").col("stage"))
                    .foreign_key(
                        ForeignKey::create()
                            .from("run_children", "parent")
                            .to("runs", "id"),
                    )
                    .take(),
            )
            .await?;
        index(
            manager,
            "run_children_child",
            "run_children",
            &["child"],
            false,
        )
        .await
    }
}

// --- m004 ----------------------------------------------------------------------

struct RunIndexes;

impl MigrationName for RunIndexes {
    fn name(&self) -> &'static str {
        "m004_run_indexes"
    }
}

#[async_trait]
impl MigrationTrait for RunIndexes {
    fn use_transaction(&self) -> Option<bool> {
        Some(true)
    }

    /// The first step after the old history: three indexes `PostgreSQL` has
    /// had since its first migration and SQLite never got. A database
    /// adopted at SQLite 020 or `PostgreSQL` 016 finds this step pending, so
    /// it shows that the migrator carries on from the adopted state;
    /// `IF NOT EXISTS` makes it a no-op where the index is already there.
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for (name, table) in [
            ("sessions_run", "sessions"),
            ("findings_run", "findings"),
            ("events_run", "events"),
        ] {
            manager
                .create_index(
                    Index::create()
                        .if_not_exists()
                        .name(name)
                        .table(table)
                        .col("run_id")
                        .take(),
                )
                .await?;
        }
        Ok(())
    }
}
