//! The records and statuses every backend stores and returns.

use std::collections::BTreeMap;
use std::fmt;

use henk_domain::allowlist::Platform;
use henk_domain::role::Role;
use henk_domain::run::{EventId, RunId, RunKind};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Why a store operation failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The database said no (the SQLite store, or the sqlx layer on either).
    #[error("database error: {0}")]
    Sqlite(String),
    /// The schema could not be brought up to date.
    #[error("migration error: {0}")]
    Migration(String),
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

impl From<sqlx::Error> for StoreError {
    fn from(error: sqlx::Error) -> Self {
        Self::Sqlite(cause_chain(&error))
    }
}

impl From<sqlx::migrate::MigrateError> for StoreError {
    fn from(error: sqlx::migrate::MigrateError) -> Self {
        Self::Migration(cause_chain(&error))
    }
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
    /// Ended because a person cancelled it from the dashboard (#69).
    Cancelled,
    /// A review ended because a review of a newer commit replaced it
    /// (#231). Not a failure: the check is neutral and nothing is posted.
    Superseded,
}

impl RunStatus {
    /// The stored text: `running`, `finished`, `failed`, `cancelled` or
    /// `superseded`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Finished => "finished",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Superseded => "superseded",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "running" => Self::Running,
            "finished" => Self::Finished,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "superseded" => Self::Superseded,
            _ => return None,
        })
    }
}

/// Where a session stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    /// Still going.
    Running,
    /// Ran to its end: the model was done, or used its turns.
    Finished,
    /// Stopped at its time limit (#231). What it drafted until then counts.
    TimedOut,
    /// Did not finish: a model error, a refusal, a stuck loop or a cancel;
    /// the session's error says which.
    DidNotFinish,
}

impl SessionStatus {
    /// The stored text: `running`, `finished`, `timed_out` or
    /// `did_not_finish`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Finished => "finished",
            Self::TimedOut => "timed_out",
            Self::DidNotFinish => "did_not_finish",
        }
    }

    /// Reads the stored text; `dropped`, what review lanes that did not
    /// finish were stored as before #231, still reads.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "running" => Self::Running,
            "finished" => Self::Finished,
            "timed_out" => Self::TimedOut,
            "did_not_finish" | "dropped" => Self::DidNotFinish,
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
    /// The session wanted to post but the line was taken or the text refused.
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
    /// The stored text: `posted`, `improved`, `refused`, and so on.
    #[must_use]
    pub fn as_str(self) -> &'static str {
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

/// A reviewer's draft as it was queued (#189), with its decision once it
/// was settled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftRecord {
    /// RFC 3339; empty when recording means now.
    pub at: String,
    /// Its number in the review: `d3`.
    pub draft: String,
    /// The session that wrote it.
    pub session: String,
    /// The session's model.
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
    /// No fact-check is configured; written as the session wrote it.
    NotChecked,
    /// The review ended before it was settled; nothing written.
    Cancelled,
    /// The write to the platform failed.
    Failed,
    /// A review loop's executor fixed it; `comment_id` holds the commit (#285).
    Fixed,
    /// A review loop's executor will not fix it here (#285).
    WontFix,
    /// A review loop ended before its verdict held (#285), or a planning
    /// pairing before the planner answered the point (#328).
    Unsettled,
    /// A planning pairing's planner took the reviewer's point (#328).
    Taken,
}

/// The `kind` of a review loop's findings in the drafts table (#285). The
/// overview's drafts per day leaves them out.
pub const LOOP_FINDING_KIND: &str = "loop_finding";

/// The `kind` of a planning pairing's points in the drafts table (#328).
/// The overview's drafts per day leaves them out as well.
pub const PLAN_POINT_KIND: &str = "plan_point";

/// The `kind` of an execute run's requirements review points in the drafts
/// table (#535). The overview's drafts per day leaves them out as well.
pub const REQUIREMENT_POINT_KIND: &str = "requirement_point";

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
            Self::Fixed => "fixed",
            Self::WontFix => "wont_fix",
            Self::Unsettled => "unsettled",
            Self::Taken => "taken",
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
            Self::Fixed,
            Self::WontFix,
            Self::Unsettled,
            Self::Taken,
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
    /// The session: the session row's name.
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

