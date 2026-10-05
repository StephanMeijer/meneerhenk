//! The run store.

use std::path::Path;
use std::sync::Mutex;

use henk_domain::allowlist::Platform;
use henk_domain::run::{EventId, RunId, RunKind};
use rusqlite::{Connection, OptionalExtension as _, params};
use rusqlite_migration::{M, Migrations};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Why a store operation failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// SQLite said no.
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The schema could not be brought up to date.
    #[error("migration error: {0}")]
    Migration(#[from] rusqlite_migration::Error),
    /// The connection mutex was poisoned by a panic elsewhere.
    #[error("store lock poisoned")]
    Poisoned,
    /// A stored value could not be read back as its type.
    #[error("corrupt value in column {column}: {value}")]
    Corrupt {
        /// Column name.
        column: &'static str,
        /// What was there.
        value: String,
    },
}

/// Where a run stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    /// Still going.
    Running,
    /// Ended on its own terms.
    Finished,
    /// Ended because something broke.
    Failed,
    /// Ended because a newer request superseded it.
    Cancelled,
}

impl RunStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Finished => "finished",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "running" => Self::Running,
            "finished" => Self::Finished,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }
}

/// Where a lane stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneStatus {
    /// Still going.
    Running,
    /// Finished cleanly.
    Finished,
    /// Dropped after failure, timeout or cancellation.
    Dropped,
}

impl LaneStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Finished => "finished",
            Self::Dropped => "dropped",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "running" => Self::Running,
            "finished" => Self::Finished,
            "dropped" => Self::Dropped,
            _ => return None,
        })
    }
}

/// What happened to a finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingAction {
    /// A new comment was posted.
    Posted,
    /// An existing comment was improved.
    Improved,
    /// The lane wanted to post but the line was taken or the text refused.
    Refused,
}

impl FindingAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Posted => "posted",
            Self::Improved => "improved",
            Self::Refused => "refused",
        }
    }
}

/// What a new run needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRun {
    /// Its id.
    pub id: RunId,
    /// Review, plan, and so on.
    pub kind: RunKind,
    /// Platform.
    pub platform: Platform,
    /// `owner/name`.
    pub repo: String,
    /// Pull/merge request number or issue number.
    pub target: u64,
    /// The reviewed commit, for reviews.
    pub commit: Option<String>,
    /// Who asked, as a stable id, when someone did.
    pub requester: Option<String>,
    /// What started it, in words.
    pub trigger: String,
    /// The public link to this run.
    pub link: String,
}

/// A stored run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRecord {
    /// Its id.
    pub id: RunId,
    /// Review, plan, and so on.
    pub kind: RunKind,
    /// Platform.
    pub platform: Platform,
    /// `owner/name`.
    pub repo: String,
    /// Pull/merge request number or issue number.
    pub target: u64,
    /// The reviewed commit, for reviews.
    pub commit: Option<String>,
    /// Who asked.
    pub requester: Option<String>,
    /// What started it.
    pub trigger: String,
    /// Where it stands.
    pub status: RunStatus,
    /// RFC 3339.
    pub started_at: String,
    /// RFC 3339, once ended.
    pub finished_at: Option<String>,
    /// The public link.
    pub link: String,
    /// The summary text, once there is one.
    pub summary: Option<String>,
    /// The error, when it failed.
    pub error: Option<String>,
}

/// A stored lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneRecord {
    /// Lane name.
    pub name: String,
    /// Model name.
    pub model: String,
    /// Where it stands.
    pub status: LaneStatus,
    /// Model calls made.
    pub turns: u64,
    /// Tokens in.
    pub input_tokens: u64,
    /// Tokens out.
    pub output_tokens: u64,
    /// The error, when dropped.
    pub error: Option<String>,
}

/// One recorded finding action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindingRecord {
    /// RFC 3339.
    pub at: String,
    /// The lane.
    pub lane: String,
    /// File path.
    pub path: String,
    /// Line number.
    pub line: u32,
    /// The platform comment id.
    pub comment_id: String,
    /// `posted`, `improved` or `refused`.
    pub action: String,
}

/// One event on a run's timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRecord {
    /// RFC 3339.
    pub at: String,
    /// `info`, `warn` or `error`.
    pub level: String,
    /// What happened.
    pub message: String,
}

