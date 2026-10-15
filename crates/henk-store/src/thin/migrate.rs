//! One numbered sequence of schema steps for both databases, and the two
//! runners that apply it.
//!
//! The numbers are SQLite's own (`PRAGMA user_version`): step 20 is what
//! SQLite's `020_run_children.sql` was. `PostgreSQL` numbered its history
//! differently (16 steps, its first covering SQLite's 1 to 3), so a step says
//! which legacy `PostgreSQL` version it is, and the runner reads the rows of
//! `henk_schema_migrations` through that table. A step after the legacy
//! history (21 and on) is recorded under its own number on both. Nothing in
//! an existing database is rewritten: a database at SQLite 20 or
//! `PostgreSQL` 16 is the end of step 20 and gets only the steps after it.

use crate::types::StoreError;

/// The steps up to and including this one are the history both databases
/// had before the unification.
const LEGACY_END: usize = 20;

/// Serialises migrations between Henk processes starting together.
const MIGRATION_LOCK: i64 = 0x4865_6e6b; // "Henk"

/// One change to the schema. An empty text is nothing to do on that database.
pub(crate) struct Step {
    pub(crate) sqlite: &'static str,
    pub(crate) postgres: &'static str,
    /// The version `PostgreSQL`'s own history recorded when it had applied
    /// this step; none for a step it never had (or had inside another).
    pub(crate) postgres_legacy: Option<i32>,
}

impl Step {
    /// The same SQL for both.
    const fn shared(sql: &'static str, postgres_legacy: Option<i32>) -> Self {
        Self {
            sqlite: sql,
            postgres: sql,
            postgres_legacy,
        }
    }

    /// Different SQL for each, or none for one.
    const fn each(
        sqlite: &'static str,
        postgres: &'static str,
        postgres_legacy: Option<i32>,
    ) -> Self {
        Self {
            sqlite,
            postgres,
            postgres_legacy,
        }
    }
}

macro_rules! file {
    ($name:literal) => {
        include_str!(concat!("../../migrations/unified/", $name, ".sql"))
    };
}

/// The steps, in order. Only ever append; step `n` is `STEPS[n - 1]`.
pub(crate) const STEPS: &[Step] = &[
    // 1 to 3 and 5 are PostgreSQL's 001: its schema was written whole.
    Step::each(
        file!("001_initial.sqlite"),
        file!("001_initial.postgres"),
        None,
    ),
    Step::each(file!("002_inbound_events.sqlite"), "", None),
    Step::each(file!("003_run_liveness.sqlite"), "", Some(1)),
    Step::shared(file!("004_dashboard"), Some(2)),
    Step::each(file!("005_prune_events.sqlite"), "", None),
    Step::shared(file!("006_event_requester"), Some(3)),
    Step::each(
        file!("007_tool_calls.sqlite"),
        file!("007_tool_calls.postgres"),
        Some(4),
    ),
    Step::each(
        file!("008_transcripts.sqlite"),
        file!("008_transcripts.postgres"),
        Some(5),
    ),
    Step::each(
        file!("009_drafts.sqlite"),
        file!("009_drafts.postgres"),
        Some(6),
    ),
    Step::shared(file!("010_superseded_by"), Some(7)),
    Step::each(
        file!("011_stages.sqlite"),
        file!("011_stages.postgres"),
        Some(8),
    ),
    Step::shared(file!("012_stats"), Some(9)),
    // The expression index of #216: PostgreSQL compares timestamps.
    Step::each(file!("013_run_instant.sqlite"), "", None),
    Step::shared(file!("014_loop"), Some(10)),
    Step::each(
        file!("015_lane_cache.sqlite"),
        file!("015_lane_cache.postgres"),
        Some(11),
    ),
    Step::each(
        file!("016_sessions.sqlite"),
        file!("016_sessions.postgres"),
        Some(12),
    ),
    Step::each(
        file!("017_rounds.sqlite"),
        file!("017_rounds.postgres"),
        Some(13),
    ),
    Step::each(
        file!("018_reasoning_tokens.sqlite"),
        file!("018_reasoning_tokens.postgres"),
        Some(14),
    ),
    Step::each(
        file!("019_tool_names.sqlite"),
        file!("019_tool_names.postgres"),
        Some(15),
    ),
    Step::shared(file!("020_run_children"), Some(16)),
];

