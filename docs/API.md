# Dashboard API

What the dashboard shows and does, as JSON, at `/dashboard/api/v1` (#198).
The single-page dashboard (#197) is its client. It exists only when the
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
| `forbidden` | 403 | Id not on the list; an action without its token or from another origin |
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

Who is signed in, and the CSRF token their actions send.

```json
{"github_id": 1234, "login": "alice", "csrf": "q3Jx..."}
```

### `GET /health`

What `/dashboard/health` shows: configuration and the database, without
calling a model or an MCP server (that is `henk doctor --probe`).

```json
{"checks": [{"name": "database", "state": "ok", "detail": "SQLite at henk.db"}]}
```

### `GET /runs`

Runs, newest first. Every filter is optional:

| Query | Meaning |
|---|---|
| `kind` | `review`, `plan`, `address`, `discord_turn` or `mail_reply` |
| `status` | `running`, `finished`, `failed` or `cancelled` |
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
    "finished_at": "2026-10-06T08:41:02.1Z"
  }],
  "next": "MjAyNi0xMC0wNlQwODoyNzoxMS42Mjk4NjA4NDJafHItMjAyNjEwMDYtMWEyYjNjNGQ"
}
```

### `GET /runs/{id}`

One run with everything the run record holds about it:

- `run`: the summary above;
- `summary`, `error`, `check_id`, `heartbeat_at`;
- `lanes`: every session (lanes, checks, planner, address) with turns and
  tokens;
- `findings`: what was done on the platform;
- `drafts`: each draft and its `decision` (#189);
- `transcripts`: the sessions whose conversation is kept (#191);
- `tool_usage`: calls per session and tool (#190);
- `events`: the timeline;
- `requests`: the inbound events that started or joined it.

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