/// One round of a session in a pairing (#374): the role resumed with
/// `handover`, and how many turns it took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoundRecord {
    /// The session.
    pub session: String,
    /// The role it runs in.
    pub role: SessionRole,
    /// This session's round, from 1.
    pub round: u32,
    /// RFC 3339; empty when recording means now.
    pub started_at: String,
    /// RFC 3339, once the round ended.
    pub ended_at: Option<String>,
    /// Its first model turn in the session's transcript.
    pub first_turn: u32,
    /// Model turns this round took; 0 until it ends.
    pub turns: u32,
    /// The message the role was resumed with, perhaps cut.
    pub handover: String,
    /// That message's length in bytes before any cut.
    pub handover_len: u64,
}

/// A stored round: (session, role, round, started at, ended at, first
/// turn, turns, handover, handover length).
pub(crate) type RoundRow = (
    String,
    String,
    i64,
    String,
    Option<String>,
    i64,
    i64,
    String,
    i64,
);

/// A stored round, checked.
pub(crate) fn round_record(row: RoundRow) -> Result<RoundRecord, StoreError> {
    let (session, role, round, started_at, ended_at, first_turn, turns, handover, handover_len) =
        row;
    Ok(RoundRecord {
        session,
        role: session_role(role)?,
        round: to_u32("session_rounds.round", round)?,
        started_at,
        ended_at,
        first_turn: to_u32("session_rounds.first_turn", first_turn)?,
        turns: to_u32("session_rounds.turns", turns)?,
        handover,
        handover_len: to_u64("session_rounds.handover_len", handover_len)?,
    })
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
    /// The session: the session row's name (`lane-a`, `check-2`,
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

/// The role a session ran in. New runs know only `planner`, `reviewer`
/// and `executor` ([`Role`]); `lane`, `check` and `address` are history
/// roles that sessions of older runs keep: a review lane, a fact-check, and
/// an address run (#324).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SessionRole {
    /// Drafts a plan.
    Planner,
    /// Reviews a pull request or a plan.
    Reviewer,
    /// Changes the code: fixes what the reviewer finds, or takes an issue
    /// to a pull request. Named `fixer` before #323.
    Executor,
    /// A review lane, before the review loop (#283).
    Lane,
    /// A fact-check of the lanes' drafts, before #309.
    Check,
    /// An address run's session, before #327.
    Address,
}

impl SessionRole {
    /// Every role, new and history.
    pub const ALL: [Self; 6] = [
        Self::Planner,
        Self::Reviewer,
        Self::Executor,
        Self::Lane,
        Self::Check,
        Self::Address,
    ];

    /// The stored text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planner => "planner",
            Self::Reviewer => "reviewer",
            Self::Executor => "executor",
            Self::Lane => "lane",
            Self::Check => "check",
            Self::Address => "address",
        }
    }

    /// Reads the stored text.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|role| role.as_str() == value)
    }
}

impl From<Role> for SessionRole {
    fn from(role: Role) -> Self {
        match role {
            Role::Planner => Self::Planner,
            Role::Reviewer => Self::Reviewer,
            Role::Executor => Self::Executor,
        }
    }
}

impl fmt::Display for SessionRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
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

/// The one tool unknown tools count under: each name a model invents is
/// a tool of its own otherwise (#341). No real tool name has parentheses.
pub const UNKNOWN_TOOL: &str = "(unknown)";

/// The tool a call counts under: [`UNKNOWN_TOOL`] for one that does not
/// exist, which is the only call with no origin, however it ended (a
/// repeated one is refused before it is looked up); its own name otherwise.
#[must_use]
pub fn counted_as<'a>(tool: &'a str, origin: &str) -> &'a str {
    if origin.is_empty() {
        UNKNOWN_TOOL
    } else {
        tool
    }
}

