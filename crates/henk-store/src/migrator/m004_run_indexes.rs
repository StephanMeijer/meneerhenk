//! The first step after the old history: three indexes only `PostgreSQL`
//! had, for both databases.

use async_trait::async_trait;
use sea_orm_migration::prelude::*;

pub(super) struct Step;

impl MigrationName for Step {
    fn name(&self) -> &'static str {
        "m004_run_indexes"
    }
}

#[async_trait]
impl MigrationTrait for Step {
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
