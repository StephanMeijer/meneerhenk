//! What the API returns and takes (#198). These are the API's own types,
//! not the store's records, so the store can change without changing the
//! API. Every text is returned as it was stored, never escaped: it is
//! other people's text (§8.3), and whoever shows it renders it as text.
//!
//! The SPA's TypeScript types are generated from these in a test
//! (`api_types_are_current`), so a change here shows up there.

use henk_domain::allowlist::Platform;
use henk_domain::run::RunKind;
use henk_store::{
    DraftRecord, EventRecord, EventWithOutcomes, FindingRecord, InboundEvent, LaneRecord,
    OutcomeRecord, RunRecord, StageRecord, ToolCallRecord, ToolUsage, TranscriptSummary,
};
use serde::{Deserialize, Serialize};

use crate::config::Settings;
use crate::runs::{MessageView, PartView, TranscriptView};

/// Who is signed in, and the token their actions carry.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Me {
    /// The GitHub user id: what decides access (§2).
    pub github_id: u64,
    /// The GitHub login, for display only.
    pub login: String,
    /// The CSRF token every action sends in `X-CSRF-Token`.
    pub csrf: String,
    /// What `POST /runs` can start here: `review`, `plan`, and `address`
    /// when address runs are configured.
    pub startable: Vec<String>,
}

/// One page of a listing, newest first.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Page<T> {
    /// The rows.
    pub items: Vec<T>,
    /// The cursor of the next page; none when this was the last.
    pub next: Option<String>,
}

/// A run, as a listing shows it.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct RunSummary {
    /// Its id.
    pub id: String,
    /// `review`, `plan`, `address`, `discord_turn` or `mail_reply`.
    pub kind: String,
    /// `github` or `gitlab`.
    pub platform: String,
    /// `owner/name`.
    pub repo: String,
    /// The pull request, merge request or issue number.
    pub target: u64,
    /// A link to that pull request, merge request or issue.
    pub target_url: Option<String>,
    /// The reviewed commit, for reviews.
    pub commit: Option<String>,
    /// `running`, `finished`, `failed`, `cancelled` or `superseded` (a
    /// review replaced by a review of a newer commit, #231).
    pub status: String,
    /// What started it.
    pub trigger: String,
    /// Who asked, as a stable id (§2).
    pub requester: Option<String>,
    /// RFC 3339.
    pub started_at: String,
    /// RFC 3339, once ended.
    pub finished_at: Option<String>,
    /// The run that replaced it, when it was superseded.
    pub superseded_by: Option<String>,
    /// Where each of its lanes stands, for lists (#225). Empty where the
    /// whole run is sent with its lanes, and on a `run` stream message.
    pub lanes: Vec<LaneDot>,
    /// Where each of its stages stands, for lists (#225). Empty as `lanes`.
    pub stages: Vec<StageDot>,
}

/// A lane as a list shows it: its name and status.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct LaneDot {
    /// `lane-a`, `check-1`, `planner`.
    pub name: String,
    /// `running`, `finished`, `timed_out` or `did_not_finish`.
    pub status: String,
}

/// A stage as a list shows it: its name and state.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct StageDot {
    /// `diff`, `lanes`, `fact_check`, and so on.
    pub name: String,
    /// `running`, `done`, `failed` or `skipped`.
    pub state: String,
}

/// The lanes and stages of one run moved on: a message of `/runs/stream`.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Progress {
    /// Which run.
    pub run_id: String,
    /// Every lane, when lanes moved; `null` when only stages did.
    pub lanes: Option<Vec<LaneDot>>,
    /// Every stage, when stages moved; `null` when only lanes did.
    pub stages: Option<Vec<StageDot>>,
}

/// The review slots (#225).
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Slots {
    /// `review.max_concurrent`.
    pub limit: u64,
    /// Slots a review holds now.
    pub in_use: u64,
    /// Reviews waiting for a slot, longest waiting first. They have no run
    /// yet.
    pub waiting: Vec<WaitingReview>,
}