/// One row of tool usage: one tool of one session, or across runs one tool
/// of one kind of session on one model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolUsage {
    /// A session name for one run; a role ([`SessionRole`]) across runs.
    pub session: String,
    /// The model; empty for one run, where the session says it.
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
                .entry((
                    call.session.clone(),
                    counted_as(&call.tool, &call.origin).to_owned(),
                ))
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

    /// Rows grouped by (model, role, tool, outcome) with a count and a
    /// sum of milliseconds, folded per model, role and tool.
    pub(crate) fn across_runs(rows: Vec<(String, String, String, String, u64, u64)>) -> Vec<Self> {
        let mut usage: BTreeMap<(String, String, String), ToolTally> = BTreeMap::new();
        for (model, role, tool, outcome, n, ms) in rows {
            usage
                .entry((model, role, tool))
                .or_default()
                .add(&outcome, n, ms);
        }
        usage
            .into_iter()
            .map(|((model, role, tool), tally)| Self {
                session: role,
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
    /// The run that replaced it, when it was superseded (#231).
    pub superseded_by: Option<RunId>,
    /// Why a review loop stopped (#286): `converged`, `max_rounds`,
    /// `repeating_finding`, `timeout` and the like. Runs from before #396
    /// may read `build_failing`, when Henk still ran checks of his own.
    pub loop_stop: Option<String>,
    /// How many rounds a review loop ran (#286).
    pub loop_rounds: Option<u32>,
}

/// What a session's model calls took and gave, as the run record keeps it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    /// The whole prompt of every call, cache reads and writes included.
    pub input: u64,
    /// The answers.
    pub output: u64,
    /// Of `input`, read from the prompt cache.
    pub cache_read: u64,
    /// Of `input`, written into the prompt cache.
    pub cache_write: u64,
    /// Of `output`, spent reasoning before answering.
    pub reasoning: u64,
}

impl TokenUsage {
    /// `input` tokens in and `output` out, none of them through a cache.
    #[must_use]
    pub const fn new(input: u64, output: u64) -> Self {
        Self {
            input,
            output,
            cache_read: 0,
            cache_write: 0,
            reasoning: 0,
        }
    }
}

/// A stored session: one conversation of one role in a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    /// Its name, unique in the run: `reviewer`, `executor`, `planner`, or
    /// in older runs a lane's.
    pub name: String,
    /// The role it ran in.
    pub role: SessionRole,
    /// Model name.
    pub model: String,
    /// Where it stands.
    pub status: SessionStatus,
    /// Model calls made.
    pub turns: u64,
    /// Tokens in: the whole prompt of every model call, cache reads and
    /// writes included.
    pub input_tokens: u64,
    /// Tokens out.
    pub output_tokens: u64,
    /// Of `input_tokens`, read from the model's prompt cache (#330).
    pub cache_read_tokens: u64,
    /// Of `input_tokens`, written into the prompt cache (#330).
    pub cache_write_tokens: u64,
    /// Of `output_tokens`, spent reasoning before answering (#365).
    pub reasoning_tokens: u64,
    /// The error, when it did not finish.
    pub error: Option<String>,
    /// RFC 3339: when it started.
    pub started_at: String,
    /// RFC 3339: when it ended, once it has.
    pub finished_at: Option<String>,
}

/// One stage of a run (#226): the steps a review or a plan goes
/// through, in the order they come.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Stage {
    /// The request arrived.
    Requested,
    /// It waited for a review slot.
    Queued,
    /// The run started, and a review its check.
    Started,
    /// A ticket run's plan stage: the plan run it started, and how it ended
    /// (#536).
    Plan,
    /// A ticket run's execute stage: the execute run and the pull request it
    /// opened (#536).
    Execute,
    /// A ticket run's review stage: the review run of that pull request and
    /// how it ended (#536).
    Review,
    /// The diff was read.
    Diff,
    /// The review's workspaces were checked out.
    Checkout,
    /// The planner's workspace was set up (and an address run's, before
    /// #327).
    Workspace,
    /// The review lanes ran; runs before the review loop (#283) only.
    Lanes,
    /// The reviewer and the executor took their rounds (#324); written as
    /// `lanes` before.
    Rounds,
    /// The planner's session ran (and an address run's, before #327).
    Session,
    /// The drafts were checked; runs before #309 only.
    FactCheck,
    /// An address run's commit was made; runs before #327 only.
    Commit,
    /// A rebase run replayed the pull request's commits onto its base,
    /// with the executor resolving any conflict (#579).
    Rebase,
    /// The executor's commit was pushed, or an address run's before #327.
    Push,
    /// What came of it was written on the platform.
    Publish,
    /// An address run replied to the threads; runs before #327 only.
    Replies,
    /// The run ended.
    Done,
}

