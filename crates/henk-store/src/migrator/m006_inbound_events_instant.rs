//! The expression index that lists inbound events by when they were received
//! as an instant, on SQLite only.

use async_trait::async_trait;
use sea_orm::DbBackend;
use sea_orm_migration::prelude::*;

pub(super) struct Step;

impl MigrationName for Step {
    fn name(&self) -> &'static str {
        "m006_inbound_events_instant"
    }
}

#[async_trait]
impl MigrationTrait for Step {
    fn use_transaction(&self) -> Option<bool> {
        Some(true)
    }

    /// The events page lists events newest first by `received_at` as an
    /// instant, not as text: as text, `...:00.4Z` sorts before `...:00Z` and
    /// is listed after it (#216 fixed the same for runs). The query builder
    /// cannot index an expression, so this is raw SQL: the text of the
    /// expression the listing orders by, written out, because SQLite uses an
    /// expression index only when the query's expression is the same.
    /// `PostgreSQL` compares timestamps natively and needs nothing.
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager.get_database_backend() == DbBackend::Sqlite {
            manager
                .get_connection()
                .execute_unprepared(
                    "CREATE INDEX IF NOT EXISTS inbound_events_received_instant ON inbound_events(
                        substr(received_at, 1, 19) || substr(CASE WHEN substr(received_at, 20, 1) = '.' THEN substr(received_at, 21, length(received_at) - 21) ELSE '' END || '000000000', 1, 9),
                        id
                    )",
                )
                .await?;
        }
        Ok(())
    }
}