/// A recorded inbound event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundEvent {
    /// Its id.
    pub id: EventId,
    /// RFC 3339.
    pub received_at: String,
    /// Source name.
    pub source: String,
    /// Kind name.
    pub kind: String,
    /// `owner/name`, when the event is about a repository.
    pub repo: Option<String>,
    /// Pull/merge request or issue number, when about one.
    pub target: Option<u64>,
    /// The raw payload as received, when recorded.
    pub payload: Option<String>,
}

/// What one listener did with an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutcomeRecord {
    /// The event.
    pub event_id: EventId,
    /// Listener name.
    pub listener: String,
    /// Outcome name.
    pub outcome: String,
    /// Detail text.
    pub detail: String,
    /// The run it led to, if any.
    pub run_id: Option<String>,
    /// RFC 3339.
    pub at: String,
}

/// Payloads larger than this are not recorded; the event still is.
pub const MAX_PAYLOAD_BYTES: usize = 256 * 1024;

/// The store.
#[derive(Debug)]
pub struct RunStore {
    connection: Mutex<Connection>,
}

fn now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

fn migrations() -> Migrations<'static> {
    Migrations::new(vec![
        M::up(include_str!("../migrations/001_initial.sql")),
        M::up(include_str!("../migrations/002_inbound_events.sql")),
    ])
}

impl RunStore {
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

