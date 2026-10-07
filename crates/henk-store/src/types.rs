//! The records and statuses every backend stores and returns.

use std::collections::BTreeMap;

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
    /// Ended because a person cancelled it from the dashboard (#69). A
    /// review superseded by a newer commit ends `Failed`, with the reason.
    Cancelled,
}

impl RunStatus {
    /// The stored text: `running`, `finished`, `failed` or `cancelled`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
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
    /// The stored text: `running`, `finished` or `dropped`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
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
    /// The fact-check found it repeats another draft or an existing finding;
    /// nothing was written for it (#189).
    Merged,
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
            Self::Merged => "merged",
        }
    }
}

/// A review lane's draft as it was queued (#189), with its decision once
/// the fact-check has settled it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftRecord {
    /// RFC 3339; empty when recording means now.
    pub at: String,
    /// Its number in the review: `d3`.
    pub draft: String,
    /// The lane that wrote it.
    pub lane: String,
    /// The lane's model.
    pub model: String,
    /// `finding`, `rewrite` or `withdrawal`.
    pub kind: String,
    /// The file.
    pub path: String,
    /// The line.
    pub line: u32,
    /// The existing comment a rewrite or withdrawal is about; empty for a
    /// new finding.
    pub target: String,
    /// The finding, the new text, or why to withdraw.
    pub body: String,
    /// What became of it; none while it waits.
    pub decision: Option<DraftDecision>,
}

/// What became of a draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftDecision {
    /// RFC 3339; empty when recording means now.
    pub at: String,
    /// The verdict.
    pub verdict: DraftVerdict,
    /// The model that gave it; empty when none did.
    pub checker: String,
    /// Why, in the checker's words, or why it went unchecked.
    pub reason: String,
    /// What it repeats (`d2` or a comment id), for a merge.
    pub same_as: String,
    /// The comment written for it, or the one it was merged into.
    pub comment_id: String,
}

/// The verdict on a draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftVerdict {
    /// Confirmed and written.
    Confirmed,
    /// Rejected; nothing written.
    Rejected,
    /// A repeat of another draft or an existing finding; merged into it.
    SameAs,
    /// No model could check it; written unchecked.
    Unchecked,
    /// No fact-check is configured; written as the lane wrote it.
    NotChecked,
    /// The review ended before it was settled; nothing written.
    Cancelled,
    /// The write to the platform failed.
    Failed,
}

/// A stored verdict, refused when it is not one.
pub(crate) fn draft_verdict(text: &str) -> Result<DraftVerdict, StoreError> {
    DraftVerdict::parse(text).ok_or_else(|| StoreError::Corrupt {
        column: "drafts.verdict",
        value: text.to_owned(),
    })
}

impl DraftVerdict {
    /// The stored text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Confirmed => "confirmed",
            Self::Rejected => "rejected",
            Self::SameAs => "same_as",
            Self::Unchecked => "unchecked",
            Self::NotChecked => "not_checked",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }

    /// Reads the stored text back.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        [
            Self::Confirmed,
            Self::Rejected,
            Self::SameAs,
            Self::Unchecked,
            Self::NotChecked,
            Self::Cancelled,
            Self::Failed,
        ]
        .into_iter()
        .find(|verdict| verdict.as_str() == text)
    }
}

/// One session's whole conversation, as the run record keeps it (#191).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptRecord {
    /// RFC 3339; empty when recording means now.
    pub at: String,
    /// The session: the lane row's name.
    pub session: String,
    /// The session's model.
    pub model: String,
    /// How the session stopped.
    pub stop: String,
    /// Model calls made.
    pub turns: u32,
    /// The body's size in bytes.
    pub bytes: u64,
    /// The transcript as JSON: system prompt, messages, tool calls and
    /// results, usage. Whole, never cut.
    pub body: String,
}

/// A transcript without its body, for listing what a run has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptSummary {
    /// RFC 3339.
    pub at: String,
    /// The session.
    pub session: String,
    /// The session's model.
    pub model: String,
    /// How it stopped.
    pub stop: String,
    /// Model calls made.
    pub turns: u32,
    /// The body's size in bytes.
    pub bytes: u64,
}

/// One tool call of a session, as the run record keeps it (#190).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallRecord {
    /// RFC 3339; empty when recording means now.
    pub at: String,
    /// The session: the lane row's name (`lane-a`, `check-2`,
    /// `planner`, `address`).
    pub session: String,
    /// The session's model.
    pub model: String,
    /// The turn the call was made in, from 1.
    pub turn: u32,
    /// The model-facing tool name.
    pub tool: String,
    /// Where the tool comes from: `henk`, `workspace` or an MCP alias.
    pub origin: String,
    /// How it ended: `ok`, `error`, `refused_scope`, `refused_repeat`,
    /// `unknown_tool`, `malformed_arguments`, `not_run` or `cancelled`.
    pub outcome: String,
    /// The arguments as the model sent them, perhaps cut.
    pub arguments: String,
    /// The arguments' length in bytes before any cut.
    pub arguments_len: u64,
    /// Characters of the result before it was cut for the model.
    pub result_chars: u64,
    /// How long it ran.
    pub elapsed_ms: u64,
}

