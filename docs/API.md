# Dashboard API

What the dashboard shows and does, as JSON, at `/dashboard/api/v1` (#198).
The dashboard app at `/dashboard` (#197) is its client. It exists only when the
`[dashboard]` table is configured, as the dashboard does.

The older `POST /review`, `/plan` and `/address` endpoints with one shared
bearer token (`hooks/api.rs`) are a separate thing and are unchanged.

## Authentication

- **Reads** need the dashboard's sign-in session, the `henk_session` cookie
  that GitHub sign-in at `/dashboard/login` sets. The cookie's path is
  `/dashboard`, which is why the API lives under it.
- No session, or an expired one: `401 unauthenticated`. A session whose
  GitHub id is no longer in `allowed_github_ids`: `403 forbidden`. The
  API never redirects.
- **Actions** (`POST`) also need all three of these, or they are a `403`:
  - the session's CSRF token in the `X-CSRF-Token` header (`GET /me` gives
    it);
  - an `Origin` (or, without one, `Referer`) of `server.public_base_url`;
  - a JSON body with `Content-Type: application/json`. A form is a
    `415 unsupported_media_type`.
- Personal API tokens for scripts are #200.

## Conventions

- Every response is JSON, with `Cache-Control: no-store` and
  `X-Content-Type-Options: nosniff`.
- Text is returned as stored, never HTML-escaped. Comment bodies, payloads,
  transcripts and tool arguments are other people's words (§8.3): show them
  as text, never as markup.
- Kinds and statuses are lowercase names: `review`, `running`, `confirmed`.
- Times are RFC 3339 strings.
- Identity is an id (§2): `requester` is `github:<id>` or the API's
  configured requester, never a display name.
- No response holds a secret.

### Errors

```json
{"error": {"code": "not_found", "message": "No such run."}}
```

| Code | Status | When |
|---|---|---|
| `unauthenticated` | 401 | No current session |
| `forbidden` | 403 | Id not on the list; an action from another origin |
| `csrf` | 403 | An action without its session's CSRF token, or another session's; a reload gets a fresh one |
| `bad_request` | 400 | A bad id, filter, cursor or body |
| `unsupported_media_type` | 415 | An action whose body is not JSON |
| `not_found` | 404 | No such run, event, transcript or route |
| `conflict` | 409 | Cancelling a run that is not running here |
| `store` | 500 | The run store failed; the log has the detail |

### Paging

A listing returns `{"items": [...], "next": "<cursor>" | null}`, newest
first. Pass `next` back as `?cursor=` for the next page; `null` means there
is no more. `?limit=` takes 1 to 100 and defaults to 50.

Run and event cursors are keyset cursors: a run started while you page does
not shift or repeat what you see. Treat a cursor as opaque. One the API did
not give out is a `400`.

## Types

TypeScript types for every request and response are in
`dashboard/src/lib/api/types.ts`. They are generated from
`crates/henk/src/dashboard/api/types.rs` by the test
`api_types_are_current`, which fails when the file is out of date. To
regenerate it:

```sh
HENK_BLESS=1 cargo test -p henk api_types_are_current
```

## Endpoints

### `GET /me`

Who is signed in, the CSRF token their actions send, and the kinds of run
`POST /runs` can start here (`address` only where address runs are
configured).

```json
{"github_id": 1234, "login": "alice", "csrf": "q3Jx...", "startable": ["review", "plan", "address"]}
```

### `GET /health`

What `/dashboard/health` shows: configuration and the database, without
calling a model or an MCP server (that is `henk doctor --probe`).

```json
{"checks": [{"name": "database", "state": "ok", "detail": "SQLite at henk.db"}]}
```

### `GET /runs`

Runs, newest first, each with its `lanes` (`name`, `status`) and `stages`
(`name`, `state`) as dots for lists (#225). Every filter is optional:

| Query | Meaning |
|---|---|
| `kind` | `review`, `plan`, `address`, `discord_turn` or `mail_reply` |
| `status` | `running`, `finished`, `failed`, `cancelled` or `superseded` |
| `platform` | `github` or `gitlab` |
| `repo` | `owner/name`, exactly |
| `target` | The pull request, merge request or issue number |
| `since`, `until` | Started at or after `since`, and before `until` (RFC 3339) |
| `limit`, `cursor` | Paging |

```json
{
  "items": [{
    "id": "r-20261006-1a2b3c4d", "kind": "review", "platform": "github",
    "repo": "StephanMeijer/meneerhenk", "target": 70,
    "target_url": "https://github.com/StephanMeijer/meneerhenk/pull/70",
    "commit": "7744f99", "status": "finished", "trigger": "synchronize",
    "requester": null, "started_at": "2026-10-06T08:27:11.629860842Z",
    "finished_at": "2026-10-06T08:41:02.1Z", "superseded_by": null
  }],
  "next": "MjAyNi0xMC0wNlQwODoyNzoxMS42Mjk4NjA4NDJafHItMjAyNjEwMDYtMWEyYjNjNGQ"
}
```

### `GET /runs/count`

How many runs the filters of `GET /runs` match, over every page.

```json
{"count": 3}
```

### `GET /runs/{id}`

Each lane also has `last_call_turn`: the turn of its latest tool call.

One run with everything the run record holds about it:

- `run`: the summary above; a review replaced by a review of a newer
  commit has status `superseded` and names that run in `superseded_by`;
- `summary`, `error`, `check_id`, `heartbeat_at`;
- `lanes`: every session (lanes, checks, planner, address) with turns and
  tokens, and its `status`: `running`, `finished`, `timed_out` (stopped at
  its time limit; what it drafted counts) or `did_not_finish` (`error` says
  why);
- `findings`: what was done on the platform;
- `drafts`: each draft and its `decision` (#189);
- `transcripts`: the sessions whose conversation is kept (#191);
- `tool_usage`: calls per session and tool (#190);
- `events`: the timeline;
- `requests`: the inbound events that started or joined it;
- `stages`: where the run is (#226), in the order the stages come. Each
  has `name`, `state` (`running`, `done`, `failed` or `skipped`),
  `started_at`, `ended_at` and `detail`, one line in Henk's own words
  (`12 files, +340 -25; 1 not reviewed`). A review goes `requested`,
  `queued` (the wait for a review slot), `started`, `diff`, `checkout`,
  `lanes`, `fact_check`, `publish`, `done`; a plan goes `requested`,
  `started`, `session`, `publish`, `done`; an address run `requested`,
  `started`, `workspace`, `session`, `commit`, `push`, `replies`, `done`.
  Runs from before stages were kept have none.

Each lane also has `started_at` and `finished_at`.

```json
{
  "run": {"id": "r-...", "kind": "review", "status": "finished", "...": "..."},
  "summary": "Not bad. 2 findings.", "error": null, "check_id": "4711", "heartbeat_at": "...",
  "lanes": [{"name": "lane-a", "model": "deepseek-v4-flash-0731", "status": "finished",
             "turns": 22, "input_tokens": 452700, "output_tokens": 3500, "error": null}],
  "findings": [{"at": "...", "lane": "lane-a", "path": "src/a.rs", "line": 4,
                "comment_id": "c-77", "action": "posted"}],
  "drafts": [{"at": "...", "id": "d1", "lane": "lane-a", "model": "...", "kind": "finding",
              "path": "src/a.rs", "line": 4, "target": "", "body": "...",
              "decision": {"at": "...", "verdict": "confirmed", "checker": "claude-opus-5-5",
                           "reason": "...", "same_as": "", "comment_id": "c-77"}}],
  "transcripts": [{"at": "...", "session": "lane-a", "model": "...", "stop": "EndTurn",
                   "turns": 22, "bytes": 182044}],
  "tool_usage": [{"session": "lane-a", "tool": "read_file", "calls": 9, "errors": 0,
                  "refusals": 1, "other": 0, "total_ms": 812}],
  "events": [{"at": "...", "level": "info", "message": "lane-a: EndTurn after 22 turns"}],
  "requests": [{"id": "e-...", "received_at": "...", "source": "github_webhook",
                "kind": "pull_request", "repo": "...", "target": 70, "requester": null}]
}
```

### `GET /runs/{id}/events`

The run's timeline in order, a page at a time (`limit`, `cursor`).

### `GET /runs/{id}/tool-calls`

The run's tool calls in order. Filter by `session`, `tool` and `outcome`
(`ok`, `error`, `refused_scope`, `refused_repeat`, `unknown_tool`,
`malformed_arguments`, `not_run`, `cancelled`).

```json
[{"at": "...", "session": "lane-a", "model": "...", "turn": 2, "tool": "read_file",
  "origin": "henk", "outcome": "refused_scope", "arguments": "{\"path\":\"../x\"}",
  "arguments_len": 15, "result_chars": 80, "elapsed_ms": 1}]
```

### `GET /runs/{id}/transcripts/{session}`

One session's whole conversation. Each message has `parts` tagged by
`type`: `text`, `call`, `result` or `opaque`.

```json
{
  "session": "lane-a", "model": "...", "stop": "EndTurn", "turns": 1,
  "prompt_tokens": 15, "output_tokens": 3, "system": "You are Hendrik ...",
  "messages": [
    {"role": "user", "turn": 0, "parts": [{"type": "text", "text": "Review ..."}]},
    {"role": "assistant", "turn": 1, "parts": [
      {"type": "call", "name": "read_file", "arguments": "{\"path\":\"src/a.rs\"}"}]},
    {"role": "user", "turn": 1, "parts": [
      {"type": "result", "error": false, "content": "fn main() {}"}]}
  ]
}
```

### `GET /runs/{id}/stream`

One run as it happens (#202), as Server-Sent Events (`text/event-stream`).
Each message's event name is its `kind` and its data is JSON in the types
above; `RunMessage` in `types.ts` is the union.

| Event | Data | When |
|---|---|---|
| `snapshot` | `RunDetail` | First, and whenever the stream cannot replay what was missed |
| `run` | `RunUpdate`: the run, summary, error, check id | The run started, ended or got its check |
| `lanes` | `Lane[]`, every lane | A lane started or ended |
| `tool_call` | `ToolCall` | A session called a tool |
| `draft` | `Draft` | A lane queued it, or the check decided it |
| `finding` | `Finding` | Something was done with a finding |
| `event` | `RunEvent` | A line on the timeline |
| `transcript` | `TranscriptRef` | A session's conversation was kept |
| `stages` | `Stage[]`, every stage | A stage began or ended |
| `heartbeat` | `{"at": "..."}` | The run's process said it is alive (every 30 seconds) |
| `end` | `{}` | The run has ended; the stream closes |

- Message ids are `<feed>-<seq>`, with the feed named per Henk process. A
  client that reconnects with `Last-Event-ID` (or `?last=`) gets exactly
  the messages of this run it missed, while this process still holds them
  (the last 2048 changes); otherwise it gets a new `snapshot`. Browsers
  send `Last-Event-ID` by themselves.
- A `snapshot` holds every change up to its id and none after, so no
  message that follows repeats what it shows.
- A run that has ended gets its `snapshot` and `end` at once. `end` always
  comes after a message that shows the run ended.
- A run another Henk process works on (two replicas on one PostgreSQL) is
  not on this process's feed: its stream sends a fresh `snapshot` every 5
  seconds until the run ends.
- Turns and tokens of a lane are stored when it ends. While it runs,
  `last_call_turn` on the lane (from its latest tool call) says how far it
  has come.
- A client that falls far behind is dropped and reconnects; it never holds
  up a run. A comment line every 15 seconds keeps the connection open.

```sh
curl -N -H "Cookie: henk_session=..." https://henk.example/dashboard/api/v1/runs/r-1/stream
```

### `GET /runs/stream`

What runs now, for the overview: a `snapshot` (`RunningSnapshot`: the
newest running runs, at most 100, how many run, and the review `slots`)
on connect and every 30 seconds, a `run` message (`RunSummary`) whenever
a run of this process starts or ends, and a `progress` message (`run_id`
with its `lanes` or its `stages` as dots, the other `null`) whenever a
running run's lanes or stages move on (#225), and a `slots` message
(`Slots`) whenever a review of this process starts waiting, takes or
frees a slot, or leaves the queue. `RunningMessage` in
`types.ts` is the union.

`slots` has `limit` (`review.max_concurrent`), `in_use`, and `waiting`:
the reviews waiting for a slot (`repo`, `target`, `since`), longest
waiting first. A waiting review has no run yet.

### `GET /stats/overview`

What happened per UTC day, for the overview's tiles (#225). `days` (1 to
90, default 14) counts today. The answer has `from`, `to` and `days`,
every day from `from` to `to` with:

- `runs`: runs started that day;
- `finished` and `failed`: of those, how many finished and how many
  failed. Cancelled, superseded and running runs are in neither, so
  `failed` of `finished + failed` is the share that did not complete;
- `findings_posted`: new comments, checked (`posted`) or not
  (`unverified`); improved comments are not new;
- `drafts`: drafts the lanes wrote.

### `GET /stats/lanes`

How each lane and fact-check session of the recent reviews ended (#229).
`last` (1 to 200, default 30) asks for the newest reviews; `since` (RFC
3339) for every review started since then, the newest 1000 of them. Not
both. Only reviews that ended on their own count: running ones are left
out, and so are cancelled and superseded ones, whose lanes were cancelled
for them by a person or for a newer commit.

The answer has `reviews` (`run_id`, `started_at`), oldest first, and
`lanes`: the review lanes by name, then the `check-N` sessions by number.
Each lane has:

- `kind`: `lane` or `check`; `models`: the models it ran, newest first;
- `outcomes`: one per review in `reviews`, in the same order: `run_id`,
  the `model` it ran there, `status` (`finished`, `timed_out`,
  `did_not_finish`) and `reason`, or `null` where it did not run;
- `ran`, `finished`, `timed_out`, `did_not_finish`: counts;
- `reasons`: why it timed out or did not finish, counted: `time_limit`,
  `rate_limit`, `provider_error`, `cancelled`, `declined` (the model
  declined) and `stuck` (it kept repeating a call). The reason is read
  from the error the lane ended with.

### `GET /quality`

What the fact-check made of the lanes' drafts across runs (#205), one row
per group, the largest first.

| Query | Meaning |
|---|---|
| `group` | `model` (default), `lane`, `repo` or `target` (a pull request) |
| `since`, `until` | Drafts queued at or after `since`, and before `until` (RFC 3339) |
| `repo` | `owner/name`, exactly |

Each row has the counts per verdict (`confirmed`, `rejected`, `same_as`,
`unchecked`, `not_checked`, `cancelled`, `failed`), `waiting` for drafts not
decided yet, `judged` (confirmed, rejected and repeats: what a checker
decided) and `rejection_rate`: rejected of judged, from 0 to 1, or `null`
when nothing was judged. Grouped by `target`, a row also has `repo`,
`target` and `target_url`.

```json
[{"key": "mistral-medium-3-5", "repo": null, "target": null, "target_url": null,
  "drafts": 82, "confirmed": 3, "rejected": 72, "same_as": 4, "unchecked": 1,
  "not_checked": 0, "cancelled": 0, "failed": 0, "waiting": 2,
  "judged": 79, "rejection_rate": 0.911}]
```

### `GET /quality/daily`

The rejection rate per UTC day (#228), for the six largest groups of
`/quality`'s query (same `group`, `repo`, `since`, `until`), largest
first. Each has `key` and `days`: every day from `since` (or the first day
with a draft, at most 90 days back) to today, each with `day`, `judged`,
`rejected` and `rate` (rejected of judged, `null` on a day nothing was
judged: a gap, not 0%).

### `GET /drafts/count`

How many drafts `/drafts`'s query matches over every page: `{"count": 72}`.

### `GET /drafts`

Drafts across runs, newest first, a page at a time: each with its run,
repository, pull request and what became of it.

| Query | Meaning |
|---|---|
| `verdict` | `confirmed`, `rejected`, `same_as`, `unchecked`, `not_checked`, `cancelled`, `failed`, or `waiting` for undecided |
| `model`, `lane`, `repo` | Exactly |
| `since`, `until` | As for `/quality` |
| `limit`, `cursor` | Paging |

```json
{"items": [{"run_id": "r-...", "repo": "o/r", "target": 70,
            "target_url": "https://github.com/o/r/pull/70",
            "draft": {"id": "d3", "lane": "lane-b", "model": "mistral-medium-3-5",
                      "kind": "finding", "path": "src/a.rs", "line": 91, "body": "...",
                      "decision": {"verdict": "rejected", "checker": "claude-opus-5-5",
                                   "reason": "...", "...": "..."}, "...": "..."}}],
 "next": "..."}
```

### `GET /tool-calls/summary`

How each tool fared across runs (#203): one row per tool, model and kind of
session (`lane`, `check`, `planner`, `address`), the most calls first.

| Query | Meaning |
|---|---|
| `since`, `until` | Calls made at or after `since`, and before `until` (RFC 3339) |
| `tool`, `model` | Exactly |
| `session_kind` | `lane`, `check`, `planner` or `address` |

Each row has `calls`, `errors` (the tool failed), `refusals` (the scope
guard or the repeat guard stopped it), `other` (never ran: an unknown tool,
malformed arguments, a cancel), `total_ms`, and `error_rate` and
`refusal_rate`, from 0 to 1.

```json
[{"tool": "read_file", "model": "mistral-medium-3-5", "session_kind": "lane",
  "calls": 40, "errors": 4, "refusals": 0, "other": 0, "total_ms": 1600,
  "error_rate": 0.1, "refusal_rate": 0.0}]
```

### `GET /tool-calls`

Tool calls across runs, newest first, a page at a time, each with its run,
repository and pull request. `transcript_kept` says whether the run still
keeps that session's conversation; transcripts are pruned on their own.

| Query | Meaning |
|---|---|
| `outcome` | `ok`, `error`, `refused_scope`, `refused_repeat`, `unknown_tool`, `malformed_arguments`, `not_run`, `cancelled`, or `problems` for everything but `ok` |
| `tool`, `model`, `session_kind`, `since`, `until` | As for the summary |
| `limit`, `cursor` | Paging |

```json
{"items": [{"run_id": "r-...", "repo": "o/r", "target": 70, "target_url": "...",
            "call": {"session": "lane-b", "turn": 12, "tool": "github__get_file_contents",
                     "outcome": "refused_scope", "arguments": "{...}", "...": "..."},
            "transcript_kept": true}],
 "next": null}
```

### `GET /events`

Inbound events newest first, each with what every listener did. Filter by
`source`, `kind` and `repo`; page with `limit` and `cursor`.

```json
{"items": [{"event": {"id": "e-...", "received_at": "...", "source": "dashboard",
                      "kind": "review_requested", "repo": "o/r", "target": 7,
                      "requester": "github:1234"},
            "outcomes": [{"listener": "review", "outcome": "started", "detail": "...",
                          "run_id": "r-...", "at": "..."}]}],
 "next": null}
```

### `GET /events/facets`

The sources and kinds of the inbound events Henk has recorded, each sorted,
for the events page's filters (#227):
`{"sources": ["api", "dashboard", "github_webhook"], "kinds": ["pull_request", "..."]}`.

### `GET /events/{id}`

One event with its `payload` as received (text, or `null` when none was
kept) and its outcomes.

### `POST /runs`

Starts a review, plan or address run, exactly as the dashboard's start
form does. The request becomes an event like a webhook, and the listeners
apply the allowlist and every refusal. Read the event to see what came of
it.

```json
{"kind": "review", "url": "https://github.com/o/r/pull/7", "commit": "abc1234"}
```

`commit` (reviews) and `note` (plans and address runs) are optional.
Answers `202` with `{"event_id": "e-..."}`, or `400` saying what is wrong.

### `POST /runs/{id}/cancel`

Cancels a running run, as the dashboard's cancel button does. The body is
`{}`. Answers `202` with `{"run_id": "r-..."}`, or `409 conflict` when the
run is not running in this process. The request is recorded as an event
either way.