impl Stage {
    /// The stored text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Queued => "queued",
            Self::Started => "started",
            Self::Plan => "plan",
            Self::Execute => "execute",
            Self::Review => "review",
            Self::Diff => "diff",
            Self::Checkout => "checkout",
            Self::Workspace => "workspace",
            Self::Lanes => "lanes",
            Self::Rounds => "rounds",
            Self::Session => "session",
            Self::FactCheck => "fact_check",
            Self::Commit => "commit",
            Self::Push => "push",
            Self::Rebase => "rebase",
            Self::Publish => "publish",
            Self::Replies => "replies",
            Self::Done => "done",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "requested" => Self::Requested,
            "queued" => Self::Queued,
            "started" => Self::Started,
            "plan" => Self::Plan,
            "execute" => Self::Execute,
            "review" => Self::Review,
            "diff" => Self::Diff,
            "checkout" => Self::Checkout,
            "workspace" => Self::Workspace,
            "lanes" => Self::Lanes,
            "rounds" => Self::Rounds,
            "session" => Self::Session,
            "fact_check" => Self::FactCheck,
            "commit" => Self::Commit,
            "push" => Self::Push,
            "rebase" => Self::Rebase,
            "publish" => Self::Publish,
            "replies" => Self::Replies,
            "done" => Self::Done,
            _ => return None,
        })
    }
}

/// Where a stage stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageState {
    /// Going now.
    Running,
    /// Done.
    Done,
    /// Broke off; the detail says why.
    Failed,
    /// Not needed, or not reached because the run ended first.
    Skipped,
}

impl StageState {
    /// The stored text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "running" => Self::Running,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "skipped" => Self::Skipped,
            _ => return None,
        })
    }

    /// Whether the stage is over.
    #[must_use]
    pub fn ended(self) -> bool {
        !matches!(self, Self::Running)
    }
}

/// A stage as a run moves through it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageWrite {
    /// Which stage.
    pub stage: Stage,
    /// Where it stands now.
    pub state: StageState,
    /// One line in Henk's own words, never other people's text.
    pub detail: String,
    /// When it started, when not now (the time a review was submitted).
    /// Kept from the first write of the stage.
    pub started_at: Option<time::OffsetDateTime>,
    /// When it ended, when it has and not now.
    pub ended_at: Option<time::OffsetDateTime>,
}

impl StageWrite {
    /// The stage as it is now, timed now.
    #[must_use]
    pub fn now(which: Stage, state: StageState, detail: impl Into<String>) -> Self {
        Self {
            stage: which,
            state,
            detail: detail.into(),
            started_at: None,
            ended_at: None,
        }
    }
}

/// A stored stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageRecord {
    /// Which stage.
    pub stage: Stage,
    /// Where it stands.
    pub state: StageState,
    /// RFC 3339.
    pub started_at: String,
    /// RFC 3339, once it ended.
    pub ended_at: Option<String>,
    /// One line in Henk's own words.
    pub detail: String,
}

/// What happened on one UTC day (#225): runs started, of them finished and
/// failed, findings posted (new comments, checked or not) and drafts
/// written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DayCounts {
    /// `YYYY-MM-DD`, UTC.
    pub day: String,
    /// Runs started that day.
    pub runs: u64,
    /// Of those, the ones that finished.
    pub finished: u64,
    /// Of those, the ones that failed. Cancelled, superseded and running
    /// runs are in neither count.
    pub failed: u64,
    /// Findings posted that day: `posted` and `unverified` comments.
    pub findings_posted: u64,
    /// Drafts written that day.
    pub drafts: u64,
}