    /// Records a new run as running.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure, including a duplicate id.
    pub fn create_run(&self, run: &NewRun) -> Result<(), StoreError> {
        self.with(|c| {
            c.execute(
                "INSERT INTO runs (id, kind, platform, repo, target, commit_sha, requester, trigger, status, started_at, link)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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

    /// Ends a run.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure.
    pub fn finish_run(
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

    /// Reads a run.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure or a corrupt row.
    pub fn run(&self, id: &RunId) -> Result<Option<RunRecord>, StoreError> {
        self.with(|c| {
            c.query_row(
                "SELECT id, kind, platform, repo, target, commit_sha, requester, trigger, status, started_at, finished_at, link, summary, error
                 FROM runs WHERE id = ?1",
                params![id.as_str()],
                |row| {
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
                    })
                },
            )
            .optional()?
            .map(RawRun::into_record)
            .transpose()
        })
    }

    /// Records a lane as running.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure.
    pub fn start_lane(&self, run: &RunId, name: &str, model: &str) -> Result<(), StoreError> {
        self.with(|c| {
            c.execute(
                "INSERT INTO lanes (run_id, name, model, status, started_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![run.as_str(), name, model, LaneStatus::Running.as_str(), now()],
            )?;
            Ok(())
        })
    }

    /// Ends a lane.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure.
    #[allow(clippy::too_many_arguments)]
    pub fn finish_lane(
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

    /// The lanes of a run.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure or a corrupt row.
    pub fn lanes(&self, run: &RunId) -> Result<Vec<LaneRecord>, StoreError> {
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

    /// Records what happened to a finding.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure.
    pub fn record_finding(
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

    /// The finding actions of a run, oldest first.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure.
    pub fn findings(&self, run: &RunId) -> Result<Vec<FindingRecord>, StoreError> {
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

    /// Adds a line to a run's timeline.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure.
    pub fn event(&self, run: &RunId, level: &str, message: &str) -> Result<(), StoreError> {
        self.with(|c| {
            c.execute(
                "INSERT INTO events (run_id, at, level, message) VALUES (?1, ?2, ?3, ?4)",
                params![run.as_str(), now(), level, message],
            )?;
            Ok(())
        })
    }

    /// The timeline of a run, oldest first.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure.
    pub fn events(&self, run: &RunId) -> Result<Vec<EventRecord>, StoreError> {
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

    /// Notes that another request joined a running run.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure.
    pub fn joined(&self, run: &RunId, source: &str) -> Result<(), StoreError> {
        self.with(|c| {
            c.execute(
                "INSERT INTO requests (run_id, at, source) VALUES (?1, ?2, ?3)",
                params![run.as_str(), now(), source],
            )?;
            Ok(())
        })
    }

    /// The most recent running review for a target, if any.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure or a corrupt row.
    pub fn running_review(
        &self,
        platform: Platform,
        repo: &str,
        target: u64,
    ) -> Result<Option<RunRecord>, StoreError> {
        let id: Option<String> = self.with(|c| {
            c.query_row(
                "SELECT id FROM runs WHERE kind = 'review' AND platform = ?1 AND repo = ?2 AND target = ?3 AND status = 'running'
                 ORDER BY started_at DESC LIMIT 1",
                params![platform_str(platform), repo, i64::try_from(target).unwrap_or(i64::MAX)],
                |row| row.get(0),
            )
            .optional()
            .map_err(StoreError::from)
        })?;
        match id {
            Some(id) => {
                let id = RunId::parse(id.clone()).map_err(|_| StoreError::Corrupt {
                    column: "runs.id",
                    value: id,
                })?;
                self.run(&id)
            }
            None => Ok(None),
        }
    }
}

impl RunStore {
    /// Records an inbound event. A payload over [`MAX_PAYLOAD_BYTES`] is dropped.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure, including a duplicate id.
    pub fn record_event(&self, event: &InboundEvent) -> Result<(), StoreError> {
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

    /// Records what a listener did with an event.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure.
    pub fn record_outcome(&self, outcome: &OutcomeRecord) -> Result<(), StoreError> {
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

    /// Reads one event.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure or a corrupt row.
    pub fn inbound_event(&self, id: &EventId) -> Result<Option<InboundEvent>, StoreError> {
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

    /// The outcomes of one event, in recording order.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure.
    pub fn outcomes(&self, id: &EventId) -> Result<Vec<OutcomeRecord>, StoreError> {
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

    /// The events whose outcomes point at a run, oldest first.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on a database failure or a corrupt row.
    pub fn inbound_events_for_run(&self, run: &RunId) -> Result<Vec<InboundEvent>, StoreError> {
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
            if let Some(event) = self.inbound_event(&id)? {
                events.push(event);
            }
        }
        Ok(events)
    }
}

fn kind_str(kind: RunKind) -> &'static str {
    match kind {
        RunKind::Review => "review",
        RunKind::Plan => "plan",
        RunKind::DiscordTurn => "discord_turn",
        RunKind::MailReply => "mail_reply",
    }
}

fn kind_parse(value: &str) -> Option<RunKind> {
    Some(match value {
        "review" => RunKind::Review,
        "plan" => RunKind::Plan,
        "discord_turn" => RunKind::DiscordTurn,
        "mail_reply" => RunKind::MailReply,
        _ => return None,
    })
}

fn platform_str(platform: Platform) -> &'static str {
    match platform {
        Platform::GitHub => "github",
        Platform::GitLab => "gitlab",
    }
}

fn platform_parse(value: &str) -> Option<Platform> {
    Some(match value {
        "github" => Platform::GitHub,
        "gitlab" => Platform::GitLab,
        _ => return None,
    })
}

struct RawRun {
    id: String,
    kind: String,
    platform: String,
    repo: String,
    target: i64,
    commit: Option<String>,
    requester: Option<String>,
    trigger: String,
    status: String,
    started_at: String,
    finished_at: Option<String>,
    link: String,
    summary: Option<String>,
    error: Option<String>,
}

impl RawRun {
    fn into_record(self) -> Result<RunRecord, StoreError> {
        let id = RunId::parse(self.id.clone()).map_err(|_| StoreError::Corrupt {
            column: "runs.id",
            value: self.id,
        })?;
        let kind = kind_parse(&self.kind).ok_or(StoreError::Corrupt {
            column: "runs.kind",
            value: self.kind,
        })?;
        let platform = platform_parse(&self.platform).ok_or(StoreError::Corrupt {
            column: "runs.platform",
            value: self.platform,
        })?;
        let status = RunStatus::parse(&self.status).ok_or(StoreError::Corrupt {
            column: "runs.status",
            value: self.status,
        })?;
        Ok(RunRecord {
            id,
            kind,
            platform,
            repo: self.repo,
            target: u64::try_from(self.target).unwrap_or(0),
            commit: self.commit,
            requester: self.requester,
            trigger: self.trigger,
            status,
            started_at: self.started_at,
            finished_at: self.finished_at,
            link: self.link,
            summary: self.summary,
            error: self.error,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::panic,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing
    )]

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

    #[test]
    fn run_round_trips() {
        let store = RunStore::in_memory().unwrap();
        store.create_run(&new_run("r-1")).unwrap();
        let run = store.run(&RunId::parse("r-1").unwrap()).unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Running);
        assert_eq!(run.repo, "o/r");
        assert_eq!(run.target, 7);
        assert!(run.finished_at.is_none());

        store
            .finish_run(&run.id, RunStatus::Finished, Some("No issues found."), None)
            .unwrap();
        let run = store.run(&run.id).unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Finished);
        assert_eq!(run.summary.as_deref(), Some("No issues found."));
        assert!(run.finished_at.is_some());
        assert!(store.run(&RunId::parse("nope").unwrap()).unwrap().is_none());
    }

    #[test]
    fn lanes_findings_and_events_attach_to_a_run() {
        let store = RunStore::in_memory().unwrap();
        let run = new_run("r-2");
        store.create_run(&run).unwrap();
        store.start_lane(&run.id, "a", "model-x").unwrap();
        store.start_lane(&run.id, "b", "model-y").unwrap();
        store
            .finish_lane(&run.id, "a", LaneStatus::Finished, 4, 1000, 200, None)
            .unwrap();
        store
            .finish_lane(&run.id, "b", LaneStatus::Dropped, 1, 10, 0, Some("timeout"))
            .unwrap();
        store
            .record_finding(&run.id, "a", "src/x.rs", 12, "c1", FindingAction::Posted)
            .unwrap();
        store.event(&run.id, "info", "started").unwrap();

        let lanes = store.lanes(&run.id).unwrap();
        assert_eq!(lanes.len(), 2);
        assert_eq!(lanes[0].status, LaneStatus::Finished);
        assert_eq!(lanes[0].input_tokens, 1000);
        assert_eq!(lanes[1].error.as_deref(), Some("timeout"));
        assert_eq!(store.events(&run.id).unwrap()[0].message, "started");
        let findings = store.findings(&run.id).unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].lane, "a");
        assert_eq!(findings[0].path, "src/x.rs");
        assert_eq!(findings[0].line, 12);
        assert_eq!(findings[0].comment_id, "c1");
        assert_eq!(findings[0].action, "posted");
        assert!(
            store
                .findings(&RunId::parse("r-none").unwrap())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn events_and_outcomes_round_trip() {
        let store = RunStore::in_memory().unwrap();
        store.create_run(&new_run("r-9")).unwrap();
        let id = EventId::parse("e-1").unwrap();
        store
            .record_event(&InboundEvent {
                id: id.clone(),
                received_at: "2026-10-03T00:00:00Z".into(),
                source: "github_webhook".into(),
                kind: "pull_request".into(),
                repo: Some("o/r".into()),
                target: Some(7),
                payload: Some("{}".into()),
            })
            .unwrap();
        store
            .record_outcome(&OutcomeRecord {
                event_id: id.clone(),
                listener: "review".into(),
                outcome: "started".into(),
                detail: "r-9".into(),
                run_id: Some("r-9".into()),
                at: String::new(),
            })
            .unwrap();
        let event = store.inbound_event(&id).unwrap().unwrap();
        assert_eq!(event.kind, "pull_request");
        assert_eq!(event.target, Some(7));
        assert_eq!(event.payload.as_deref(), Some("{}"));
        let outcomes = store.outcomes(&id).unwrap();
        assert_eq!(outcomes.len(), 1);
        assert!(!outcomes[0].at.is_empty());
        let linked = store
            .inbound_events_for_run(&RunId::parse("r-9").unwrap())
            .unwrap();
        assert_eq!(linked.len(), 1);
        assert!(
            store
                .inbound_event(&EventId::parse("e-nope").unwrap())
                .unwrap()
                .is_none()
        );

        let big = "x".repeat(MAX_PAYLOAD_BYTES + 1);
        store
            .record_event(&InboundEvent {
                id: EventId::parse("e-2").unwrap(),
                payload: Some(big),
                ..event
            })
            .unwrap();
        assert!(
            store
                .inbound_event(&EventId::parse("e-2").unwrap())
                .unwrap()
                .unwrap()
                .payload
                .is_none(),
            "oversized payload dropped"
        );
    }

    #[test]
    fn running_review_finds_the_active_one_only() {
        let store = RunStore::in_memory().unwrap();
        store.create_run(&new_run("r-3")).unwrap();
        assert_eq!(
            store
                .running_review(Platform::GitHub, "o/r", 7)
                .unwrap()
                .unwrap()
                .id
                .as_str(),
            "r-3"
        );
        assert!(
            store
                .running_review(Platform::GitHub, "o/r", 8)
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .running_review(Platform::GitLab, "o/r", 7)
                .unwrap()
                .is_none()
        );
        store
            .finish_run(
                &RunId::parse("r-3").unwrap(),
                RunStatus::Cancelled,
                None,
                None,
            )
            .unwrap();
        assert!(
            store
                .running_review(Platform::GitHub, "o/r", 7)
                .unwrap()
                .is_none()
        );
        store
            .joined(&RunId::parse("r-3").unwrap(), "comment")
            .unwrap();
    }
}
