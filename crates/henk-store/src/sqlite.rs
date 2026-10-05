//! SQLite: one connection behind a mutex. Writes take microseconds, so the
//! async methods run them in place.

use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;
use henk_domain::run::{EventId, RunId};
use rusqlite::{Connection, OptionalExtension as _, params};
use rusqlite_migration::{M, Migrations};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::store::RunStore;
use crate::types::{
    EventRecord, FindingAction, FindingRecord, InboundEvent, LaneRecord, LaneStatus,
    MAX_PAYLOAD_BYTES, NewRun, OutcomeRecord, RawRun, RunRecord, RunStatus, StoreError, kind_str,
    now, platform_str,
};

/// The run store over SQLite.
#[derive(Debug)]
pub struct SqliteStore {
    connection: Mutex<Connection>,
}

fn migrations() -> Migrations<'static> {
    Migrations::new(vec![
        M::up(include_str!("../migrations/sqlite/001_initial.sql")),
        M::up(include_str!("../migrations/sqlite/002_inbound_events.sql")),
        M::up(include_str!("../migrations/sqlite/003_run_liveness.sql")),
    ])
}

impl SqliteStore {
    /// Opens or creates the database at `path` and migrates it.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the file cannot be opened or migrated.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        Self::from_connection(Connection::open(path)?)
    }

    /// An in-memory store, for tests and probes.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when SQLite cannot create the database.
    pub fn in_memory() -> Result<Self, StoreError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(mut connection: Connection) -> Result<Self, StoreError> {
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        migrations().to_latest(&mut connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn with<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let connection = self.connection.lock().map_err(|_| StoreError::Poisoned)?;
        f(&connection)
    }
}

#[async_trait]
impl RunStore for SqliteStore {
    async fn create_run(&self, run: &NewRun) -> Result<(), StoreError> {
        self.with(|c| {
            c.execute(
                "INSERT INTO runs (id, kind, platform, repo, target, commit_sha, requester, trigger, status, started_at, link, heartbeat_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?10)",
                params![
                    run.id.as_str(),
                    kind_str(run.kind),
                    platform_str(run.platform),
                    run.repo,
                    i64::try_from(run.target).unwrap_or(i64::MAX),
                    run.commit,
                    run.requester,
                    run.trigger,
                    RunStatus::Running.as_str(),
                    now(),
                    run.link,
                ],
            )?;
            Ok(())
        })
    }

    async fn finish_run(
        &self,
        id: &RunId,
        status: RunStatus,
        summary: Option<&str>,
        error: Option<&str>,
    ) -> Result<(), StoreError> {
        self.with(|c| {
            c.execute(
                "UPDATE runs SET status = ?2, finished_at = ?3, summary = ?4, error = ?5 WHERE id = ?1",
                params![id.as_str(), status.as_str(), now(), summary, error],
            )?;
            Ok(())
        })
    }

    async fn run(&self, id: &RunId) -> Result<Option<RunRecord>, StoreError> {
        self.with(|c| {
            c.query_row(
                &format!("SELECT {RUN_COLUMNS} FROM runs WHERE id = ?1"),
                params![id.as_str()],
                raw_run,
            )
            .optional()?
            .map(RawRun::into_record)
            .transpose()
        })
    }

    async fn heartbeat(&self, id: &RunId) -> Result<(), StoreError> {
        self.with(|c| {
            c.execute(
                "UPDATE runs SET heartbeat_at = ?2 WHERE id = ?1",
                params![id.as_str(), now()],
            )?;
            Ok(())
        })
    }

    async fn set_check(&self, id: &RunId, check_id: &str) -> Result<(), StoreError> {
        self.with(|c| {
            c.execute(
                "UPDATE runs SET check_id = ?2 WHERE id = ?1",
                params![id.as_str(), check_id],
            )?;
            Ok(())
        })
    }

