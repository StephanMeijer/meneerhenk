// Generated from crates/henk/src/dashboard/api/types.rs by the test
// api_types_are_current. Do not edit; run it with HENK_BLESS=1 instead.

export type Me = { 
/**
 * The GitHub user id: what decides access (§2).
 */
github_id: number, 
/**
 * The GitHub login, for display only.
 */
login: string, 
/**
 * The CSRF token every action sends in `X-CSRF-Token`.
 */
csrf: string, 
/**
 * What `POST /runs` can start here: `review`, `plan`, and `address`
 * when address runs are configured.
 */
startable: Array<string>, };

export type Page<T> = { 
/**
 * The rows.
 */
items: Array<T>, 
/**
 * The cursor of the next page; none when this was the last.
 */
next: string | null, };

export type RunSummary = { 
/**
 * Its id.
 */
id: string, 
/**
 * `review`, `plan`, `address`, `discord_turn` or `mail_reply`.
 */
kind: string, 
/**
 * `github` or `gitlab`.
 */
platform: string, 
/**
 * `owner/name`.
 */
repo: string, 
/**
 * The pull request, merge request or issue number.
 */
target: number, 
/**
 * A link to that pull request, merge request or issue.
 */
target_url: string | null, 
/**
 * The reviewed commit, for reviews.
 */
commit: string | null, 
/**
 * `running`, `finished`, `failed`, `cancelled` or `superseded` (a
 * review replaced by a review of a newer commit, #231).
 */
status: string, 
/**
 * What started it.
 */
trigger: string, 
/**
 * Who asked, as a stable id (§2).
 */
requester: string | null, 
/**
 * RFC 3339.
 */
started_at: string, 
/**
 * RFC 3339, once ended.
 */
finished_at: string | null, 
/**
 * The run that replaced it, when it was superseded.
 */
superseded_by: string | null, 
/**
 * Where each of its lanes stands, for lists (#225). Empty where the
 * whole run is sent with its lanes, and on a `run` stream message.
 */
lanes: Array<LaneDot>, 
/**
 * Where each of its stages stands, for lists (#225). Empty as `lanes`.
 */
stages: Array<StageDot>, };

export type RunCount = { 
/**
 * The number.
 */
count: number, };

export type RunUpdate = { 
/**
 * The run.
 */
run: RunSummary, 
/**
 * The summary text, once there is one.
 */
summary: string | null, 
/**
 * The error, when it failed.
 */
error: string | null, 
/**
 * The platform's id for the review's check.
 */
check_id: string | null, };

export type QualityRow = { 
/**
 * The model, lane or repository; `owner/name #7` for a pull request.
 */
key: string, 
/**
 * The repository, when grouped by pull request.
 */
repo: string | null, 
/**
 * The number, when grouped by pull request.
 */
target: number | null, 
/**
 * A link to that pull request, merge request or issue.
 */
target_url: string | null, 
/**
 * Every draft.
 */
drafts: number, 
/**
 * Confirmed and written.
 */
confirmed: number, 
/**
 * Rejected by the check.
 */
rejected: number, 
/**
 * Repeats, merged into another draft or finding.
 */
same_as: number, 
/**
 * No model could check them.
 */
unchecked: number, 
/**
 * No check was configured.
 */
not_checked: number, 
/**
 * The review ended first.
 */
cancelled: number, 
/**
 * The write failed.
 */
failed: number, 
/**
 * Not decided yet.
 */
waiting: number, 
/**
 * Drafts a checker decided: confirmed, rejected and repeats.
 */
judged: number, 
/**
 * Rejected of judged, from 0 to 1; none when nothing was judged.
 */
rejection_rate: number | null, };

export type QualitySeries = { 
/**
 * The model, lane or repository; `owner/name #7` for a pull request.
 */
key: string, 
/**
 * Every day of the period, oldest first.
 */
days: Array<DayRate>, };

export type DayRate = { 
/**
 * `YYYY-MM-DD`, UTC.
 */
day: string, 
/**
 * Drafts the check judged: confirmed, rejected and repeats.
 */
judged: number, 
/**
 * Of those, rejected.
 */
rejected: number, 
/**
 * Rejected of judged, from 0 to 1; `null` on a day it judged none.
 */
rate: number | null, };

export type DraftCount = { 
/**
 * The number.
 */
count: number, };