/// A review waiting for a slot.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct WaitingReview {
    /// `owner/name`.
    pub repo: String,
    /// The pull or merge request.
    pub target: u64,
    /// RFC 3339: since when it waits.
    pub since: String,
}

/// What happened per UTC day (#225): `GET /stats/overview`.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct OverviewStats {
    /// The first day, `YYYY-MM-DD`.
    pub from: String,
    /// The last day, today.
    pub to: String,
    /// Every day from `from` to `to`, oldest first; a quiet day is zeros.
    pub days: Vec<DayStats>,
}

/// One UTC day.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct DayStats {
    /// `YYYY-MM-DD`.
    pub day: String,
    /// Runs started.
    pub runs: u64,
    /// Of those, finished.
    pub finished: u64,
    /// Of those, failed: what did not complete. Cancelled, superseded and
    /// running runs are in neither.
    pub failed: u64,
    /// Findings posted: new comments, checked or not.
    pub findings_posted: u64,
    /// Drafts the lanes wrote.
    pub drafts: u64,
}

/// How many runs a filter matches, over every page.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct RunCount {
    /// The number.
    pub count: u64,
}

/// One run with everything the run record holds about it.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct RunDetail {
    /// The run.
    pub run: RunSummary,
    /// The summary text, once there is one.
    pub summary: Option<String>,
    /// The error, when it failed.
    pub error: Option<String>,
    /// The platform's id for the review's check.
    pub check_id: Option<String>,
    /// RFC 3339: when its process last said it was alive.
    pub heartbeat_at: Option<String>,
    /// Its sessions: review lanes, checks, the planner, the address run.
    pub lanes: Vec<Lane>,
    /// What happened to findings on the platform.
    pub findings: Vec<Finding>,
    /// A review's drafts and what became of each (#189).
    pub drafts: Vec<Draft>,
    /// The sessions whose conversation is kept (#191).
    pub transcripts: Vec<TranscriptRef>,
    /// Tool calls per session and tool (#190).
    pub tool_usage: Vec<ToolUsageRow>,
    /// The run's timeline.
    pub events: Vec<RunEvent>,
    /// The requests that started or joined it.
    pub requests: Vec<EventSummary>,
    /// Its stages, in the order they come (#226). Empty for a run from
    /// before stages were kept.
    pub stages: Vec<Stage>,
}

/// One stage of a run: a review goes `requested`, `queued`, `started`,
/// `diff`, `checkout`, `lanes`, `fact_check`, `publish`, `done`; a plan or
/// an address run goes through `workspace`, `session` and, for an address
/// run, `commit`, `push` and `replies` instead.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Stage {
    /// Which stage.
    pub name: String,
    /// `running`, `done`, `failed` or `skipped`.
    pub state: String,
    /// RFC 3339.
    pub started_at: String,
    /// RFC 3339, once it ended.
    pub ended_at: Option<String>,
    /// One line in Henk's own words: `12 files, +340 -25; 1 not reviewed`.
    pub detail: String,
}

impl From<&StageRecord> for Stage {
    fn from(stage: &StageRecord) -> Self {
        Self {
            name: stage.stage.as_str().to_owned(),
            state: stage.state.as_str().to_owned(),
            started_at: stage.started_at.clone(),
            ended_at: stage.ended_at.clone(),
            detail: stage.detail.clone(),
        }
    }
}

/// When a run's process last said it was alive.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Heartbeat {
    /// RFC 3339.
    pub at: String,
}

/// A run's own fields after it changed: started, ended, got its check.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct RunUpdate {
    /// The run.
    pub run: RunSummary,
    /// The summary text, once there is one.
    pub summary: Option<String>,
    /// The error, when it failed.
    pub error: Option<String>,
    /// The platform's id for the review's check.
    pub check_id: Option<String>,
}

/// What runs now: the start of `/runs/stream`, and again now and then.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct RunningSnapshot {
    /// The newest running runs, at most 100.
    pub runs: Vec<RunSummary>,
    /// How many run, beyond those too.
    pub count: u64,
    /// The review slots and what waits for one.
    pub slots: Slots,
}