    async fn orphaned_runs(
        &self,
        stale_before: OffsetDateTime,
    ) -> Result<Vec<RunRecord>, StoreError> {
        let cutoff = stale_before
            .format(&Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned());
        self.with(|c| {
            let mut statement = c.prepare(&format!(
                "SELECT {RUN_COLUMNS} FROM runs
                 WHERE status = ?1 AND (heartbeat_at IS NULL OR heartbeat_at < ?2)
                 ORDER BY started_at"
            ))?;
            let raw = statement
                .query_map(params![RunStatus::Running.as_str(), cutoff], raw_run)?
                .collect::<Result<Vec<_>, _>>()?;
            raw.into_iter().map(RawRun::into_record).collect()
        })
    }

    async fn drop_running_lanes(&self, run: &RunId, reason: &str) -> Result<(), StoreError> {
        self.with(|c| {
            c.execute(
                "UPDATE lanes SET status = ?2, finished_at = ?3, error = ?4
                 WHERE run_id = ?1 AND status = ?5",
                params![
                    run.as_str(),
                    LaneStatus::Dropped.as_str(),
                    now(),
                    reason,
                    LaneStatus::Running.as_str()
                ],
            )?;
            Ok(())
        })
    }

    async fn start_lane(&self, run: &RunId, name: &str, model: &str) -> Result<(), StoreError> {
        self.with(|c| {
            c.execute(
                "INSERT INTO lanes (run_id, name, model, status, started_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![run.as_str(), name, model, LaneStatus::Running.as_str(), now()],
            )?;
            Ok(())
        })
    }

    async fn finish_lane(
        &self,
        run: &RunId,
        name: &str,
        status: LaneStatus,
        turns: u64,
        input_tokens: u64,
        output_tokens: u64,
        error: Option<&str>,
    ) -> Result<(), StoreError> {
        self.with(|c| {
            c.execute(
                "UPDATE lanes SET status = ?3, turns = ?4, input_tokens = ?5, output_tokens = ?6, finished_at = ?7, error = ?8
                 WHERE run_id = ?1 AND name = ?2",
                params![
                    run.as_str(),
                    name,
                    status.as_str(),
                    i64::try_from(turns).unwrap_or(i64::MAX),
                    i64::try_from(input_tokens).unwrap_or(i64::MAX),
                    i64::try_from(output_tokens).unwrap_or(i64::MAX),
                    now(),
                    error
                ],
            )?;
            Ok(())
        })
    }

    async fn lanes(&self, run: &RunId) -> Result<Vec<LaneRecord>, StoreError> {
        self.with(|c| {
            let mut statement = c.prepare(
                "SELECT name, model, status, turns, input_tokens, output_tokens, error FROM lanes WHERE run_id = ?1 ORDER BY name",
            )?;
            let rows = statement.query_map(params![run.as_str()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            })?;
            let mut lanes = Vec::new();
            for row in rows {
                let (name, model, status, turns, input_tokens, output_tokens, error) = row?;
                let status = LaneStatus::parse(&status)
                    .ok_or(StoreError::Corrupt { column: "lanes.status", value: status })?;
                lanes.push(LaneRecord {
                    name,
                    model,
                    status,
                    turns: u64::try_from(turns).unwrap_or(0),
                    input_tokens: u64::try_from(input_tokens).unwrap_or(0),
                    output_tokens: u64::try_from(output_tokens).unwrap_or(0),
                    error,
                });
            }
            Ok(lanes)
        })
    }

    async fn record_finding(
        &self,
        run: &RunId,
        lane: &str,
        path: &str,
        line_number: u32,
        comment_id: &str,
        action: FindingAction,
    ) -> Result<(), StoreError> {
        self.with(|c| {
            c.execute(
                "INSERT INTO findings (run_id, lane, path, line, comment_id, action, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![run.as_str(), lane, path, line_number, comment_id, action.as_str(), now()],
            )?;
            Ok(())
        })
    }