export type ToolSummaryRow = { 
/**
 * The tool, as the model named it.
 */
tool: string, 
/**
 * The model.
 */
model: string, 
/**
 * `lane`, `check`, `planner` or `address`.
 */
session_kind: string, 
/**
 * Every call.
 */
calls: number, 
/**
 * Calls the tool reported as failed.
 */
errors: number, 
/**
 * Calls the scope guard or the repeat guard refused.
 */
refusals: number, 
/**
 * Calls that never ran for another reason.
 */
other: number, 
/**
 * Milliseconds the calls ran, together.
 */
total_ms: number, 
/**
 * Errors of calls, from 0 to 1.
 */
error_rate: number, 
/**
 * Refusals of calls, from 0 to 1.
 */
refusal_rate: number, };

export type ToolCallItem = { 
/**
 * The run.
 */
run_id: string, 
/**
 * The run's repository.
 */
repo: string, 
/**
 * The run's pull request, merge request or issue.
 */
target: number, 
/**
 * A link to it.
 */
target_url: string | null, 
/**
 * The call.
 */
call: ToolCall, 
/**
 * Whether the run still keeps the conversation of the call's session,
 * so whether a link to its turn leads anywhere.
 */
transcript_kept: boolean, };

export type DraftItem = { 
/**
 * The run.
 */
run_id: string, 
/**
 * The run's repository.
 */
repo: string, 
/**
 * The run's pull request, merge request or issue.
 */
target: number, 
/**
 * A link to it.
 */
target_url: string | null, 
/**
 * The draft and what became of it.
 */
draft: Draft, };

export type RunningSnapshot = { 
/**
 * The newest running runs, at most 100.
 */
runs: Array<RunSummary>, 
/**
 * How many run, beyond those too.
 */
count: number, 
/**
 * The review slots and what waits for one.
 */
slots: Slots, };

export type RunMessage = { "kind": "snapshot", "data": RunDetail } | { "kind": "run", "data": RunUpdate } | { "kind": "lanes", "data": Array<Lane> } | { "kind": "tool_call", "data": ToolCall } | { "kind": "draft", "data": Draft } | { "kind": "finding", "data": Finding } | { "kind": "event", "data": RunEvent } | { "kind": "transcript", "data": TranscriptRef } | { "kind": "stages", "data": Array<Stage> } | { "kind": "heartbeat", "data": Heartbeat } | { "kind": "end" };

export type RunningMessage = { "kind": "snapshot", "data": RunningSnapshot } | { "kind": "run", "data": RunSummary } | { "kind": "progress", "data": Progress };

export type RunDetail = { 
/**
 * The run.
 */
run: RunSummary, 
/**
 * The summary text, once there is one.
 */
summary: string | null, 
/**
 * The error, when it failed.
 */
error: string | null, 
/**
 * The platform's id for the review's check.
 */
check_id: string | null, 
/**
 * RFC 3339: when its process last said it was alive.
 */
heartbeat_at: string | null, 
/**
 * Its sessions: review lanes, checks, the planner, the address run.
 */
lanes: Array<Lane>, 
/**
 * What happened to findings on the platform.
 */
findings: Array<Finding>, 
/**
 * A review's drafts and what became of each (#189).
 */
drafts: Array<Draft>, 
/**
 * The sessions whose conversation is kept (#191).
 */
transcripts: Array<TranscriptRef>, 
/**
 * Tool calls per session and tool (#190).
 */
tool_usage: Array<ToolUsageRow>, 
/**
 * The run's timeline.
 */
events: Array<RunEvent>, 
/**
 * The requests that started or joined it.
 */
requests: Array<EventSummary>, 
/**
 * Its stages, in the order they come (#226). Empty for a run from
 * before stages were kept.
 */
stages: Array<Stage>, };

export type LaneDot = { 
/**
 * `lane-a`, `check-1`, `planner`.
 */
name: string, 
/**
 * `running`, `finished`, `timed_out` or `did_not_finish`.
 */
status: string, };

export type StageDot = { 
/**
 * `diff`, `lanes`, `fact_check`, and so on.
 */
name: string, 
/**
 * `running`, `done`, `failed` or `skipped`.
 */
state: string, };

export type Progress = { 
/**
 * Which run.
 */
run_id: string, 
/**
 * Every lane, when lanes moved; `null` when only stages did.
 */
lanes: Array<LaneDot> | null, 
/**
 * Every stage, when stages moved; `null` when only lanes did.
 */
stages: Array<StageDot> | null, };