/// One message of `/runs/{id}/stream`: its SSE event name is `kind`, its
/// data is `data`. The TypeScript side reads them as this union.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum RunMessage {
    /// The whole run: the first message, and after a gap.
    Snapshot(Box<RunDetail>),
    /// The run started, ended or got its check.
    Run(RunUpdate),
    /// A lane started or ended: every lane of the run.
    Lanes(Vec<Lane>),
    /// A session called a tool.
    ToolCall(ToolCall),
    /// A lane queued a draft, or the check decided one.
    Draft(Draft),
    /// Something was done with a finding.
    Finding(Finding),
    /// A line on the run's timeline.
    Event(RunEvent),
    /// A session's conversation was kept.
    Transcript(TranscriptRef),
    /// A stage began or ended: every stage of the run (#226).
    Stages(Vec<Stage>),
    /// The run's process said it is alive.
    Heartbeat(Heartbeat),
    /// The run has ended; the stream closes.
    End,
}

/// One message of `/runs/stream`.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum RunningMessage {
    /// What runs now.
    Snapshot(RunningSnapshot),
    /// A run started or ended.
    Run(Box<RunSummary>),
    /// A running run's lanes or stages moved on.
    Progress(Progress),
    /// A review started waiting, took or freed a slot, or left the queue.
    Slots(Slots),
}

/// One group's rejection rate per UTC day (#228): `GET /quality/daily`.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct QualitySeries {
    /// The model, lane or repository; `owner/name #7` for a pull request.
    pub key: String,
    /// Every day of the period, oldest first.
    pub days: Vec<DayRate>,
}

/// What the check made of one group's drafts on one day.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct DayRate {
    /// `YYYY-MM-DD`, UTC.
    pub day: String,
    /// Drafts the check judged: confirmed, rejected and repeats.
    pub judged: u64,
    /// Of those, rejected.
    pub rejected: u64,
    /// Rejected of judged, from 0 to 1; `null` on a day it judged none.
    pub rate: Option<f64>,
}

/// How many drafts a filter matches, over every page.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct DraftCount {
    /// The number.
    pub count: u64,
}

/// What became of the drafts of one group: a model, a lane, a repository
/// or a pull request (#205).
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct QualityRow {
    /// The model, lane or repository; `owner/name #7` for a pull request.
    pub key: String,
    /// The repository, when grouped by pull request.
    pub repo: Option<String>,
    /// The number, when grouped by pull request.
    pub target: Option<u64>,
    /// A link to that pull request, merge request or issue.
    pub target_url: Option<String>,
    /// Every draft.
    pub drafts: u64,
    /// Confirmed and written.
    pub confirmed: u64,
    /// Rejected by the check.
    pub rejected: u64,
    /// Repeats, merged into another draft or finding.
    pub same_as: u64,
    /// No model could check them.
    pub unchecked: u64,
    /// No check was configured.
    pub not_checked: u64,
    /// The review ended first.
    pub cancelled: u64,
    /// The write failed.
    pub failed: u64,
    /// Not decided yet.
    pub waiting: u64,
    /// Drafts a checker decided: confirmed, rejected and repeats.
    pub judged: u64,
    /// Rejected of judged, from 0 to 1; none when nothing was judged.
    pub rejection_rate: Option<f64>,
}

/// A draft across runs, with the run it belongs to (#205).
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct DraftItem {
    /// The run.
    pub run_id: String,
    /// The run's repository.
    pub repo: String,
    /// The run's pull request, merge request or issue.
    pub target: u64,
    /// A link to it.
    pub target_url: Option<String>,
    /// The draft and what became of it.
    pub draft: Draft,
}

/// One tool of one model in one kind of session, across runs (#203).
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ToolSummaryRow {
    /// The tool, as the model named it.
    pub tool: String,
    /// The model.
    pub model: String,
    /// `lane`, `check`, `planner` or `address`.
    pub session_kind: String,
    /// Every call.
    pub calls: u64,
    /// Calls the tool reported as failed.
    pub errors: u64,
    /// Calls the scope guard or the repeat guard refused.
    pub refusals: u64,
    /// Calls that never ran for another reason.
    pub other: u64,
    /// Milliseconds the calls ran, together.
    pub total_ms: u64,
    /// Errors of calls, from 0 to 1.
    pub error_rate: f64,
    /// Refusals of calls, from 0 to 1.
    pub refusal_rate: f64,
}

