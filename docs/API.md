# Dashboard API

What the dashboard shows and does, as JSON, at `/dashboard/api/v1` (#198).
The dashboard app at `/dashboard` (#197) is its client. It exists only when the
`[dashboard]` table is configured, as the dashboard does.

The older `POST /review` and `/plan` endpoints with one shared bearer
token (`hooks/api.rs`) are a separate thing and are unchanged.
Agents use the same reads and starts over MCP at `/mcp`, with their own
tokens (README, "MCP server"); the tools answer with the types below.

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
`POST /runs` can start here, as configured: `review` with `[review.loop]`,
`plan` with `[planning]` and `execute` with `[execution]` (#329).

```json
{"github_id": 1234, "login": "alice", "csrf": "q3Jx...", "startable": ["review", "plan", "execute"]}
```

### `GET /health`

What `/dashboard/health` shows: configuration and the database, without
calling a model or an MCP server (that is `henk doctor --probe`).

```json
{"checks": [{"name": "database", "state": "ok", "detail": "SQLite at henk.db"}]}
```

### `GET /runs`

Runs, newest first, each with its `sessions` (`name`, `role`, `status`) and
`stages` (`name`, `state`) as dots for lists (#225). Every filter is optional:

| Query | Meaning |
|---|---|
| `kind` | `review`, `plan`, `execute` (an issue taken to a pull request, #329), `address` (runs from before #327), `discord_turn` or `mail_reply` |
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

One run with everything the run record holds about it:

- `run`: the summary above; a review replaced by a review of a newer
  commit has status `superseded` and names that run in `superseded_by`;
- `summary`, `error`, `check_id`, `heartbeat_at`;
- `sessions`: every model session, each with its `name` and its `role`:
  `planner`, `reviewer` or `executor` (a review loop's `reviewer` and
  `executor`, a plan's `planner` and `reviewer`, an execute run's
  `executor`). Sessions of older runs keep their names and carry a history
  role (#324): the `fixer` of runs before #323 is `executor`, the review
  lanes of reviews before #309 are `lane` and their `check-N` fact-check
  sessions `check`, and the session of an address run from before #327 is
  `address`. Each has turns and tokens (`input_tokens` the whole prompt, of which
  `cache_read_tokens` came from the model's prompt cache and
  `cache_write_tokens` went into it, #330), and its `status`:
  `running`, `finished`, `timed_out` (stopped at its time limit) or
  `did_not_finish` (`error` says why), `started_at`, `finished_at`, and
  `last_call_turn`: the turn of its latest tool call;
- `rounds`: a pairing's rounds in the order they started (#374): each
  `session` and its `role`, its `round` (from 1, per session),
  `started_at`, `ended_at` (`null` while it runs), `first_turn` (its first
  model turn in the session's transcript: the `#turn-<n>` a link lands
  on), `turns`, and `handover`, the message the role was resumed with,
  cut at 8 KiB (`handover_cut` says so; the transcript holds all of it).
  Empty for a run without a pairing and for runs from before #374;
- `findings`: what was done with a finding comment on the platform, with
  the `session` that wrote it. Only reviews made by lanes have any: a
  review loop posts nothing;
- `drafts`: each with the `session` that wrote it. A review loop's
  findings, `kind` `loop_finding` from the `reviewer`, each with the executor's `decision`: `fixed` (its commit in
  `comment_id`), `rejected`, `wont_fix`, or `unsettled` when the loop ended
  first (#285). A plan's drafts are its reviewer's points, `kind`
  `plan_point`, with `path` the part of the plan the point is about, `line`
  0, `target` the rejected point it contests if any, and the planner's
  `decision`: `taken`, `rejected`, or `unsettled` when the review ended
  first (#328). Reviews made by lanes hold the lanes' drafts and the
  fact-check's decisions instead (#189);
- `transcripts`: the sessions whose conversation is kept (#191);
- `tool_usage`: calls per session and tool (#190);
- `events`: the timeline;
- `requests`: the inbound events that started or joined it;
- `stages`: where the run is (#226), in the order the stages come. Each
  has `name`, `state` (`running`, `done`, `failed` or `skipped`),
  `started_at`, `ended_at` and `detail`, one line in Henk's own words
  (`12 files, +340 -25; 1 not reviewed`). A review goes `requested`,
  `queued` (the wait for a review slot), `started`, `diff`, `checkout`,
  `rounds` (the review loop's rounds), `push` (when the loop pushed, or
  skipped when it may not), `publish` (skipped: a review posts nothing),
  `done`. Reviews from before #324 have `lanes` where `rounds` is, and runs
  from before #309 can have a `fact_check` stage after it. A plan goes `requested`,
  `started`, `session` (the planner's draft and the review's rounds),
  `publish` (nothing reaches the issue before it), `done`. A run's
  `loop_stop` and `loop_rounds` say why and after how many rounds a review
  loop or a plan's review stopped. An execute run goes `requested`,
  `started`, `checkout`, `session` (the executor), `commit` (the checks
  and the changeset), `push` (the new branch), `publish` (the pull request
  opened), `done`; its `summary` names the pull request. An address run, from before
  #327, went `requested`, `started`, `workspace`, `session`, `commit`,
  `push`, `replies`, `done`.
  Runs from before stages were kept have none.

```json
{
  "run": {"id": "r-...", "kind": "review", "status": "finished", "...": "..."},
  "summary": "No issues found.\n\nThe review loop ran 2 rounds, settled 1 finding, pushed 1 commit and stopped: converged, the reviewer found nothing more.",
  "error": null, "check_id": "4711", "heartbeat_at": "...",
  "sessions": [{"name": "reviewer", "role": "reviewer", "model": "claude-opus-5-5", "status": "finished",
             "turns": 22, "input_tokens": 452700, "output_tokens": 3500,
             "cache_read_tokens": 431000, "cache_write_tokens": 21000, "error": null}],
  "findings": [],
  "drafts": [{"at": "...", "id": "f1", "session": "reviewer", "model": "claude-opus-5-5",
              "kind": "loop_finding", "path": "src/a.rs", "line": 4, "target": "", "body": "...",
              "decision": {"at": "...", "verdict": "fixed", "checker": "executor",
                           "reason": "...", "same_as": "", "comment_id": "9f8e7d6c5b4a"}}],
  "transcripts": [{"at": "...", "session": "reviewer", "model": "...", "stop": "EndTurn",
                   "turns": 22, "bytes": 182044}],
  "tool_usage": [{"session": "reviewer", "tool": "read_file", "calls": 9, "errors": 0,
                  "refusals": 1, "other": 0, "total_ms": 812}],
  "events": [{"at": "...", "level": "info", "message": "..."}],
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
[{"at": "...", "session": "reviewer", "model": "...", "turn": 2, "tool": "read_file",
  "origin": "henk", "outcome": "refused_scope", "arguments": "{\"path\":\"../x\"}",
  "arguments_len": 15, "result_chars": 80, "elapsed_ms": 1}]
```

### `GET /runs/{id}/transcripts/{session}`

One session's whole conversation. Each message has `parts` tagged by
`type`: `text`, `call`, `result` or `opaque`.

```json
{
  "session": "reviewer", "model": "...", "stop": "EndTurn", "turns": 1,
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
| `sessions` | `Session[]`, every session | A session started or ended |
| `rounds` | `Round[]`, every round | A round of a pairing started or ended |
| `tool_call` | `ToolCall` | A session called a tool |
| `draft` | `Draft` | The reviewer reported a finding or raised a point on a plan, or the executor or planner answered it |
| `finding` | `Finding` | Something was done with a finding comment; only a review by lanes did that |
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
- Turns and tokens of a session are stored when it ends. While it runs,
  `last_call_turn` on its session (from its latest tool call) says how far it
  has come.
- A client that falls far behind is dropped and reconnects; it never holds
  up a run. A comment line every 15 seconds keeps the connection open.

```sh
curl -N -H "Cookie: henk_session=..." https://henk.example/dashboard/api/v1/runs/r-1/stream
```

### `GET /runs/{id}/sessions/{session}/stream`

One running session's conversation as it happens (#238), as Server-Sent
Events; `SessionMessage` in `types.ts` is the union. The stored
transcript (`GET /runs/{id}/transcripts/{session}`) is kept only when a
session ends; this is the conversation until then.

| Event | Data | When |
|---|---|---|
| `snapshot` | `LiveSnapshot`: `messages`, `cut` | First, and whenever the stream cannot replay what was missed |
| `message` | `LiveMessage`: `seq`, `at`, `message` | The session appended a message: the opening (turn 0), an answer, tool results, a nudge |
| `end` | `{"elsewhere": bool}` | The session ended, or does not run in this process; the stream closes |

- A `message` is a `Message` as in a transcript, shown as text: what a
  model or a tool said is data (§8.3).
- Message ids are `<feed>-<seq>`, `seq` counting the session's messages
  from 1. A client that reconnects with `Last-Event-ID` (or `?last=`)
  gets the messages it missed while this process still holds them;
  otherwise a new `snapshot`.
- This process keeps the newest 200 messages or 512 KiB of each running
  session, and a text part longer than 16 KiB is cut with a note. `cut`
  says older messages were let go: the transcript has them once the
  session ends. Nothing here is stored.
- Messages are as the session appended them, before old tool results are
  shortened for the model, so the stored transcript can show less.
- A session that has ended gets `end` at once: its transcript takes
  over. One that another Henk process runs gets `end` with `elsewhere:
  true`; its tool calls still show on the run's stream.

### `GET /runs/stream`

What runs now, for the overview: a `snapshot` (`RunningSnapshot`: the
newest running runs, at most 100, how many run, and the review `slots`)
on connect and every 30 seconds, a `run` message (`RunSummary`) whenever
a run of this process starts or ends, and a `progress` message (`run_id`
with its `sessions` or its `stages` as dots, the other `null`) whenever a
running run's sessions or stages move on (#225), and a `slots` message
(`Slots`) whenever a review of this process starts waiting, takes or
frees a slot, or leaves the queue. `RunningMessage` in
`types.ts` is the union.

`slots` has `limit` (`review.max_concurrent`), `in_use`, and `waiting`:
the reviews waiting for a slot, in the order they will start (#251). Each
has `run_id`, `kind` (`review`), `platform`, `repo`, `target`,
`target_url`, `commit`, `trigger`, `requester`, `since` (when it was
requested), `position` (from 1) and `reason` (`no_slot`: every slot is
taken; a review never waits behind another of the same pull request, as a
new commit supersedes it and the same commit joins it). A waiting review
has its run id but no run record until it starts; `POST
/runs/{id}/cancel` cancels it, and it then ends as cancelled while
queued. Every `slots` message holds the whole list, so a client replaces
what it had.

### `GET /stats/overview`

What happened per UTC day, for the overview's tiles (#225). `days` (1 to
90, default 14) counts today. The answer has `from`, `to` and `days`,
every day from `from` to `to` with:

- `runs`: runs started that day;
- `finished` and `failed`: of those, how many finished and how many
  failed. Cancelled, superseded and running runs are in neither, so
  `failed` of `finished + failed` is the share that did not complete;
- `findings_posted`: new comments, checked (`posted`) or not
  (`unverified`); improved comments are not new. Only reviews by lanes,
  before #309, posted any;
- `drafts`: drafts the review lanes wrote. A review loop's findings and a plan
  review's points are left out (#285, #328), so days since #309 have
  none.

### `GET /stats/sessions`

How each session of the recent reviews ended (#229): the review loop's
`reviewer` and `executor` (`fixer` before #323, in the executor's row),
and in reviews from before the loop (#309), the
lanes and the `check-N` fact-check sessions. It was `GET /stats/lanes`
before #325, which now answers 404.
`last` (1 to 200, default 30) asks for the newest reviews; `since` (RFC
3339) for every review started since then, the newest 1000 of them. Not
both. Only reviews that ended on their own count: running ones are left
out, and so are cancelled and superseded ones, whose sessions were
cancelled for them by a person or for a newer commit.

The answer has `reviews` (`run_id`, `started_at`), oldest first, and
`sessions`: the `reviewer` and the `executor` first, then the review lanes
by name, then the `check-N` sessions by number. Each has:

- `name`; `role`: `reviewer`, `executor`, `lane` or `check`; `models`: the models it
  ran, newest first;
- `outcomes`: one per review in `reviews`, in the same order: `run_id`,
  the `model` it ran there, `status` (`finished`, `timed_out`,
  `did_not_finish`) and `reason`, or `null` where it did not run;
- `ran`, `finished`, `timed_out`, `did_not_finish`: counts;
- `reasons`: why it timed out or did not finish, counted: `time_limit`,
  `rate_limit`, `provider_error`, `cancelled`, `declined` (the model
  declined) and `stuck` (it kept repeating a call). The reason is read
  from the error the session ended with.

### `GET /tool-calls/summary`

How each tool fared across runs (#203): one row per tool, model and
session `role` (`planner`, `reviewer`, `executor`, `address`, `lane`,
`check`; `executor` includes the `fixer` of runs before #323), the most
calls first. The role is the one stored with the session (#324): `lane`
and `check` (the fact-check) are sessions of reviews from before the loop
(#309), and `address` the session of address runs from before #327.

| Query | Meaning |
|---|---|
| `since`, `until` | Calls made at or after `since`, and before `until` (RFC 3339) |
| `tool`, `model` | Exactly |
| `role` | `planner`, `reviewer`, `executor`, `lane`, `check` or `address` |

Each row has `calls`, `errors` (the tool failed), `refusals` (the scope
guard or the repeat guard stopped it), `other` (never ran: an unknown tool,
malformed arguments, a cancel), `total_ms`, and `error_rate` and
`refusal_rate`, from 0 to 1.

```json
[{"tool": "read_file", "model": "claude-opus-5-5", "role": "reviewer",
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
| `tool`, `model`, `role`, `since`, `until` | As for the summary |
| `limit`, `cursor` | Paging |

```json
{"items": [{"run_id": "r-...", "repo": "o/r", "target": 70, "target_url": "...",
            "call": {"session": "reviewer", "turn": 12, "tool": "github__get_file_contents",
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

Starts a review, a plan or an execute run, exactly as the dashboard's start form does. The request becomes an event like a webhook, and the listeners
apply the allowlist and every refusal. Read the event to see what came of
it.

```json
{"kind": "review", "url": "https://github.com/o/r/pull/7", "commit": "abc1234"}
```

`kind` is `review`, `plan` or `execute`. `url` is a pull request for a
review and an issue for the others. `commit` (reviews) and `note` (plans
and execute runs) are optional.
Answers `202` with `{"event_id": "e-..."}`, or `400` saying what is wrong.

### `POST /runs/{id}/cancel`

Cancels a running run, as the dashboard's cancel button does. The body is
`{}`. Answers `202` with `{"run_id": "r-..."}`, or `409 conflict` when the
run is not running in this process. The request is recorded as an event
either way.