/// Merges per-day rows from the three counting queries into
/// [`DayCounts`], oldest day first. Days with nothing are left out.
pub(crate) fn merge_days(
    runs: Vec<(String, i64, i64, i64)>,
    findings: Vec<(String, i64)>,
    drafts: Vec<(String, i64)>,
) -> Vec<DayCounts> {
    let n = |v: i64| u64::try_from(v).unwrap_or_default();
    let mut days: std::collections::BTreeMap<String, DayCounts> = std::collections::BTreeMap::new();
    let at = |day: String| DayCounts {
        day,
        ..DayCounts::default()
    };
    for (day, all, finished, failed) in runs {
        let entry = days.entry(day.clone()).or_insert_with(|| at(day));
        entry.runs += n(all);
        entry.finished += n(finished);
        entry.failed += n(failed);
    }
    for (day, posted) in findings {
        days.entry(day.clone())
            .or_insert_with(|| at(day))
            .findings_posted += n(posted);
    }
    for (day, written) in drafts {
        days.entry(day.clone()).or_insert_with(|| at(day)).drafts += n(written);
    }
    days.into_values().collect()
}

/// How one session of a review ended (#229).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEnding {
    /// The review.
    pub run_id: RunId,
    /// When the review started, RFC 3339.
    pub started_at: String,
    /// The session's name: `reviewer`, or in older reviews `lane-a`,
    /// `check-1`.
    pub name: String,
    /// The role it ran in.
    pub role: SessionRole,
    /// The model it ran.
    pub model: String,
    /// How it ended.
    pub status: SessionStatus,
    /// Why it did not finish, when it did not.
    pub error: Option<String>,
}

/// The most reviews [`crate::RunStore::session_endings`] reads at once.
pub const MOST_SESSION_REVIEWS: u32 = 1000;

/// A stored session ending: (run, started at, name, role, model, status,
/// error).
pub(crate) type SessionEndingRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
);

/// Stored session endings, checked.
pub(crate) fn session_endings(
    rows: Vec<SessionEndingRow>,
) -> Result<Vec<SessionEnding>, StoreError> {
    rows.into_iter()
        .map(|(run, started_at, name, role, model, status, error)| {
            let run_id = run_id("sessions.run_id", run)?;
            let role = session_role(role)?;
            let status = session_status(status)?;
            Ok(SessionEnding {
                run_id,
                started_at,
                name,
                role,
                model,
                status,
                error,
            })
        })
        .collect()
}

/// Stored (run, session, role, status) rows, checked.
pub(crate) fn session_dots(
    rows: Vec<(String, String, String, String)>,
) -> Result<Vec<(RunId, String, SessionRole, SessionStatus)>, StoreError> {
    rows.into_iter()
        .map(|(run, name, role, status)| {
            let run = run_id("sessions.run_id", run)?;
            Ok((run, name, session_role(role)?, session_status(status)?))
        })
        .collect()
}

/// A stored session role, checked.
pub(crate) fn session_role(role: String) -> Result<SessionRole, StoreError> {
    SessionRole::parse(&role).ok_or(StoreError::Corrupt {
        column: "sessions.role",
        value: role,
    })
}

/// A stored session status, checked.
pub(crate) fn session_status(status: String) -> Result<SessionStatus, StoreError> {
    SessionStatus::parse(&status).ok_or(StoreError::Corrupt {
        column: "sessions.status",
        value: status,
    })
}

/// A stored run id, checked.
pub(crate) fn run_id(column: &'static str, id: String) -> Result<RunId, StoreError> {
    RunId::parse(id.clone()).map_err(|_| StoreError::Corrupt { column, value: id })
}

/// A stored inbound event id, checked.
pub(crate) fn event_id(id: String) -> Result<EventId, StoreError> {
    EventId::parse(id.clone()).map_err(|_| StoreError::Corrupt {
        column: "inbound_events.id",
        value: id,
    })
}