/// A tool call across runs, with the run it belongs to (#203).
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ToolCallItem {
    /// The run.
    pub run_id: String,
    /// The run's repository.
    pub repo: String,
    /// The run's pull request, merge request or issue.
    pub target: u64,
    /// A link to it.
    pub target_url: Option<String>,
    /// The call.
    pub call: ToolCall,
    /// Whether the run still keeps the conversation of the call's session,
    /// so whether a link to its turn leads anywhere.
    pub transcript_kept: bool,
}

/// One session of a run.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Lane {
    /// `lane-a`, `check-1`, `planner`, `address`.
    pub name: String,
    /// The model.
    pub model: String,
    /// `running`, `finished`, `timed_out` (stopped at its time limit; what
    /// it drafted counts) or `did_not_finish` (see `error`), #231.
    pub status: String,
    /// Model calls made.
    pub turns: u64,
    /// Tokens in, not read from a cache.
    pub input_tokens: u64,
    /// Tokens out.
    pub output_tokens: u64,
    /// Why it did not finish.
    pub error: Option<String>,
    /// The turn of its latest tool call: how far a running lane has come,
    /// since turns and tokens are stored when it ends.
    pub last_call_turn: Option<u32>,
    /// RFC 3339: when it started.
    pub started_at: String,
    /// RFC 3339: when it ended, once it has.
    pub finished_at: Option<String>,
}

/// One thing done with a finding on the platform.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Finding {
    /// RFC 3339.
    pub at: String,
    /// The lane.
    pub lane: String,
    /// The file.
    pub path: String,
    /// The line.
    pub line: u32,
    /// The platform's comment id.
    pub comment_id: String,
    /// `posted`, `improved`, `refused`, `rejected`, `unverified`,
    /// `withdrawn` or `merged`.
    pub action: String,
}

/// A lane's draft and what became of it (#189).
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Draft {
    /// RFC 3339, when it was queued.
    pub at: String,
    /// Its number in the review: `d3`.
    pub id: String,
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
    /// The comment a rewrite or withdrawal is about; empty for a finding.
    pub target: String,
    /// The finding, the new text, or why to withdraw.
    pub body: String,
    /// What became of it; none while it waits.
    pub decision: Option<DraftDecision>,
}

/// What became of a draft.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct DraftDecision {
    /// RFC 3339.
    pub at: String,
    /// `confirmed`, `rejected`, `same_as`, `unchecked`, `not_checked`,
    /// `cancelled` or `failed`.
    pub verdict: String,
    /// The model that gave the verdict; empty when none did.
    pub checker: String,
    /// Why.
    pub reason: String,
    /// What it repeats, for a merge.
    pub same_as: String,
    /// The comment written for it, or merged into.
    pub comment_id: String,
}

/// A session whose conversation is kept.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct TranscriptRef {
    /// RFC 3339.
    pub at: String,
    /// The session.
    pub session: String,
    /// Its model.
    pub model: String,
    /// How it stopped.
    pub stop: String,
    /// Model calls made.
    pub turns: u32,
    /// The stored conversation's size.
    pub bytes: u64,
}

/// One tool of one session, added up.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ToolUsageRow {
    /// The session.
    pub session: String,
    /// The tool.
    pub tool: String,
    /// Every call.
    pub calls: u64,
    /// Calls the tool reported as failed.
    pub errors: u64,
    /// Calls the scope guard or the repeat guard refused.
    pub refusals: u64,
    /// Calls that never ran for another reason.
    pub other: u64,
    /// Milliseconds the calls ran, together.
    pub total_ms: u64,
}

/// One line of a run's timeline.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct RunEvent {
    /// RFC 3339.
    pub at: String,
    /// `info`, `warn` or `error`.
    pub level: String,
    /// What happened.
    pub message: String,
}