    async fn findings(&self, run: &RunId) -> Result<Vec<FindingRecord>, StoreError> {
        self.with(|c| {
            let mut statement = c.prepare(
                "SELECT created_at, lane, path, line, comment_id, action FROM findings WHERE run_id = ?1 ORDER BY id",
            )?;
            let rows = statement.query_map(params![run.as_str()], |row| {
                Ok(FindingRecord {
                    at: row.get(0)?,
                    lane: row.get(1)?,
                    path: row.get(2)?,
                    line: row.get(3)?,
                    comment_id: row.get(4)?,
                    action: row.get(5)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        })
    }

    async fn event(&self, run: &RunId, level: &str, message: &str) -> Result<(), StoreError> {
        self.with(|c| {
            c.execute(
                "INSERT INTO events (run_id, at, level, message) VALUES (?1, ?2, ?3, ?4)",
                params![run.as_str(), now(), level, message],
            )?;
            Ok(())
        })
    }

    async fn events(&self, run: &RunId) -> Result<Vec<EventRecord>, StoreError> {
        self.with(|c| {
            let mut statement =
                c.prepare("SELECT at, level, message FROM events WHERE run_id = ?1 ORDER BY id")?;
            let rows = statement.query_map(params![run.as_str()], |row| {
                Ok(EventRecord {
                    at: row.get(0)?,
                    level: row.get(1)?,
                    message: row.get(2)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(StoreError::from)
        })
    }

    async fn joined(&self, run: &RunId, source: &str) -> Result<(), StoreError> {
        self.with(|c| {
            c.execute(
                "INSERT INTO requests (run_id, at, source) VALUES (?1, ?2, ?3)",
                params![run.as_str(), now(), source],
            )?;
            Ok(())
        })
    }

    async fn record_event(&self, event: &InboundEvent) -> Result<(), StoreError> {
        let payload = event
            .payload
            .as_deref()
            .filter(|p| p.len() <= MAX_PAYLOAD_BYTES);
        self.with(|c| {
            c.execute(
                "INSERT INTO inbound_events (id, received_at, source, kind, repo, target, payload) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    event.id.as_str(),
                    event.received_at,
                    event.source,
                    event.kind,
                    event.repo,
                    event.target.map(|t| i64::try_from(t).unwrap_or(i64::MAX)),
                    payload,
                ],
            )?;
            Ok(())
        })
    }

    async fn record_outcome(&self, outcome: &OutcomeRecord) -> Result<(), StoreError> {
        self.with(|c| {
            c.execute(
                "INSERT INTO event_outcomes (event_id, listener, outcome, detail, run_id, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    outcome.event_id.as_str(),
                    outcome.listener,
                    outcome.outcome,
                    outcome.detail,
                    outcome.run_id,
                    if outcome.at.is_empty() { now() } else { outcome.at.clone() },
                ],
            )?;
            Ok(())
        })
    }

    async fn inbound_event(&self, id: &EventId) -> Result<Option<InboundEvent>, StoreError> {
        self.with(|c| {
            c.query_row(
                "SELECT id, received_at, source, kind, repo, target, payload FROM inbound_events WHERE id = ?1",
                params![id.as_str()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<i64>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                    ))
                },
            )
            .optional()?
            .map(|(id, received_at, source, kind, repo, target, payload)| {
                let id = EventId::parse(id.clone()).map_err(|_| StoreError::Corrupt { column: "inbound_events.id", value: id })?;
                Ok(InboundEvent {
                    id,
                    received_at,
                    source,
                    kind,
                    repo,
                    target: target.and_then(|t| u64::try_from(t).ok()),
                    payload,
                })
            })
            .transpose()
        })
    }

    async fn outcomes(&self, id: &EventId) -> Result<Vec<OutcomeRecord>, StoreError> {
        self.with(|c| {
            let mut statement = c.prepare(
                "SELECT listener, outcome, detail, run_id, at FROM event_outcomes WHERE event_id = ?1 ORDER BY rowid",
            )?;
            let rows = statement.query_map(params![id.as_str()], |row| {
                Ok(OutcomeRecord {
                    event_id: id.clone(),
                    listener: row.get(0)?,
                    outcome: row.get(1)?,
                    detail: row.get(2)?,
                    run_id: row.get(3)?,
                    at: row.get(4)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::from)
        })
    }

    async fn inbound_events_for_run(&self, run: &RunId) -> Result<Vec<InboundEvent>, StoreError> {
        let ids: Vec<String> = self.with(|c| {
            let mut statement = c.prepare(
                "SELECT DISTINCT e.id FROM inbound_events e JOIN event_outcomes o ON o.event_id = e.id WHERE o.run_id = ?1 ORDER BY e.received_at",
            )?;
            let rows = statement.query_map(params![run.as_str()], |row| row.get(0))?;
            rows.collect::<Result<Vec<_>, _>>().map_err(StoreError::from)
        })?;
        let mut events = Vec::new();
        for id in ids {
            let id = EventId::parse(id.clone()).map_err(|_| StoreError::Corrupt {
                column: "inbound_events.id",
                value: id,
            })?;
            if let Some(event) = self.inbound_event(&id).await? {
                events.push(event);
            }
        }
        Ok(events)
    }
}

/// The columns [`raw_run`] reads, in order.
const RUN_COLUMNS: &str = "id, kind, platform, repo, target, commit_sha, requester, trigger, status, started_at, finished_at, link, summary, error, heartbeat_at, check_id";

fn raw_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRun> {
    Ok(RawRun {
        id: row.get(0)?,
        kind: row.get(1)?,
        platform: row.get(2)?,
        repo: row.get(3)?,
        target: row.get(4)?,
        commit: row.get(5)?,
        requester: row.get(6)?,
        trigger: row.get(7)?,
        status: row.get(8)?,
        started_at: row.get(9)?,
        finished_at: row.get(10)?,
        link: row.get(11)?,
        summary: row.get(12)?,
        error: row.get(13)?,
        heartbeat_at: row.get(14)?,
        check_id: row.get(15)?,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use henk_domain::allowlist::Platform;
    use henk_domain::run::RunKind;

    use super::*;

    fn new_run(id: &str) -> NewRun {
        NewRun {
            id: RunId::parse(id).unwrap(),
            kind: RunKind::Review,
            platform: Platform::GitHub,
            repo: "o/r".into(),
            target: 7,
            commit: Some("abc".into()),
            requester: None,
            trigger: "opened".into(),
            link: format!("https://henk.example/runs/{id}"),
        }
    }

    #[tokio::test]
    async fn a_run_that_never_had_a_heartbeat_is_orphaned() {
        let store = SqliteStore::in_memory().unwrap();
        store.create_run(&new_run("r-silent")).await.unwrap();
        store
            .with(|c| {
                c.execute(
                    "UPDATE runs SET heartbeat_at = NULL WHERE id = 'r-silent'",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        let an_hour_ago = OffsetDateTime::now_utc() - time::Duration::hours(1);
        let orphans = store.orphaned_runs(an_hour_ago).await.unwrap();
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans.first().unwrap().id.as_str(), "r-silent");
    }

    #[tokio::test]
    async fn joining_a_running_run_is_recorded() {
        let store = SqliteStore::in_memory().unwrap();
        store.create_run(&new_run("r-3")).await.unwrap();
        let run = RunId::parse("r-3").unwrap();
        store.joined(&run, "comment").await.unwrap();
        store.joined(&run, "webhook").await.unwrap();
        let sources: Vec<String> = store
            .with(|c| {
                let mut statement =
                    c.prepare("SELECT source FROM requests WHERE run_id = ?1 ORDER BY rowid")?;
                let rows = statement.query_map(params![run.as_str()], |row| row.get(0))?;
                Ok(rows.collect::<Result<_, _>>()?)
            })
            .unwrap();
        assert_eq!(
            sources,
            ["comment", "webhook"],
            "one row per join, in order"
        );
    }
}