export type Slots = { 
/**
 * `review.max_concurrent`.
 */
limit: number, 
/**
 * Slots a review holds now.
 */
in_use: number, 
/**
 * Reviews waiting for a slot, longest waiting first. They have no run
 * yet.
 */
waiting: Array<WaitingReview>, };

export type WaitingReview = { 
/**
 * `owner/name`.
 */
repo: string, 
/**
 * The pull or merge request.
 */
target: number, 
/**
 * RFC 3339: since when it waits.
 */
since: string, };

export type OverviewStats = { 
/**
 * The first day, `YYYY-MM-DD`.
 */
from: string, 
/**
 * The last day, today.
 */
to: string, 
/**
 * Every day from `from` to `to`, oldest first; a quiet day is zeros.
 */
days: Array<DayStats>, };

export type DayStats = { 
/**
 * `YYYY-MM-DD`.
 */
day: string, 
/**
 * Runs started.
 */
runs: number, 
/**
 * Of those, finished.
 */
finished: number, 
/**
 * Of those, failed: what did not complete. Cancelled, superseded and
 * running runs are in neither.
 */
failed: number, 
/**
 * Findings posted: new comments, checked or not.
 */
findings_posted: number, 
/**
 * Drafts the lanes wrote.
 */
drafts: number, };

export type LaneStats = { 
/**
 * The reviews, oldest first: the grid's columns.
 */
reviews: Array<ReviewMark>, 
/**
 * The lanes, then the fact-check sessions: the grid's rows.
 */
lanes: Array<LaneRow>, };

export type SessionMessage = { "kind": "snapshot", "data": LiveSnapshot } | { "kind": "message", "data": LiveMessage } | { "kind": "end", "data": SessionEnd };

export type LiveSnapshot = { 
/**
 * Oldest first.
 */
messages: Array<LiveMessage>, 
/**
 * Older messages were let go; the transcript has them once the
 * session ends.
 */
cut: boolean, };

export type LiveMessage = { 
/**
 * Its place in the session, from 1.
 */
seq: number, 
/**
 * When it was appended, RFC 3339.
 */
at: string, 
/**
 * The message.
 */
message: Message, };

export type SessionEnd = { 
/**
 * It runs in another Henk process, whose sessions this one does not
 * hear; its tool calls still show on the run.
 */
elsewhere: boolean, };

export type ReviewMark = { 
/**
 * The run.
 */
run_id: string, 
/**
 * RFC 3339.
 */
started_at: string, };

export type LaneRow = { 
/**
 * `lane-a`, `check-1`.
 */
name: string, 
/**
 * `lane` or `check`.
 */
kind: string, 
/**
 * The models it ran, newest first.
 */
models: Array<string>, 
/**
 * Per review in `reviews`, how it ended; `null` where it did not run.
 */
outcomes: Array<LaneOutcome | null>, 
/**
 * The reviews it ran in.
 */
ran: number, 
/**
 * Of those, finished.
 */
finished: number, 
/**
 * Of those, stopped at the time limit; what it drafted until then counts.
 */
timed_out: number, 
/**
 * Of those, did not finish.
 */
did_not_finish: number, 
/**
 * Why it timed out or did not finish, counted.
 */
reasons: LaneReasons, };

export type LaneOutcome = { 
/**
 * The run.
 */
run_id: string, 
/**
 * The model it ran.
 */
model: string, 
/**
 * `finished`, `timed_out`, `did_not_finish` or `running`.
 */
status: string, 
/**
 * Why it did not finish: `time_limit`, `rate_limit`, `provider_error`,
 * `cancelled`, `declined` or `stuck`; `null` when it finished.
 */
reason: string | null, };

export type LaneReasons = { 
/**
 * Stopped at the time limit.
 */
time_limit: number, 
/**
 * The model endpoint's rate limit.
 */
rate_limit: number, 
/**
 * Another model endpoint error.
 */
provider_error: number, 
/**
 * Cancelled with the review.
 */
cancelled: number, 
/**
 * The model declined.
 */
declined: number, 
/**
 * Stuck repeating a tool call.
 */
stuck: number, };