/// One tool call (#190).
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ToolCall {
    /// RFC 3339.
    pub at: String,
    /// The session.
    pub session: String,
    /// The session's model.
    pub model: String,
    /// The turn, from 1.
    pub turn: u32,
    /// The tool, as the model named it.
    pub tool: String,
    /// `henk`, `workspace` or an MCP server's alias.
    pub origin: String,
    /// `ok`, `error`, `refused_scope`, `refused_repeat`, `unknown_tool`,
    /// `malformed_arguments`, `not_run` or `cancelled`.
    pub outcome: String,
    /// The arguments as the model sent them, perhaps cut.
    pub arguments: String,
    /// The arguments' length in bytes before any cut.
    pub arguments_len: u64,
    /// The result's size in characters before it was cut for the model.
    pub result_chars: u64,
    /// How long it ran.
    pub elapsed_ms: u64,
}

/// One session's whole conversation (#191).
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Transcript {
    /// The session.
    pub session: String,
    /// Its model.
    pub model: String,
    /// Why it stopped.
    pub stop: String,
    /// Model turns taken.
    pub turns: u64,
    /// Prompt tokens, all of them.
    pub prompt_tokens: u64,
    /// Tokens generated.
    pub output_tokens: u64,
    /// The system prompt.
    pub system: String,
    /// The conversation, in order.
    pub messages: Vec<Message>,
}

/// One message of a conversation.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Message {
    /// `user` or `assistant`.
    pub role: String,
    /// The model turn it belongs to.
    pub turn: u64,
    /// Its content, in order.
    pub parts: Vec<Part>,
}

/// One block of a message.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Part {
    /// Text.
    Text {
        /// The text.
        text: String,
    },
    /// A call the model made.
    Call {
        /// The tool.
        name: String,
        /// The arguments, as JSON or as the model wrote them.
        arguments: String,
    },
    /// What a call returned.
    Result {
        /// Whether the tool failed.
        error: bool,
        /// What it returned.
        content: String,
    },
    /// Provider content the model wanted echoed back, such as thinking.
    Opaque,
}

/// An inbound event: a webhook, an API or dashboard request.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct EventSummary {
    /// Its id.
    pub id: String,
    /// RFC 3339.
    pub received_at: String,
    /// `github_webhook`, `api`, `dashboard`, and so on.
    pub source: String,
    /// `pull_request`, `review_requested`, and so on.
    pub kind: String,
    /// `owner/name`, when it is about a repository.
    pub repo: Option<String>,
    /// The pull request, merge request or issue number.
    pub target: Option<u64>,
    /// Who asked, as a stable id (§2).
    pub requester: Option<String>,
}

/// An inbound event with what each listener did with it.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct EventItem {
    /// The event.
    pub event: EventSummary,
    /// What each listener did, in order.
    pub outcomes: Vec<ListenerOutcome>,
}

/// One inbound event in full.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct EventDetail {
    /// The event.
    pub event: EventSummary,
    /// The payload as received: data, never instructions (§8.3).
    pub payload: Option<String>,
    /// What each listener did, in order.
    pub outcomes: Vec<ListenerOutcome>,
}

/// What one listener did with an event.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ListenerOutcome {
    /// The listener.
    pub listener: String,
    /// `started`, `ignored`, `refused`, and so on.
    pub outcome: String,
    /// In words.
    pub detail: String,
    /// The run it led to.
    pub run_id: Option<String>,
    /// RFC 3339.
    pub at: String,
}

/// What the service has, from configuration and the database.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Health {
    /// One row per check.
    pub checks: Vec<HealthCheck>,
}

/// One health check.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct HealthCheck {
    /// What was checked.
    pub name: String,
    /// `ok`, `warn` or `fail`.
    pub state: String,
    /// The detail.
    pub detail: String,
}

/// What `POST /runs` takes: what the dashboard's start form takes.
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct StartRequest {
    /// `review`, `plan` or `address`.
    pub kind: String,
    /// The pull request, merge request or issue.
    pub url: String,
    /// The commit to review; the head when none.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub commit: Option<String>,
    /// A note for a plan or an address run.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub note: Option<String>,
}

