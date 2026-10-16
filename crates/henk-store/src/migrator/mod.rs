#![doc = include_str!("README.md")]

mod m001_baseline;
mod m002_run_instant;
mod m003_run_children;
mod m004_run_indexes;
mod m005_session_constraint_names;

use async_trait::async_trait;
use sea_orm::{
    ConnectionTrait, DatabaseConnection, DatabaseTransaction, DbBackend, DbErr, Statement,
    TransactionTrait,
};
use sea_orm_migration::prelude::*;

/// Serialises migrations between Henk processes starting together ("Henk").
const MIGRATION_LOCK: i64 = 0x4865_6e6b;

/// The names of the steps the old stores' last schema (SQLite 020,
/// `PostgreSQL` 016) already contains.
const LEGACY_FINAL: [&str; 3] = ["m001_baseline", "m002_run_instant", "m003_run_children"];

/// The schema version the old stores ended at, per database.
const LEGACY_SQLITE_VERSION: i64 = 20;
const LEGACY_POSTGRES_VERSION: i64 = 16;

/// The steps, in order. Only ever append: a step that has shipped is never
/// edited, reordered or removed.
pub struct Migrator;

#[async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![
            Box::new(m001_baseline::Step),
            Box::new(m002_run_instant::Step),
            Box::new(m003_run_children::Step),
            Box::new(m004_run_indexes::Step),
            Box::new(m005_session_constraint_names::Step),
        ]
    }
}

/// Brings the database up to date: adopts a database the old stores made,
/// then applies the steps it lacks. On `PostgreSQL` the whole thing runs
/// under an advisory lock, as the old store's migrator did, so Henk
/// processes starting together migrate one after the other.
///
/// # Errors
///
/// Returns [`DbErr`] when the database cannot be reached, when it is at a
/// schema version of the previous migrator other than the last one (see the
/// module documentation), or when a step fails.
pub async fn migrate(db: &DatabaseConnection) -> Result<(), DbErr> {
    if db.get_database_backend() == DbBackend::Postgres {
        let transaction = db.begin().await?;
        match migrate_locked(&transaction).await {
            Ok(()) => transaction.commit().await,
            Err(error) => {
                // Roll back now: a dropped transaction is rolled back only
                // when its connection is next used, and until then the lock
                // below stays held and every other Henk waits for it.
                let _ = transaction.rollback().await;
                Err(error)
            }
        }
    } else {
        adopt(db).await?;
        Migrator::up(db, None).await
    }
}

/// The `PostgreSQL` half of [`migrate`], inside its transaction and under the
/// lock, which the transaction releases when it ends.
async fn migrate_locked(db: &DatabaseTransaction) -> Result<(), DbErr> {
    db.execute_raw(Statement::from_sql_and_values(
        DbBackend::Postgres,
        "SELECT pg_advisory_xact_lock($1)",
        [MIGRATION_LOCK.into()],
    ))
    .await?;
    adopt(db).await?;
    Migrator::up(db, None).await
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
    if legacy < final_version {
        return Err(DbErr::Migration(format!(
            "the database is at schema version {legacy} of the previous migrator; \
             open it once with a Henk that still has the previous migrator, to reach \
             version {final_version}, then start this one"
        )));
    }
    if legacy > final_version {
        return Err(DbErr::Migration(format!(
            "the database is at schema version {legacy} of the previous migrator, \
             which is newer than this Henk knows ({final_version}); \
             start the Henk that wrote it, or restore a backup"
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

// --- what differs per database, for the steps to share --------------------------

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