/// What kind of session a session name is: `check` for a fact-check,
/// `planner`, `address`, and `lane` for a review lane.
#[must_use]
pub fn session_kind(session: &str) -> &'static str {
    if session.starts_with("check-") {
        "check"
    } else if session == "planner" {
        "planner"
    } else if session == "address" {
        "address"
    } else {
        "lane"
    }
}

/// How many calls of one tool ended how, and their time together.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolTally {
    /// Every call.
    pub calls: u64,
    /// Calls the tool reported as failed.
    pub errors: u64,
    /// Calls a guard refused: the scope guard or the repeat guard.
    pub refusals: u64,
    /// Calls that never ran for another reason: an unknown tool, malformed
    /// arguments, a session that was ending or cancelled.
    pub other: u64,
    /// Milliseconds the calls ran, together.
    pub total_ms: u64,
}

impl ToolTally {
    /// Counts `n` calls that ended as `outcome` and ran `ms` together.
    pub fn add(&mut self, outcome: &str, n: u64, ms: u64) {
        self.calls += n;
        self.total_ms += ms;
        match outcome {
            "ok" => {}
            "error" => self.errors += n,
            "refused_scope" | "refused_repeat" => self.refusals += n,
            _ => self.other += n,
        }
    }
}

/// One row of tool usage: one tool of one session, or across runs one tool
/// of one kind of session on one model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolUsage {
    /// A session name for one run; a session kind ([`session_kind`]) across
    /// runs.
    pub session: String,
    /// The model; empty for one run, where the lane says it.
    pub model: String,
    /// The tool.
    pub tool: String,
    /// What its calls came to.
    pub tally: ToolTally,
}

impl ToolUsage {
    /// The calls of one run, per session and tool, in session and tool
    /// order.
    #[must_use]
    pub fn from_calls(calls: &[ToolCallRecord]) -> Vec<Self> {
        let mut usage: BTreeMap<(String, String), ToolTally> = BTreeMap::new();
        for call in calls {
            usage
                .entry((call.session.clone(), call.tool.clone()))
                .or_default()
                .add(&call.outcome, 1, call.elapsed_ms);
        }
        usage
            .into_iter()
            .map(|((session, tool), tally)| Self {
                session,
                model: String::new(),
                tool,
                tally,
            })
            .collect()
    }

    /// Rows grouped by (model, session, tool, outcome) with a count and a
    /// sum of milliseconds, folded per model, kind of session and tool.
    pub(crate) fn across_runs(rows: Vec<(String, String, String, String, u64, u64)>) -> Vec<Self> {
        let mut usage: BTreeMap<(String, &'static str, String), ToolTally> = BTreeMap::new();
        for (model, session, tool, outcome, n, ms) in rows {
            usage
                .entry((model, session_kind(&session), tool))
                .or_default()
                .add(&outcome, n, ms);
        }
        usage
            .into_iter()
            .map(|((model, kind, tool), tally)| Self {
                session: kind.to_owned(),
                model,
                tool,
                tally,
            })
            .collect()
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
    /// Who asked, when the source knows: the API's requester or
    /// `github:<id>` from the dashboard (#69). Never a display name.
    pub requester: Option<String>,
}

/// What one pruning pass deleted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneCounts {
    /// Inbound events deleted.
    pub events: u64,
    /// Outcomes of those events deleted.
    pub outcomes: u64,
    /// Session transcripts deleted (#191).
    pub transcripts: u64,
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

/// Which runs a listing shows. `None` matches anything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunFilter {
    /// Review, plan, and so on.
    pub kind: Option<RunKind>,
    /// Where it stands.
    pub status: Option<RunStatus>,
    /// Platform.
    pub platform: Option<Platform>,
    /// `owner/name`, exactly.
    pub repo: Option<String>,
    /// The pull request, merge request or issue number.
    pub target: Option<u64>,
    /// Started at or after this RFC 3339 time.
    pub since: Option<String>,
    /// Started before this RFC 3339 time.
    pub until: Option<String>,
    /// Only runs listed after this one: for keyset paging, newest first.
    pub before: Option<RunKey>,
}

/// Where a run sits in a newest-first listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunKey {
    /// The run's `started_at`, as the store returned it.
    pub started_at: String,
    /// The run's id, which breaks a tie on the time.
    pub id: String,
}

/// Where an inbound event sits in a newest-first listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventKey {
    /// The event's `received_at`, as the store returned it.
    pub received_at: String,
    /// The event's id, which breaks a tie on the time.
    pub id: String,
}

/// Which inbound events a listing shows. `None` matches anything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventFilter {
    /// Source name, such as `github_webhook` or `api`.
    pub source: Option<String>,
    /// Kind name, such as `pull_request`.
    pub kind: Option<String>,
    /// `owner/name`, exactly.
    pub repo: Option<String>,
    /// Only events listed after this one: for keyset paging, newest first.
    pub before: Option<EventKey>,
}