/// A start request became this event; its outcomes say what came of it.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Started {
    /// The event's id.
    pub event_id: String,
}

/// A cancel was sent to this run.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Cancelled {
    /// The run.
    pub run_id: String,
}

/// Every error's body.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ErrorBody {
    /// The error.
    pub error: ErrorDetail,
}

/// What went wrong.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ErrorDetail {
    /// `unauthenticated`, `forbidden`, `bad_request`, `not_found`,
    /// `conflict`, `unsupported_media_type` or `store`.
    pub code: String,
    /// In words.
    pub message: String,
}

/// A run kind's API name, the one `?kind=` takes.
pub fn kind_name(kind: RunKind) -> &'static str {
    match kind {
        RunKind::Review => "review",
        RunKind::Plan => "plan",
        RunKind::DiscordTurn => "discord_turn",
        RunKind::MailReply => "mail_reply",
        RunKind::Address => "address",
    }
}

impl RunSummary {
    pub(super) fn from_record(settings: &Settings, run: &RunRecord) -> Self {
        Self {
            id: run.id.as_str().to_owned(),
            kind: kind_name(run.kind).to_owned(),
            platform: match run.platform {
                Platform::GitHub => "github",
                Platform::GitLab => "gitlab",
            }
            .to_owned(),
            repo: run.repo.clone(),
            target: run.target,
            target_url: target_url(settings, run),
            commit: run.commit.clone(),
            status: run.status.as_str().to_owned(),
            trigger: run.trigger.clone(),
            requester: run.requester.clone(),
            started_at: run.started_at.clone(),
            finished_at: run.finished_at.clone(),
            superseded_by: run.superseded_by.as_ref().map(|by| by.as_str().to_owned()),
            lanes: Vec::new(),
            stages: Vec::new(),
        }
    }
}

impl From<&LaneRecord> for Lane {
    fn from(lane: &LaneRecord) -> Self {
        Self {
            name: lane.name.clone(),
            model: lane.model.clone(),
            status: lane.status.as_str().to_owned(),
            turns: lane.turns,
            input_tokens: lane.input_tokens,
            output_tokens: lane.output_tokens,
            error: lane.error.clone(),
            last_call_turn: None,
            started_at: lane.started_at.clone(),
            finished_at: lane.finished_at.clone(),
        }
    }
}

impl From<&FindingRecord> for Finding {
    fn from(finding: &FindingRecord) -> Self {
        Self {
            at: finding.at.clone(),
            lane: finding.lane.clone(),
            path: finding.path.clone(),
            line: finding.line,
            comment_id: finding.comment_id.clone(),
            action: finding.action.clone(),
        }
    }
}

impl From<&DraftRecord> for Draft {
    fn from(draft: &DraftRecord) -> Self {
        Self {
            at: draft.at.clone(),
            id: draft.draft.clone(),
            lane: draft.lane.clone(),
            model: draft.model.clone(),
            kind: draft.kind.clone(),
            path: draft.path.clone(),
            line: draft.line,
            target: draft.target.clone(),
            body: draft.body.clone(),
            decision: draft.decision.as_ref().map(|d| DraftDecision {
                at: d.at.clone(),
                verdict: d.verdict.as_str().to_owned(),
                checker: d.checker.clone(),
                reason: d.reason.clone(),
                same_as: d.same_as.clone(),
                comment_id: d.comment_id.clone(),
            }),
        }
    }
}

impl From<&TranscriptSummary> for TranscriptRef {
    fn from(transcript: &TranscriptSummary) -> Self {
        Self {
            at: transcript.at.clone(),
            session: transcript.session.clone(),
            model: transcript.model.clone(),
            stop: transcript.stop.clone(),
            turns: transcript.turns,
            bytes: transcript.bytes,
        }
    }
}

impl From<&ToolUsage> for ToolUsageRow {
    fn from(usage: &ToolUsage) -> Self {
        Self {
            session: usage.session.clone(),
            tool: usage.tool.clone(),
            calls: usage.tally.calls,
            errors: usage.tally.errors,
            refusals: usage.tally.refusals,
            other: usage.tally.other,
            total_ms: usage.tally.total_ms,
        }
    }
}