/// The step a database has reached when `PostgreSQL`'s bookkeeping says
/// `version`: a legacy version is the step that recorded it, a later one is
/// the step of that number.
fn step_of(version: i32) -> usize {
    let number = usize::try_from(version).unwrap_or(0);
    if number > LEGACY_END {
        return number;
    }
    STEPS
        .iter()
        .position(|step| step.postgres_legacy == Some(version))
        .map_or(0, |index| index + 1)
}

/// The version `PostgreSQL`'s bookkeeping records for step `number`, if any.
fn version_of(number: usize) -> Option<i32> {
    if number > LEGACY_END {
        return i32::try_from(number).ok();
    }
    STEPS.get(number - 1).and_then(|step| step.postgres_legacy)
}

fn newer(applied: usize) -> StoreError {
    StoreError::Schema(format!(
        "the database is at schema version {applied}, newer than the {} this Henk knows",
        STEPS.len()
    ))
}

/// Brings a `SQLite` database up to step `upto` (at most the last), by its
/// `user_version`, in one transaction.
pub(crate) fn sqlite(connection: &mut rusqlite::Connection, upto: usize) -> Result<(), StoreError> {
    // IMMEDIATE: two processes starting together take turns, and the second
    // reads the version the first left.
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let applied: usize = transaction
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .map(|n| usize::try_from(n).unwrap_or(0))?;
    if applied > STEPS.len() {
        return Err(newer(applied));
    }
    for (index, step) in STEPS.iter().enumerate().take(upto).skip(applied) {
        transaction.execute_batch(step.sqlite)?;
        // A pragma takes no parameter; the number is ours.
        transaction.execute_batch(&format!("PRAGMA user_version = {}", index + 1))?;
    }
    transaction.commit()?;
    Ok(())
}

/// Brings a `PostgreSQL` database up to step `upto` (at most the last), by
/// the rows of `henk_schema_migrations`, in one transaction.
pub(crate) async fn postgres(
    client: &mut deadpool_postgres::Object,
    upto: usize,
) -> Result<(), StoreError> {
    let transaction = client.transaction().await?;
    transaction
        .execute("SELECT pg_advisory_xact_lock($1)", &[&MIGRATION_LOCK])
        .await?;
    transaction
        .batch_execute(
            "CREATE TABLE IF NOT EXISTS henk_schema_migrations (
                version    INTEGER PRIMARY KEY,
                applied_at TIMESTAMPTZ NOT NULL DEFAULT now()
             )",
        )
        .await?;
    let applied = transaction
        .query("SELECT version FROM henk_schema_migrations", &[])
        .await?
        .iter()
        .map(|row| row.try_get::<_, i32>(0).map(step_of))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .max()
        .unwrap_or(0);
    if applied > STEPS.len() {
        transaction.rollback().await?;
        return Err(newer(applied));
    }
    for (index, step) in STEPS.iter().enumerate().take(upto).skip(applied) {
        if !step.postgres.is_empty() {
            transaction.batch_execute(step.postgres).await?;
        }
        if let Some(version) = version_of(index + 1) {
            transaction
                .execute(
                    "INSERT INTO henk_schema_migrations (version) VALUES ($1)",
                    &[&version],
                )
                .await?;
        }
    }
    transaction.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_legacy_history_is_the_sixteen_steps_postgres_had() {
        let versions: Vec<i32> = STEPS.iter().filter_map(|s| s.postgres_legacy).collect();
        assert_eq!(versions, (1..=16).collect::<Vec<_>>());
        assert_eq!(STEPS.len(), LEGACY_END);
    }

    #[test]
    fn postgres_versions_map_to_steps_and_back() {
        assert_eq!(step_of(0), 0);
        assert_eq!(step_of(1), 3);
        assert_eq!(step_of(16), 20);
        assert_eq!(step_of(21), 21);
        assert_eq!(version_of(3), Some(1));
        assert_eq!(version_of(20), Some(16));
        assert_eq!(version_of(5), None);
        assert_eq!(version_of(21), Some(21));
    }
}
