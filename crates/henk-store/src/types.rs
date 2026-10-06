//! The records and statuses every backend stores and returns.

use henk_domain::allowlist::Platform;
use henk_domain::run::{EventId, RunId, RunKind};
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
    /// `PostgreSQL` said no, or no connection could be had. The text is the
    /// whole cause chain: the driver's own `Display` is only "db error".
    #[error("PostgreSQL: {0}")]
    Postgres(String),
    /// The database could not be set up: a bad URL, TLS or pool settings.
    #[error("database setup: {0}")]
    Connect(String),
    /// The schema is not one this Henk can work with.
    #[error("database schema: {0}")]
    Schema(String),
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

impl From<tokio_postgres::Error> for StoreError {
    fn from(error: tokio_postgres::Error) -> Self {
        Self::Postgres(cause_chain(&error))
    }
}

impl From<deadpool_postgres::PoolError> for StoreError {
    fn from(error: deadpool_postgres::PoolError) -> Self {
        Self::Postgres(cause_chain(&error))
    }
}

/// An error and its sources as one line, each distinct message once.
fn cause_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut next = Some(error);
    while let Some(error) = next {
        let text = error.to_string();
        if !parts.iter().any(|seen| seen.contains(&text)) {
            parts.push(text);
        }
        next = error.source();
    }
    parts.join(": ")
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
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Finished => "finished",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
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
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Finished => "finished",
            Self::Dropped => "dropped",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
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
    /// The fact-check found the finding wrong; nothing was posted or changed.
    Rejected,
    /// The fact-check could not run; the finding was posted unchecked.
    Unverified,
    /// A finding was withdrawn as wrong: its text replaced, its thread resolved.
    Withdrawn,
}

impl FindingAction {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Posted => "posted",
            Self::Improved => "improved",
            Self::Refused => "refused",
            Self::Rejected => "rejected",
            Self::Unverified => "unverified",
            Self::Withdrawn => "withdrawn",
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
    /// RFC 3339: when the process running it last said it was alive.
    pub heartbeat_at: Option<String>,
    /// The platform's id for the review's check, once it has one.
    pub check_id: Option<String>,
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

/// Now, as RFC 3339.
#[must_use]
pub(crate) fn now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

pub(crate) fn kind_str(kind: RunKind) -> &'static str {
    match kind {
        RunKind::Review => "review",
        RunKind::Plan => "plan",
        RunKind::DiscordTurn => "discord_turn",
        RunKind::MailReply => "mail_reply",
    }
}

pub(crate) fn kind_parse(value: &str) -> Option<RunKind> {
    Some(match value {
        "review" => RunKind::Review,
        "plan" => RunKind::Plan,
        "discord_turn" => RunKind::DiscordTurn,
        "mail_reply" => RunKind::MailReply,
        _ => return None,
    })
}

pub(crate) fn platform_str(platform: Platform) -> &'static str {
    match platform {
        Platform::GitHub => "github",
        Platform::GitLab => "gitlab",
    }
}

pub(crate) fn platform_parse(value: &str) -> Option<Platform> {
    Some(match value {
        "github" => Platform::GitHub,
        "gitlab" => Platform::GitLab,
        _ => return None,
    })
}

/// A number for a signed 64-bit column. Beyond `i64` is refused, on every
/// backend alike, rather than stored as something else.
pub(crate) fn to_i64(column: &'static str, value: u64) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::Corrupt {
        column,
        value: value.to_string(),
    })
}

/// A signed column read back as unsigned; a negative value is corrupt.
pub(crate) fn to_u64(column: &'static str, value: i64) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| StoreError::Corrupt {
        column,
        value: value.to_string(),
    })
}

/// A run row as a backend read it, before its values are checked.
pub(crate) struct RawRun {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) platform: String,
    pub(crate) repo: String,
    pub(crate) target: i64,
    pub(crate) commit: Option<String>,
    pub(crate) requester: Option<String>,
    pub(crate) trigger: String,
    pub(crate) status: String,
    pub(crate) started_at: String,
    pub(crate) finished_at: Option<String>,
    pub(crate) link: String,
    pub(crate) summary: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) heartbeat_at: Option<String>,
    pub(crate) check_id: Option<String>,
}

impl RawRun {
    /// Checks every value and builds the record.
    pub(crate) fn into_record(self) -> Result<RunRecord, StoreError> {
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
            target: to_u64("runs.target", self.target)?,
            commit: self.commit,
            requester: self.requester,
            trigger: self.trigger,
            status,
            started_at: self.started_at,
            finished_at: self.finished_at,
            link: self.link,
            summary: self.summary,
            error: self.error,
            heartbeat_at: self.heartbeat_at,
            check_id: self.check_id,
        })
    }
}