impl From<&EventRecord> for RunEvent {
    fn from(event: &EventRecord) -> Self {
        Self {
            at: event.at.clone(),
            level: event.level.clone(),
            message: event.message.clone(),
        }
    }
}

impl From<&ToolCallRecord> for ToolCall {
    fn from(call: &ToolCallRecord) -> Self {
        Self {
            at: call.at.clone(),
            session: call.session.clone(),
            model: call.model.clone(),
            turn: call.turn,
            tool: call.tool.clone(),
            origin: call.origin.clone(),
            outcome: call.outcome.clone(),
            arguments: call.arguments.clone(),
            arguments_len: call.arguments_len,
            result_chars: call.result_chars,
            elapsed_ms: call.elapsed_ms,
        }
    }
}

impl From<TranscriptView> for Transcript {
    fn from(view: TranscriptView) -> Self {
        Self {
            session: view.session,
            model: view.model,
            stop: view.stop,
            turns: view.turns,
            prompt_tokens: view.tokens.0,
            output_tokens: view.tokens.1,
            system: view.system,
            messages: view.messages.into_iter().map(Message::from).collect(),
        }
    }
}

impl From<MessageView> for Message {
    fn from(message: MessageView) -> Self {
        Self {
            role: message.role,
            turn: message.turn,
            parts: message
                .parts
                .into_iter()
                .map(|part| match part {
                    PartView::Text(text) => Part::Text { text },
                    PartView::Call { name, arguments } => Part::Call { name, arguments },
                    PartView::Result { error, content } => Part::Result { error, content },
                    PartView::Opaque => Part::Opaque,
                })
                .collect(),
        }
    }
}

impl From<&InboundEvent> for EventSummary {
    fn from(event: &InboundEvent) -> Self {
        Self {
            id: event.id.as_str().to_owned(),
            received_at: event.received_at.clone(),
            source: event.source.clone(),
            kind: event.kind.clone(),
            repo: event.repo.clone(),
            target: event.target,
            requester: event.requester.clone(),
        }
    }
}

impl From<&OutcomeRecord> for ListenerOutcome {
    fn from(outcome: &OutcomeRecord) -> Self {
        Self {
            listener: outcome.listener.clone(),
            outcome: outcome.outcome.clone(),
            detail: outcome.detail.clone(),
            run_id: outcome.run_id.clone(),
            at: outcome.at.clone(),
        }
    }
}

impl From<&EventWithOutcomes> for EventItem {
    fn from(item: &EventWithOutcomes) -> Self {
        Self {
            event: EventSummary::from(&item.event),
            outcomes: item.outcomes.iter().map(ListenerOutcome::from).collect(),
        }
    }
}

/// A link to the pull request, merge request or issue a run is about.
fn target_url(settings: &Settings, run: &RunRecord) -> Option<String> {
    let issue = run.kind == RunKind::Plan;
    link_to(settings, run.platform, &run.repo, run.target, issue)
}

/// A link to pull request, merge request or issue `target` of `repo`.
pub(super) fn link_to(
    settings: &Settings,
    platform: Platform,
    repo: &str,
    target: u64,
    issue: bool,
) -> Option<String> {
    match platform {
        Platform::GitHub => {
            let api = settings
                .github
                .as_ref()
                .map_or("https://api.github.com", |g| g.api_base.as_str());
            let web = if api.trim_end_matches('/') == "https://api.github.com" {
                "https://github.com".to_owned()
            } else {
                api.trim_end_matches('/')
                    .trim_end_matches("/api/v3")
                    .to_owned()
            };
            let what = if issue { "issues" } else { "pull" };
            Some(format!("{web}/{repo}/{what}/{target}"))
        }
        Platform::GitLab => {
            let api = settings.gitlab.as_ref()?.api_url.trim_end_matches('/');
            let web = api.trim_end_matches("/api/v4");
            let what = if issue { "issues" } else { "merge_requests" };
            Some(format!("{web}/{repo}/-/{what}/{target}"))
        }
    }
}
