//! SQLite 020 / `PostgreSQL` 016: the runs a ticket run started.

use async_trait::async_trait;
use sea_orm_migration::prelude::*;

use super::{index, text};

pub(super) struct Step;

impl MigrationName for Step {
    fn name(&self) -> &'static str {
        "m003_run_children"
    }
}

#[async_trait]
impl MigrationTrait for Step {
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
