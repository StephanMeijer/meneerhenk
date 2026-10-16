//! The expression index of SQLite 013, on SQLite only.

use async_trait::async_trait;
use sea_orm::DbBackend;
use sea_orm_migration::prelude::*;

pub(super) struct Step;

impl MigrationName for Step {
    fn name(&self) -> &'static str {
        "m002_run_instant"
    }
}

#[async_trait]
impl MigrationTrait for Step {
    fn use_transaction(&self) -> Option<bool> {
        Some(true)
    }

    /// SQLite 013: orders and pages `started_at` as an instant, not as text
    /// (#216). The query builder cannot index an expression, so this is raw
    /// SQL: the text of the expression the run queries order by, written out.
    /// SQLite uses an expression index only when the query's expression is
    /// the same, so a change to that expression changes this index in a new
    /// step. `PostgreSQL` compares timestamps natively and needs nothing.
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
