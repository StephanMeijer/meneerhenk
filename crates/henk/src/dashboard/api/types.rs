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
    DraftRecord, EventRecord, EventWithOutcomes, FindingRecord, InboundEvent, OutcomeRecord,
    RoundRecord, RunRecord, SessionRecord, StageRecord, ToolCallRecord, ToolUsage,
    TranscriptSummary,
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
    /// What `POST /runs` can start here, as configured: `review` with
    /// `[review.loop]`, `plan` with `[planning]`, `execute` with `[execution]`.
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
    /// `review`, `plan`, `execute`, `rebase`, `fix_ci`, `ticket`, `address`,
    /// `discord_turn` or `mail_reply`.
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
    /// Why a review loop stopped, such as `converged` (#286).
    pub loop_stop: Option<String>,
    /// How many rounds a review loop ran (#286).
    pub loop_rounds: Option<u32>,
    /// Where each of its sessions stands, for lists (#225). Empty where
    /// the whole run is sent with its sessions, and on a `run` stream
    /// message.
    pub sessions: Vec<SessionDot>,
    /// Where each of its stages stands, for lists (#225). Empty as
    /// `sessions`.
    pub stages: Vec<StageDot>,
}

/// A session as a list shows it: its name, role and status.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SessionDot {
    /// `reviewer`, `executor`, `planner`; `lane-a`, `check-1` in older runs.
    pub name: String,
    /// `planner`, `reviewer` or `executor`; older runs also `lane`, `check`
    /// and `address`.
    pub role: String,
    /// `running`, `finished`, `timed_out` or `did_not_finish`.
    pub status: String,
}

/// A stage as a list shows it: its name and state.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct StageDot {
    /// `diff`, `rounds`, `publish`, and so on.
    pub name: String,
    /// `running`, `done`, `failed` or `skipped`.
    pub state: String,
}

/// The sessions and stages of one run moved on: a message of `/runs/stream`.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Progress {
    /// Which run.
    pub run_id: String,
    /// Every session, when sessions moved; `null` when only stages did.
    pub sessions: Option<Vec<SessionDot>>,
    /// Every stage, when stages moved; `null` when only sessions did.
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

/// The ssh backend's sandbox hosts and their slots (#403); no hosts when
/// it is not configured.
#[derive(Debug, Serialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SandboxHosts {
    /// Every host, in id order.
    pub hosts: Vec<SandboxHost>,
    /// Runs waiting for a host with room.
    pub waiting: u64,
}

/// One sandbox host's slots.
#[derive(Debug, Serialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SandboxHost {
    /// The host's id.
    pub id: String,
    /// Its `max_concurrent`; `null` is no limit.
    pub limit: Option<u64>,
    /// Workspaces open on it.
    pub in_use: u64,
}

impl From<crate::workspace::SandboxHosts> for SandboxHosts {
    fn from(now: crate::workspace::SandboxHosts) -> Self {
        let count = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
        Self {
            hosts: now
                .hosts
                .into_iter()
                .map(|h| SandboxHost {
                    id: h.id,
                    limit: h.limit.map(count),
                    in_use: count(h.in_use),
                })
                .collect(),
            waiting: count(now.waiting),
        }
    }
}

/// A review or rebase waiting for a slot (#251, #580), or a review
/// waiting for a rebase of its pull request. It has its run id, but no run
/// record until it starts.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct WaitingReview {
    /// The run it will be; cancel it with `POST /runs/{id}/cancel`.
    pub run_id: String,
    /// `review` or `rebase`; plans join the queue with #79.
    pub kind: String,
    /// `rebase` waits ahead of every `review` (#580); a move keeps it so.
    pub priority: String,
    /// `github` or `gitlab`.
    pub platform: String,
    /// `owner/name`.
    pub repo: String,
    /// The pull or merge request.
    pub target: u64,
    /// A link to it, when one can be made.
    pub target_url: Option<String>,
    /// The commit it will review; `null` for a rebase.
    pub commit: Option<String>,
    /// What started it.
    pub trigger: String,
    /// Who asked, as a stable id, when someone did.
    pub requester: Option<String>,
    /// RFC 3339: since when it waits, the time it was requested.
    pub since: String,
    /// Its place in the queue, from 1: the order it starts in.
    pub position: u64,
    /// Why it waits: `no_slot`, every slot is taken or a rebase waits
    /// ahead; `after_rebase`, a rebase of its pull request comes first.
    pub reason: String,
    /// The rebase it waits for, with `after_rebase`.
    pub waits_for: Option<String>,
}