/// One page of a listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    limit: u32,
    offset: u32,
}

impl Page {
    /// The most rows one page holds.
    pub const MAX: u32 = 100;

    /// `limit` rows from `offset`; the limit is capped at [`Page::MAX`] and is
    /// at least 1.
    #[must_use]
    pub fn new(limit: u32, offset: u32) -> Self {
        Self {
            limit: limit.clamp(1, Self::MAX),
            offset,
        }
    }

    /// Rows per page.
    #[must_use]
    pub fn limit(self) -> u32 {
        self.limit
    }

    /// Rows skipped.
    #[must_use]
    pub fn offset(self) -> u32 {
        self.offset
    }
}

/// An inbound event with what each listener did with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventWithOutcomes {
    /// The event.
    pub event: InboundEvent,
    /// Its outcomes, in recording order.
    pub outcomes: Vec<OutcomeRecord>,
}

/// One `event_outcomes` row as a backend read it, for [`attach_outcomes`].
pub(crate) struct OutcomeRow {
    pub(crate) event_id: String,
    pub(crate) listener: String,
    pub(crate) outcome: String,
    pub(crate) detail: String,
    pub(crate) run_id: Option<String>,
    pub(crate) at: String,
}

/// Puts each outcome row under its event, keeping the rows' order.
pub(crate) fn attach_outcomes(
    events: Vec<InboundEvent>,
    rows: &[OutcomeRow],
) -> Vec<EventWithOutcomes> {
    events
        .into_iter()
        .map(|event| {
            let outcomes = rows
                .iter()
                .filter(|row| row.event_id == event.id.as_str())
                .map(|row| OutcomeRecord {
                    event_id: event.id.clone(),
                    listener: row.listener.clone(),
                    outcome: row.outcome.clone(),
                    detail: row.detail.clone(),
                    run_id: row.run_id.clone(),
                    at: row.at.clone(),
                })
                .collect();
            EventWithOutcomes { event, outcomes }
        })
        .collect()
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

/// The stored name of a status.
pub(crate) fn status_str(status: RunStatus) -> &'static str {
    status.as_str()
}

pub(crate) fn kind_str(kind: RunKind) -> &'static str {
    match kind {
        RunKind::Review => "review",
        RunKind::Plan => "plan",
        RunKind::DiscordTurn => "discord_turn",
        RunKind::MailReply => "mail_reply",
        RunKind::Address => "address",
    }
}

pub(crate) fn kind_parse(value: &str) -> Option<RunKind> {
    Some(match value {
        "review" => RunKind::Review,
        "plan" => RunKind::Plan,
        "discord_turn" => RunKind::DiscordTurn,
        "mail_reply" => RunKind::MailReply,
        "address" => RunKind::Address,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn call(session: &str, tool: &str, outcome: &str, ms: u64) -> ToolCallRecord {
        ToolCallRecord {
            at: String::new(),
            session: session.into(),
            model: "m".into(),
            turn: 1,
            tool: tool.into(),
            origin: "henk".into(),
            outcome: outcome.into(),
            arguments: "{}".into(),
            arguments_len: 2,
            result_chars: 0,
            elapsed_ms: ms,
        }
    }

    #[test]
    fn tool_usage_adds_up_per_session_and_tool() {
        let calls = [
            call("lane-a", "read_file", "ok", 10),
            call("lane-a", "read_file", "error", 5),
            call("lane-a", "read_file", "refused_repeat", 0),
            call("lane-a", "bash", "cancelled", 30),
            call("check-lane-a-1", "read_file", "refused_scope", 0),
        ];
        let shown: Vec<(String, String, u64, u64, u64, u64, u64)> = ToolUsage::from_calls(&calls)
            .into_iter()
            .map(|u| {
                (
                    u.session,
                    u.tool,
                    u.tally.calls,
                    u.tally.errors,
                    u.tally.refusals,
                    u.tally.other,
                    u.tally.total_ms,
                )
            })
            .collect();
        let row = |session: &str, tool: &str, c, e, r, o, ms| {
            (session.to_owned(), tool.to_owned(), c, e, r, o, ms)
        };
        assert_eq!(
            shown,
            [
                row("check-lane-a-1", "read_file", 1, 0, 1, 0, 0),
                row("lane-a", "bash", 1, 0, 0, 1, 30),
                row("lane-a", "read_file", 3, 1, 1, 0, 15),
            ]
        );
    }

    #[test]
    fn a_session_name_says_its_kind() {
        assert_eq!(session_kind("check-lane-b-3"), "check");
        assert_eq!(session_kind("planner"), "planner");
        assert_eq!(session_kind("address"), "address");
        assert_eq!(session_kind("lane-a"), "lane");
        assert_eq!(session_kind("security"), "lane");
    }
}
