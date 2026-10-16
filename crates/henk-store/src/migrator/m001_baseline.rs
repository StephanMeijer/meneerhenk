//! The first schema: every table of SQLite 019 / `PostgreSQL` 015 with its
//! indexes, in the migration DSL. A database the old stores made already has
//! all of this and is adopted past it (see the module documentation).

use async_trait::async_trait;
use sea_orm::DbBackend;
use sea_orm_migration::prelude::*;

use super::{index, key, number, run_fk, text, time};

pub(super) struct Step;

impl MigrationName for Step {
    fn name(&self) -> &'static str {
        "m001_baseline"
    }
}

#[async_trait]
impl MigrationTrait for Step {
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
