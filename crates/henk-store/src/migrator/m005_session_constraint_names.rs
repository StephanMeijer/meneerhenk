//! `PostgreSQL` only: the names of the `sessions` primary key and foreign key.
//!
//! The table was `lanes` until `PostgreSQL` 012 renamed it, and a rename keeps
//! the names its constraints had: a database the old migrations made has
//! `lanes_pkey` and `lanes_run_id_fkey` where the baseline names them
//! `sessions_pkey` and `sessions_run_id_fkey`. A later step that drops one
//! by name would otherwise work on a fresh database and fail on an adopted
//! one. SQLite names neither, so it has nothing to do.

use async_trait::async_trait;
use sea_orm::DbBackend;
use sea_orm_migration::prelude::*;

pub(super) struct Step;

impl MigrationName for Step {
    fn name(&self) -> &'static str {
        "m005_session_constraint_names"
    }
}

#[async_trait]
impl MigrationTrait for Step {
    fn use_transaction(&self) -> Option<bool> {
        Some(true)
    }

    /// RAW: renaming a constraint only if it has the old name, which the DSL
    /// cannot say. Renaming the primary key renames its index with it.
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager.get_database_backend() == DbBackend::Postgres {
            manager
                .get_connection()
                .execute_unprepared(
                    "DO $$
                    BEGIN
                        IF EXISTS (SELECT 1 FROM pg_constraint
                                   WHERE conname = 'lanes_pkey' AND conrelid = 'sessions'::regclass) THEN
                            ALTER TABLE sessions RENAME CONSTRAINT lanes_pkey TO sessions_pkey;
                        END IF;
                        IF EXISTS (SELECT 1 FROM pg_constraint
                                   WHERE conname = 'lanes_run_id_fkey' AND conrelid = 'sessions'::regclass) THEN
                            ALTER TABLE sessions RENAME CONSTRAINT lanes_run_id_fkey TO sessions_run_id_fkey;
                        END IF;
                    END
                    $$",
                )
                .await?;
        }
        Ok(())
    }
}