impl WaitingReview {
    pub(super) fn from_waiting(settings: &Settings, waiting: crate::coordinator::Waiting) -> Self {
        Self {
            run_id: waiting.run.as_str().to_owned(),
            kind: kind_name(waiting.kind).to_owned(),
            priority: waiting.priority.as_str().to_owned(),
            platform: match waiting.platform {
                Platform::GitHub => "github",
                Platform::GitLab => "gitlab",
            }
            .to_owned(),
            target_url: link_to(
                settings,
                waiting.platform,
                &waiting.repo,
                waiting.number,
                false,
            ),
            repo: waiting.repo,
            target: waiting.number,
            commit: waiting.commit.map(|c| c.as_str().to_owned()),
            trigger: waiting.trigger,
            requester: waiting.requester,
            since: waiting
                .since
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_default(),
            position: u64::try_from(waiting.position).unwrap_or(u64::MAX),
            reason: waiting.reason.as_str().to_owned(),
            waits_for: waiting.waits_for.map(|run| run.as_str().to_owned()),
        }
    }
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

/// How the sessions of the recent reviews ended (#229):
/// `GET /stats/sessions`.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SessionStats {
    /// The reviews, oldest first: the grid's columns.
    pub reviews: Vec<ReviewMark>,
    /// The reviewer and the executor, then older reviews' lanes and
    /// fact-check sessions: the grid's rows.
    pub sessions: Vec<SessionRow>,
}

/// One review in the grid.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ReviewMark {
    /// The run.
    pub run_id: String,
    /// RFC 3339.
    pub started_at: String,
}

/// One session, by name, across the reviews.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SessionRow {
    /// `reviewer`, `executor`; `lane-a`, `check-1` in older reviews.
    pub name: String,
    /// `reviewer` or `executor`; older reviews also `lane` and `check`.
    pub role: String,
    /// The models it ran, newest first.
    pub models: Vec<String>,
    /// Per review in `reviews`, how it ended; `null` where it did not run.
    pub outcomes: Vec<Option<SessionOutcome>>,
    /// The reviews it ran in.
    pub ran: u64,
    /// Of those, finished.
    pub finished: u64,
    /// Of those, stopped at the time limit; what it drafted until then counts.
    pub timed_out: u64,
    /// Of those, did not finish.
    pub did_not_finish: u64,
    /// Why it timed out or did not finish, counted.
    pub reasons: SessionReasons,
}

/// How a session ended in one review.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SessionOutcome {
    /// The run.
    pub run_id: String,
    /// The model it ran.
    pub model: String,
    /// `finished`, `timed_out`, `did_not_finish` or `running`.
    pub status: String,
    /// Why it did not finish: `time_limit`, `rate_limit`, `provider_error`,
    /// `cancelled`, `declined` or `stuck`; `null` when it finished.
    pub reason: Option<String>,
}