export type Stage = { 
/**
 * Which stage.
 */
name: string, 
/**
 * `running`, `done`, `failed` or `skipped`.
 */
state: string, 
/**
 * RFC 3339.
 */
started_at: string, 
/**
 * RFC 3339, once it ended.
 */
ended_at: string | null, 
/**
 * One line in Henk's own words: `12 files, +340 -25; 1 not reviewed`.
 */
detail: string, };

export type Heartbeat = { 
/**
 * RFC 3339.
 */
at: string, };

export type Lane = { 
/**
 * `lane-a`, `check-1`, `planner`, `address`.
 */
name: string, 
/**
 * The model.
 */
model: string, 
/**
 * `running`, `finished`, `timed_out` (stopped at its time limit; what
 * it drafted counts) or `did_not_finish` (see `error`), #231.
 */
status: string, 
/**
 * Model calls made.
 */
turns: number, 
/**
 * Tokens in, not read from a cache.
 */
input_tokens: number, 
/**
 * Tokens out.
 */
output_tokens: number, 
/**
 * Why it did not finish.
 */
error: string | null, 
/**
 * The turn of its latest tool call: how far a running lane has come,
 * since turns and tokens are stored when it ends.
 */
last_call_turn: number | null, 
/**
 * RFC 3339: when it started.
 */
started_at: string, 
/**
 * RFC 3339: when it ended, once it has.
 */
finished_at: string | null, };

export type Finding = { 
/**
 * RFC 3339.
 */
at: string, 
/**
 * The lane.
 */
lane: string, 
/**
 * The file.
 */
path: string, 
/**
 * The line.
 */
line: number, 
/**
 * The platform's comment id.
 */
comment_id: string, 
/**
 * `posted`, `improved`, `refused`, `rejected`, `unverified`,
 * `withdrawn` or `merged`.
 */
action: string, };

export type Draft = { 
/**
 * RFC 3339, when it was queued.
 */
at: string, 
/**
 * Its number in the review: `d3`.
 */
id: string, 
/**
 * The lane that wrote it.
 */
lane: string, 
/**
 * The lane's model.
 */
model: string, 
/**
 * `finding`, `rewrite` or `withdrawal`.
 */
kind: string, 
/**
 * The file.
 */
path: string, 
/**
 * The line.
 */
line: number, 
/**
 * The comment a rewrite or withdrawal is about; empty for a finding.
 */
target: string, 
/**
 * The finding, the new text, or why to withdraw.
 */
body: string, 
/**
 * What became of it; none while it waits.
 */
decision: DraftDecision | null, };

export type DraftDecision = { 
/**
 * RFC 3339.
 */
at: string, 
/**
 * `confirmed`, `rejected`, `same_as`, `unchecked`, `not_checked`,
 * `cancelled` or `failed`.
 */
verdict: string, 
/**
 * The model that gave the verdict; empty when none did.
 */
checker: string, 
/**
 * Why.
 */
reason: string, 
/**
 * What it repeats, for a merge.
 */
same_as: string, 
/**
 * The comment written for it, or merged into.
 */
comment_id: string, };

export type TranscriptRef = { 
/**
 * RFC 3339.
 */
at: string, 
/**
 * The session.
 */
session: string, 
/**
 * Its model.
 */
model: string, 
/**
 * How it stopped.
 */
stop: string, 
/**
 * Model calls made.
 */
turns: number, 
/**
 * The stored conversation's size.
 */
bytes: number, };

export type ToolUsageRow = { 
/**
 * The session.
 */
session: string, 
/**
 * The tool.
 */
tool: string, 
/**
 * Every call.
 */
calls: number, 
/**
 * Calls the tool reported as failed.
 */
errors: number, 
/**
 * Calls the scope guard or the repeat guard refused.
 */
refusals: number, 
/**
 * Calls that never ran for another reason.
 */
other: number, 
/**
 * Milliseconds the calls ran, together.
 */
total_ms: number, };

export type RunEvent = { 
/**
 * RFC 3339.
 */
at: string, 
/**
 * `info`, `warn` or `error`.
 */
level: string, 
/**
 * What happened.
 */
message: string, };

