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
csrf: string, };

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
 * `running`, `finished`, `failed` or `cancelled`.
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
finished_at: string | null, };

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
requests: Array<EventSummary>, };

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
 * `running`, `finished` or `dropped`.
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
 * Why it was dropped.
 */
error: string | null, };

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