/// Why sessions did not finish, counted.
#[derive(Debug, Default, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SessionReasons {
    /// Stopped at the time limit.
    pub time_limit: u64,
    /// The model endpoint's rate limit.
    pub rate_limit: u64,
    /// Another model endpoint error.
    pub provider_error: u64,
    /// Cancelled with the review.
    pub cancelled: u64,
    /// The model declined.
    pub declined: u64,
    /// Stuck repeating a tool call.
    pub stuck: u64,
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
    /// Drafts written.
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
    /// Its sessions: the reviewer, the executor, the planner; older runs
    /// have review lanes, checks and an address run's session.
    pub sessions: Vec<Session>,
    /// A pairing's rounds in the order they started (#374): which role
    /// was resumed in which round, and with what. Empty for runs without
    /// a pairing and for runs from before #374.
    pub rounds: Vec<Round>,
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
/// `diff`, `checkout`, `rounds`, `push`, `publish`, `done`; a plan goes
/// through `workspace` and `session` instead. Reviews before #324 have
/// `lanes` for `rounds`, runs before #309 `fact_check`, and address runs
/// before #327 `commit` and `replies`.
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
    /// The sandbox hosts' slots, and how many runs wait for one (#403).
    pub sandbox_hosts: SandboxHosts,
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
    /// A session started or ended: every session of the run.
    Sessions(Vec<Session>),
    /// A round of a pairing started or ended: every round of the run.
    Rounds(Vec<Round>),
    /// A session called a tool.
    ToolCall(ToolCall),
    /// A session queued a draft, or a verdict decided one.
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

/// One message of `/runs/{id}/sessions/{session}/stream` (#238).
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum SessionMessage {
    /// What the session said as far back as is kept: the first message,
    /// and after a gap.
    Snapshot(LiveSnapshot),
    /// It said something.
    Message(LiveMessage),
    /// It ended, or does not run in this process; the stream closes.
    End(SessionEnd),
}

/// A running session's conversation as far back as is kept.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct LiveSnapshot {
    /// Oldest first.
    pub messages: Vec<LiveMessage>,
    /// Older messages were let go; the transcript has them once the
    /// session ends.
    pub cut: bool,
}

/// One message of a running session, as it was appended: before
/// compaction, so it can be longer than in the stored transcript.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct LiveMessage {
    /// Its place in the session, from 1.
    pub seq: u64,
    /// When it was appended, RFC 3339.
    pub at: String,
    /// The message.
    pub message: Message,
}

/// Why a session's stream closes.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct SessionEnd {
    /// It runs in another Henk process, whose sessions this one does not
    /// hear; its tool calls still show on the run.
    pub elsewhere: bool,
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
    /// A running run's sessions or stages moved on.
    Progress(Progress),
    /// A review started waiting, took or freed a slot, or left the queue.
    Slots(Slots),
    /// A workspace took or freed a sandbox host's slot, or a run started
    /// or stopped waiting for one (#403).
    SandboxHosts(SandboxHosts),
}

/// The sources and kinds Henk has recorded, for the events page's filters
/// (#227): `GET /events/facets`.
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct EventFacets {
    /// `github_webhook`, `api`, `dashboard`, and so on, sorted.
    pub sources: Vec<String>,
    /// `pull_request`, `review_requested`, and so on, sorted.
    pub kinds: Vec<String>,
}