export type ToolCall = { 
/**
 * RFC 3339.
 */
at: string, 
/**
 * The session.
 */
session: string, 
/**
 * The session's model.
 */
model: string, 
/**
 * The turn, from 1.
 */
turn: number, 
/**
 * The tool, as the model named it.
 */
tool: string, 
/**
 * `henk`, `workspace` or an MCP server's alias.
 */
origin: string, 
/**
 * `ok`, `error`, `refused_scope`, `refused_repeat`, `unknown_tool`,
 * `malformed_arguments`, `not_run` or `cancelled`.
 */
outcome: string, 
/**
 * The arguments as the model sent them, perhaps cut.
 */
arguments: string, 
/**
 * The arguments' length in bytes before any cut.
 */
arguments_len: number, 
/**
 * The result's size in characters before it was cut for the model.
 */
result_chars: number, 
/**
 * How long it ran.
 */
elapsed_ms: number, };

export type Transcript = { 
/**
 * The session.
 */
session: string, 
/**
 * Its model.
 */
model: string, 
/**
 * Why it stopped.
 */
stop: string, 
/**
 * Model turns taken.
 */
turns: number, 
/**
 * Prompt tokens, all of them.
 */
prompt_tokens: number, 
/**
 * Tokens generated.
 */
output_tokens: number, 
/**
 * The system prompt.
 */
system: string, 
/**
 * The conversation, in order.
 */
messages: Array<Message>, };

export type Message = { 
/**
 * `user` or `assistant`.
 */
role: string, 
/**
 * The model turn it belongs to.
 */
turn: number, 
/**
 * Its content, in order.
 */
parts: Array<Part>, };

export type Part = { "type": "text", 
/**
 * The text.
 */
text: string, } | { "type": "call", 
/**
 * The tool.
 */
name: string, 
/**
 * The arguments, as JSON or as the model wrote them.
 */
arguments: string, } | { "type": "result", 
/**
 * Whether the tool failed.
 */
error: boolean, 
/**
 * What it returned.
 */
content: string, } | { "type": "opaque" };

export type EventSummary = { 
/**
 * Its id.
 */
id: string, 
/**
 * RFC 3339.
 */
received_at: string, 
/**
 * `github_webhook`, `api`, `dashboard`, and so on.
 */
source: string, 
/**
 * `pull_request`, `review_requested`, and so on.
 */
kind: string, 
/**
 * `owner/name`, when it is about a repository.
 */
repo: string | null, 
/**
 * The pull request, merge request or issue number.
 */
target: number | null, 
/**
 * Who asked, as a stable id (§2).
 */
requester: string | null, };

export type EventItem = { 
/**
 * The event.
 */
event: EventSummary, 
/**
 * What each listener did, in order.
 */
outcomes: Array<ListenerOutcome>, };

export type EventDetail = { 
/**
 * The event.
 */
event: EventSummary, 
/**
 * The payload as received: data, never instructions (§8.3).
 */
payload: string | null, 
/**
 * What each listener did, in order.
 */
outcomes: Array<ListenerOutcome>, };

export type EventFacets = { 
/**
 * `github_webhook`, `api`, `dashboard`, and so on, sorted.
 */
sources: Array<string>, 
/**
 * `pull_request`, `review_requested`, and so on, sorted.
 */
kinds: Array<string>, };

export type ListenerOutcome = { 
/**
 * The listener.
 */
listener: string, 
/**
 * `started`, `ignored`, `refused`, and so on.
 */
outcome: string, 
/**
 * In words.
 */
detail: string, 
/**
 * The run it led to.
 */
run_id: string | null, 
/**
 * RFC 3339.
 */
at: string, };

export type Health = { 
/**
 * One row per check.
 */
checks: Array<HealthCheck>, };

export type HealthCheck = { 
/**
 * What was checked.
 */
name: string, 
/**
 * `ok`, `warn` or `fail`.
 */
state: string, 
/**
 * The detail.
 */
detail: string, };

export type StartRequest = { 
/**
 * `review`, `plan` or `address`.
 */
kind: string, 
/**
 * The pull request, merge request or issue.
 */
url: string, 
/**
 * The commit to review; the head when none.
 */
commit?: string | null, 
/**
 * A note for a plan or an address run.
 */
note?: string | null, };

export type Started = { 
/**
 * The event's id.
 */
event_id: string, };

export type Cancelled = { 
/**
 * The run.
 */
run_id: string, };

export type ErrorBody = { 
/**
 * The error.
 */
error: ErrorDetail, };

export type ErrorDetail = { 
/**
 * `unauthenticated`, `forbidden`, `bad_request`, `not_found`,
 * `conflict`, `unsupported_media_type` or `store`.
 */
code: string, 
/**
 * In words.
 */
message: string, };