/// A stored run kind, checked.
pub(crate) fn run_kind(kind: String) -> Result<RunKind, StoreError> {
    kind_parse(&kind).ok_or(StoreError::Corrupt {
        column: "runs.kind",
        value: kind,
    })
}

/// A stored run platform, checked.
pub(crate) fn run_platform(platform: String) -> Result<Platform, StoreError> {
    platform_parse(&platform).ok_or(StoreError::Corrupt {
        column: "runs.platform",
        value: platform,
    })
}

/// Stored (run, stage, state) rows, checked, each run's in stage order.
pub(crate) fn stage_dots(
    rows: Vec<(String, String, String)>,
) -> Result<Vec<(RunId, Stage, StageState)>, StoreError> {
    let mut dots = rows
        .into_iter()
        .map(|(run, which, where_at)| {
            let run = run_id("stages.run_id", run)?;
            let which = Stage::parse(&which).ok_or(StoreError::Corrupt {
                column: "stages.stage",
                value: which,
            })?;
            let where_at = StageState::parse(&where_at).ok_or(StoreError::Corrupt {
                column: "stages.state",
                value: where_at,
            })?;
            Ok((run, which, where_at))
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    dots.sort_by(|a, b| (a.0.as_str(), a.1).cmp(&(b.0.as_str(), b.1)));
    Ok(dots)
}

/// Stored stage rows as records, in the order the stages come.
pub(crate) fn stage_records(
    rows: Vec<(String, String, String, Option<String>, String)>,
) -> Result<Vec<StageRecord>, StoreError> {
    let mut records = rows
        .into_iter()
        .map(|(stage, state, started_at, ended_at, detail)| {
            Ok(StageRecord {
                stage: Stage::parse(&stage).ok_or(StoreError::Corrupt {
                    column: "stages.stage",
                    value: stage,
                })?,
                state: StageState::parse(&state).ok_or(StoreError::Corrupt {
                    column: "stages.state",
                    value: state,
                })?,
                started_at,
                ended_at,
                detail,
            })
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    records.sort_by_key(|record| record.stage);
    Ok(records)
}

/// Stored `run_children` rows as the stage and the run each is, in the
/// order the stages come.
pub(crate) fn child_records(
    rows: Vec<(String, String)>,
) -> Result<Vec<(Stage, RunId)>, StoreError> {
    let mut children = rows
        .into_iter()
        .map(|(stage, child)| {
            Ok((
                Stage::parse(&stage).ok_or(StoreError::Corrupt {
                    column: "run_children.stage",
                    value: stage,
                })?,
                RunId::parse(&child).map_err(|_| StoreError::Corrupt {
                    column: "run_children.child",
                    value: child,
                })?,
            ))
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    children.sort_by_key(|(stage, _)| *stage);
    Ok(children)
}

/// One recorded finding action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindingRecord {
    /// RFC 3339.
    pub at: String,
    /// The session.
    pub session: String,
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

/// The sources and kinds of the inbound events Henk has recorded, for the
/// events page's filters (#227).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventFacets {
    /// `github_webhook`, `api`, `dashboard`, and so on, sorted.
    pub sources: Vec<String>,
    /// `pull_request`, `review_requested`, and so on, sorted.
    pub kinds: Vec<String>,
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

/// Which tool calls a listing or a tally covers (#203). `None` matches
/// anything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolCallFilter {
    /// How they ended; listings only.
    pub outcome: Option<OutcomeFilter>,
    /// The tool, as the model named it.
    pub tool: Option<String>,
    /// The session's model.
    pub model: Option<String>,
    /// The role of the session that made them.
    pub role: Option<SessionRole>,
    /// Made at or after this RFC 3339 time.
    pub since: Option<String>,
    /// Made before this RFC 3339 time.
    pub until: Option<String>,
    /// Only calls listed after this one: for keyset paging, newest first.
    pub before: Option<ToolCallKey>,
}

/// An outcome to list tool calls by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutcomeFilter {
    /// Ended this way: `ok`, `error`, `refused_scope`, and so on.
    Is(String),
    /// Ended any way but `ok`.
    Problems,
}

impl OutcomeFilter {
    /// The text a query parameter takes.
    pub(crate) fn param(filter: Option<&Self>) -> Option<&str> {
        filter.map(|f| match f {
            Self::Is(outcome) => outcome.as_str(),
            Self::Problems => "problems",
        })
    }
}

/// Where a tool call sits in a newest-first listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallKey {
    /// When it was made, as the store returned it.
    pub at: String,
    /// Its row id, which breaks a tie on the time.
    pub id: i64,
}

/// A tool call in a listing across runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallListing {
    /// Its row id, for the keyset.
    pub id: i64,
    /// The run.
    pub run_id: String,
    /// The run's repository.
    pub repo: String,
    /// The run's pull request, merge request or issue.
    pub target: u64,
    /// The run's platform.
    pub platform: Platform,
    /// The run's kind: a plan run is about an issue, the others about a
    /// pull or merge request.
    pub kind: RunKind,
    /// The call.
    pub call: ToolCallRecord,
    /// Whether the run still keeps the conversation of the call's session.
    /// Transcripts are stored per session and pruned on their own, so a
    /// call can outlive its conversation.
    pub transcript_kept: bool,
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

pub(crate) fn kind_str(kind: RunKind) -> &'static str {
    match kind {
        RunKind::Review => "review",
        RunKind::Plan => "plan",
        RunKind::Execute => "execute",
        RunKind::Rebase => "rebase",
        RunKind::Ticket => "ticket",
        RunKind::DiscordTurn => "discord_turn",
        RunKind::MailReply => "mail_reply",
        RunKind::Address => "address",
    }
}

fn kind_parse(value: &str) -> Option<RunKind> {
    Some(match value {
        "review" => RunKind::Review,
        "plan" => RunKind::Plan,
        "execute" => RunKind::Execute,
        "rebase" => RunKind::Rebase,
        "ticket" => RunKind::Ticket,
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

fn platform_parse(value: &str) -> Option<Platform> {
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

/// A signed column read back as a `u32`; a value out of range is corrupt.
pub(crate) fn to_u32(column: &'static str, value: i64) -> Result<u32, StoreError> {
    u32::try_from(value).map_err(|_| StoreError::Corrupt {
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
    pub(crate) superseded_by: Option<String>,
    pub(crate) loop_stop: Option<String>,
    pub(crate) loop_rounds: Option<i64>,
}

impl RawRun {
    /// Checks every value and builds the record.
    pub(crate) fn into_record(self) -> Result<RunRecord, StoreError> {
        let id = run_id("runs.id", self.id)?;
        let kind = run_kind(self.kind)?;
        let platform = run_platform(self.platform)?;
        let status = RunStatus::parse(&self.status).ok_or(StoreError::Corrupt {
            column: "runs.status",
            value: self.status,
        })?;
        let superseded_by = self
            .superseded_by
            .map(|by| run_id("runs.superseded_by", by))
            .transpose()?;
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
            superseded_by,
            loop_stop: self.loop_stop,
            loop_rounds: self
                .loop_rounds
                .map(|n| to_u32("runs.loop_rounds", n))
                .transpose()?,
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
            ToolCallRecord {
                origin: String::new(),
                ..call("lane-a", "nope", "unknown_tool", 0)
            },
            ToolCallRecord {
                origin: String::new(),
                ..call("lane-a", "Review complete. End. [cut]", "refused_repeat", 0)
            },
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
                row("lane-a", "(unknown)", 2, 0, 1, 1, 0),
                row("lane-a", "bash", 1, 0, 0, 1, 30),
                row("lane-a", "read_file", 3, 1, 1, 0, 15),
            ]
        );
    }

    #[test]
    fn a_session_role_reads_back_as_stored() {
        for role in SessionRole::ALL {
            assert_eq!(SessionRole::parse(role.as_str()), Some(role));
        }
        assert_eq!(SessionRole::parse("fixer"), None, "the migration maps it");
        assert_eq!(
            SessionRole::from(Role::Executor),
            SessionRole::Executor,
            "a new run writes only the three roles"
        );
    }
}