/// One tool of one model in one kind of session, across runs (#203).
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct ToolSummaryRow {
    /// The tool, as the model named it.
    pub tool: String,
    /// The model.
    pub model: String,
    /// The role of the sessions that made the calls: `planner`, `reviewer`
    /// or `executor`; older runs also `lane`, `check` and `address`.
    pub role: String,
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
pub struct Session {
    /// `reviewer`, `executor`, `planner`; `lane-a`, `check-1` and
    /// `address` in older runs.
    pub name: String,
    /// The role it ran in: `planner`, `reviewer` or `executor`; older runs
    /// also `lane`, `check` and `address`.
    pub role: String,
    /// The model.
    pub model: String,
    /// `running`, `finished`, `timed_out` (stopped at its time limit; what
    /// it drafted counts) or `did_not_finish` (see `error`), #231.
    pub status: String,
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
    /// Why it did not finish.
    pub error: Option<String>,
    /// The turn of its latest tool call: how far a running session has come,
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
    /// The session.
    pub session: String,
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

/// A session's draft and what became of it (#189).
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Draft {
    /// RFC 3339, when it was queued.
    pub at: String,
    /// Its number in the review: `d3`.
    pub id: String,
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
    /// `review`, `plan` or `execute` (#329).
    pub kind: String,
    /// The pull request, merge request or issue.
    pub url: String,
    /// The commit to review; the head when none.
    #[serde(default)]
    #[cfg_attr(test, ts(optional = nullable))]
    pub commit: Option<String>,
    /// A note for a plan or an execute run.
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

/// What `POST /runs/{id}/queue` takes: where the waiting review goes
/// (#401).
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct MoveBody {
    /// Its new place, 1 being the next to start; past the end is last.
    pub position: u64,
}

/// A waiting review moved in the queue (#401).
#[derive(Debug, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
pub struct Moved {
    /// The run.
    pub run_id: String,
    /// Where it was, from 1.
    pub from: u64,
    /// Where it is now, from 1.
    pub to: u64,
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
        RunKind::Execute => "execute",
        RunKind::Rebase => "rebase",
        RunKind::FixCi => "fix_ci",
        RunKind::Ticket => "ticket",
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
            loop_stop: run.loop_stop.clone(),
            loop_rounds: run.loop_rounds,
            sessions: Vec::new(),
            stages: Vec::new(),
        }
    }
}

/// One round of a session in a pairing (#374).
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[expect(
    clippy::struct_field_names,
    reason = "`round` is the round's number, as the API names it"
)]
pub struct Round {
    /// The session: `reviewer`, `executor`, `planner`.
    pub session: String,
    /// Its role: `planner`, `reviewer` or `executor`.
    pub role: String,
    /// This session's round, from 1.
    pub round: u32,
    /// RFC 3339: when the role was resumed.
    pub started_at: String,
    /// RFC 3339: when its round ended; `null` while it runs.
    pub ended_at: Option<String>,
    /// Its first model turn in the session's transcript: the
    /// `#turn-<n>` a link to it lands on.
    pub first_turn: u32,
    /// Model turns it took; 0 while it runs.
    pub turns: u32,
    /// The message it was handed, perhaps cut; the transcript holds all of
    /// it.
    pub handover: String,
    /// Whether `handover` was cut.
    pub handover_cut: bool,
}

impl From<&RoundRecord> for Round {
    fn from(round: &RoundRecord) -> Self {
        Self {
            session: round.session.clone(),
            role: round.role.as_str().to_owned(),
            round: round.round,
            started_at: round.started_at.clone(),
            ended_at: round.ended_at.clone(),
            first_turn: round.first_turn,
            turns: round.turns,
            handover: round.handover.clone(),
            handover_cut: u64::try_from(round.handover.len()).unwrap_or(u64::MAX)
                < round.handover_len,
        }
    }
}

impl From<&SessionRecord> for Session {
    fn from(session: &SessionRecord) -> Self {
        Self {
            name: session.name.clone(),
            role: session.role.as_str().to_owned(),
            model: session.model.clone(),
            status: session.status.as_str().to_owned(),
            turns: session.turns,
            input_tokens: session.input_tokens,
            output_tokens: session.output_tokens,
            cache_read_tokens: session.cache_read_tokens,
            cache_write_tokens: session.cache_write_tokens,
            reasoning_tokens: session.reasoning_tokens,
            error: session.error.clone(),
            last_call_turn: None,
            started_at: session.started_at.clone(),
            finished_at: session.finished_at.clone(),
        }
    }
}

impl From<&FindingRecord> for Finding {
    fn from(finding: &FindingRecord) -> Self {
        Self {
            at: finding.at.clone(),
            session: finding.session.clone(),
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
            session: draft.session.clone(),
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
    let issue = run.kind.is_about_an_issue();
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
            let web = henk_platform::github::web_base(api);
            let what = if issue { "issues" } else { "pull" };
            Some(format!("{web}/{repo}/{what}/{target}"))
        }
        Platform::GitLab => {
            let web = henk_platform::gitlab::web_base(&settings.gitlab.as_ref()?.api_url);
            let what = if issue { "issues" } else { "merge_requests" };
            Some(format!("{web}/{repo}/-/{what}/{target}"))
        }
    }
}
