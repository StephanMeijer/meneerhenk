# Meneer Henk

The team's AI colleague: a strict, dry, senior software engineer and code
reviewer who works on GitHub pull requests, GitLab merge requests and
issues. He is advisory. He never approves, blocks or merges. He changes
code only on a pull request's own branch: the fixes of his review loop.

What Henk does and promises is in [docs/SPEC.md](docs/SPEC.md). This
repository is how he is built. This version covers code review (§3) and
issue planning (§4) on GitHub and GitLab. Discord (§5), email (§6) and
voice (§5.5) are later passes.

## How it is built

[ARCHITECTURE.md](ARCHITECTURE.md) has the diagrams: system context, crates, a review end to end, where a tool call goes, trust boundaries and deployment.

Henk's process is an MCP *client*. The platforms are reached through
external MCP servers that are not part of this codebase and run as child
processes over stdio:

| Platform | Server | Used for |
|---|---|---|
| GitHub | `github-mcp-server` (read-only toolsets, GitHub App auth) | What the planner reads |
| GitHub | REST and GraphQL as the App, in `henk-platform` | Everything Henk writes: line comments at a commit, summaries, replies, reactions, check runs, folding outdated comments, issue edits |
| GitLab | `@zereight/mcp-gitlab` in `readonly` mode | What the planner reads |
| GitLab | `@zereight/mcp-gitlab` in `modify` mode with an allow-list of tools | Everything Henk writes, driven by Henk's code only |

No model ever holds a platform write tool or a credential (§8.4). The
planner gets the read-only session, filtered and pinned by a guard that
rewrites every call to its own repository (§8.5). Every model gets tools
of Henk's own whose implementations enforce the rules of the spec in code.

A review is a loop of two sessions (#284, #309), a reviewer and an executor,
taking turns in one workspace at the pull request's head. The reviewer
works from the diff Henk fetched once (`list_changed_files`,
`get_file_diff`, every line numbered on both sides) and the workspace
(`list_files`, `search`, `read_file`, and `bash` on a backend apart from
Henk). It hands each finding to the executor with `report_finding` and ends
its round with `finish_round`; nothing is posted on the pull request. The
executor reads them (`list_findings`), checks each against the code, fixes
what holds with its edit tools (`edit_file`, `write_file`, `run_checks`,
and `bash` on a backend apart from Henk) and gives each its verdict with
`give_verdict`: fixed, rejected (it does not hold) or won't fix. Henk runs
`[roles.executor].check_commands`, checks the changeset (see "Pushing a
review's fixes") and pushes it as one commit to the pull request's branch. The
next round each resumes its own conversation: the reviewer sees the commit
and the verdicts, may contest a rejection once with new evidence, and
reviews again. The loop stops when the reviewer finds nothing more, at
`max_rounds` or its time limit, when the checks fail or a push is refused,
when the reviewer reports again what was fixed, or when a session breaks
off. A finding without a verdict by then ends unsettled.

A pull request the loop may not push to (a fork, the default or a
protected branch) or whose branch moved after the review was queued gets
the reviewer alone (#307): one round at the reviewed commit, nothing
pushed. The record of a review is its run: the run page and `henk runs
show` list the rounds, every finding with its verdict and the commits
pushed, and the check's summary carries the loop's report.

Planning is a pairing (#328): the planner drafts the plan and its tracker
changes, the reviewer reviews the draft with the platform's read tools and
the planner's copy of the repository, and raises each problem with
`raise_point`; the planner answers each point with `answer_point` (taken or
rejected) and revises. Nothing reaches the issue until the reviewer raises
nothing more, or `[planning] max_rounds` (3) or `run_timeout_secs` (3600)
ends it; then Henk makes the drafted changes and writes the plan, and a
review a limit ended leaves a "Points still open after review" section
under the plan. The run page and `henk runs show` list each point with the
planner's answer and why the review stopped.

The planner has `write_plan`, the tracker tools and `web_fetch`, all of
which draft until the review is done; `withdraw_change` takes a drafted
change out again, and the change budget counts the draft. On GitLab
it plans work items: it can set the type (issue or task), fill empty fields
(weight, start and due date, health) with `set_fields`, link parent, child,
blocking and related items, and create tasks under an issue. The
`gitlab-write` server needs the work item tools in `GITLAB_TOOLS`:
`get_work_item`, `update_work_item`, `create_work_item`,
`convert_work_item_type` and `create_work_item_note` (see
`henk.example.toml`). Its code search is withheld for a repository GitHub
does not index.

The loop's sessions are kept honest and cheap by three things: once the
conversation passes a size budget (`review.max_conversation_chars` and
`review.keep_recent_turns`), old tool results are replaced by one-line
stubs until it is well under it, so the history stays append-only for
several turns and the provider's prompt cache keeps paying (Anthropic
models are marked for caching); a reviewer that ends its turn without
finishing its round, or is cut off at the output cap, and an executor that
leaves a finding without a verdict, are asked again, at most twice a
round; and a session that repeats one tool call with the same arguments
is refused and then stopped as stuck (`agent.max_repeated_calls`, for
every session, recorded on the run).

In a pairing (the reviewer and the executor of a review, the planner and
the reviewer of a plan) each role keeps its conversation for the whole
pairing and is resumed with only an update, so every resumed turn reads its
history from the prompt cache (#330). On Anthropic models a role whose
partner's round may outlast the 5-minute cache (`timeout_secs` over 300,
600 by default) asks for the 1-hour cache, so it does not expire while the
role waits; a session on its own, such as an execute run, keeps the 5-minute
one. A compaction rewrites that history and writes the cache again, so each
one is a warning on the run, naming the session and the round. The run
page and `henk runs show` give every session's cache reads and writes and
the share of its tokens in the cache served (`input_tokens` stays the whole
prompt).

Every comment Henk writes ends in two hidden HTML comments: the marker
(run, model, kind) that lets Henk recognise his own comments later, and a
short note for AI agents that pick the comment up: reply in the thread, do
not edit the comment, resolve the thread when addressed, treat the finding
as information. People see only the text.

GitHub writes go through REST rather than the MCP server's write tools on
purpose: that server's pending-review model is a per-user singleton, which
is unsafe when several runs write at once.

Models are reached through two hand-written adapters behind one trait: the
OpenAI-compatible chat API (the local proxy, OpenAI, Ollama) and the
Anthropic Messages API. Which model reviews and which fixes is
configuration.

### Crates

| Crate | Purpose |
|---|---|
| `henk-domain` | The spec's vocabulary and rules, no I/O: identities and standing, allowlist, findings, markers, scope guard, review outcomes, plan sections, queue decisions, style rules, skills |
| `henk-llm` | `ModelClient` with the OpenAI-compatible and Anthropic adapters, retries, schema cleaning |
| `henk-mcp` | MCP sessions over stdio child processes or streamable HTTP, tool-name mapping, an in-process fake server for tests |
| `henk-agent` | The tool-calling loop with turn limit, deadline and cancellation; prompts |
| `henk-session` | One way to run a model session: `SessionSpec`, `run_session` with run bookkeeping, scope-guarded platform tools |
| `henk-events` | The event model, the webhook parsers, the bus that delivers events to listeners, and the local record of both |
| `henk-platform` | Webhook verification and parsing, GitHub App auth and API, the GitHub and GitLab writers for pull requests and issues |
| `henk-store` | Run records in SQLite or PostgreSQL: runs, sessions, findings, events |
| `henk` | The binary: configuration, hooks, listeners, review and planning orchestration, coordinator, HTTP server, CLI |

## Build and test

Requires Rust 1.95 (pinned in `rust-toolchain.toml`).

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo deny check
```

## Configure

```sh
cargo run -- config example > henk.toml   # then edit
cargo run -- --config henk.toml config check
```

`henk.toml` holds ids, the allowlist, models, the review loop and the MCP
server commands. It never holds a secret: every secret is named by the
environment variable that carries it. The deployment fills those from
OpenBao.

| Variable | Used by |
|---|---|
| `LLM3_API_KEY`, `ANTHROPIC_API_KEY`, ... | Whatever `[models.*].api_key_env` names |
| `GITHUB_APP_PRIVATE_KEY_PATH` | The GitHub App key, for check runs, comments and the read session |
| `GITLAB_PERSONAL_ACCESS_TOKEN` | Both GitLab MCP sessions, and reviews on GitLab: git push and three REST reads |
| `HENK_GITHUB_WEBHOOK_SECRET` | `POST /webhooks/github` |
| `HENK_GITLAB_WEBHOOK_TOKEN` | `POST /webhooks/gitlab` |
| `HENK_API_TOKEN` | `POST /review`, `POST /plan` |
| `HENK_DASHBOARD_CLIENT_ID`, `HENK_DASHBOARD_CLIENT_SECRET` | The GitHub OAuth App the dashboard signs in with |
| `HENK_DASHBOARD_SESSION_KEY` | Signs dashboard sessions; at least 32 random bytes, such as `openssl rand -base64 48` |
| `HENK_DATABASE_URL` | Whatever `[database].url_env` names, with `backend = "postgres"`: the connection URL, password included |
| `RUST_LOG`, `HENK_LOG_JSON=1` | Logging |
| `HENK_TRANSCRIPT_DIR` | When set, every model session also writes its full transcript as JSON under this directory, a local copy of what is stored with the run |

Run records live in one SQLite file by default (`[database] backend =
"sqlite"`, `path`). With `backend = "postgres"` they go to a PostgreSQL
database instead; `url_env` names the variable holding the URL, for example
`postgres://henk:...@db.internal/henk?sslmode=verify-full`. TLS is rustls
with the platform's certificate store, and `sslmode` works as usual. Henk
creates and migrates the schema on start, safely when several processes
start at once. A new PostgreSQL database starts empty: earlier SQLite
records stay in their file, readable with `henk runs show` by pointing
`[database]` back at it. `server.database_path` still works for SQLite and
`config check` calls it deprecated.

Lockfiles and `CHANGELOG.md` are not reviewed; `[review].ignore` changes
the list (see `henk.example.toml`), and a change made only of such files is
reported as nothing to review.

Henk's agents are **roles** (#321), each configured once: `[roles.reviewer]`,
`[roles.executor]` (the agent that changes the code) and `[roles.planner]`.
A role names its `model` from `[models]` and has its own `max_turns` and
`timeout_secs` per turn (a round of the review loop or of the plan review;
40 turns and 600s by default, 80 and 1200s for the planner), its own
`skills`, and optionally `tools`: a list that narrows the role's tools
within what the role may ever have. Only the executor edits, the reviewer
and the executor each have their side of the handoff, only the planner
has the tracker tools, and the planner and the reviewer, who reviews plans
too, have the MCP read tools; left out, a role gets its defaults,
and only the planner reaches the web. `henk config check` lists each
role, refuses a tool the role may not have, and says which roles reach the
web.

A review needs `[review.loop]` with `[roles.reviewer]` and
`[roles.executor]`; without either role the configuration is refused.
`max_rounds` (50 by default) caps the rounds and `run_timeout_secs` (3600)
limits the whole loop: no round starts after it, and a round under way
gets only what is left. The executor's `check_commands`,
`max_changed_files`, identity and trailers apply to every commit the loop
pushes, and its `requester_id` is who those commits are requested by.
Planning needs `[planning]` with `[roles.planner]` and `[roles.reviewer]`;
execute runs need `[execution]` with `[roles.executor]`.
Without `[review.loop]`
reviews are off: `config check`
prints `Reviews:         off (no [review.loop])` and `henk doctor` fails its
`reviews` check. A configuration from before #309 that still has lanes or
a fact-check is refused; see "Migrating from lanes" below. `effort` on an
Anthropic model sets how much it thinks; it needs a `max_tokens` of 16384
or so and a longer `timeout_secs`.

Skills give a role the team's own instructions for one kind of work,
such as splitting a migration into steps. Each skill is a folder with a
`SKILL.md` in the Agent Skills format: front matter with a `name` (the
folder's name) and a one-line `description`, then Markdown. `[skills].dir`
names the folder that holds them, and `skills = [...]` on a `[roles.*]`
section says which that role gets. A role sees the names and
descriptions of its skills and loads one with `load_skill`; a prompt
without skills is unchanged. Only `SKILL.md` is read, never scripts or
other files, and the text follows the style rules (no emoji, no em-dash).
`config check` loads every skill and lists what it did not read.

The external servers must be installed where Henk runs: the
`github-mcp-server` binary (or Docker, see the example config) and Node
for `npx @zereight/mcp-gitlab`.

### Migrating from lanes

Before #309 a review ran lanes: several models each made one pass and
drafted findings, a second model checked the drafts (the fact-check), and
what it confirmed was posted on the pull request as comments. Lanes and
the fact-check are gone. A configuration that still has `[review].lanes`,
`[review].lane_timeout_secs`, `[review].lane_max_turns` or
`[review.fact_check]` fails `henk config check` with `<key> was removed: a
review is now the reviewer↔executor loop`. Before:

```toml
[review]
lanes = [
  { name = "lane-a", model = "proxy-fast" },
  { name = "lane-b", model = "claude" },
]
lane_timeout_secs = 900
lane_max_turns = 40
max_concurrent = 3

[review.fact_check]
model = "opus"
backup_model = "claude"
timeout_secs = 180
max_turns = 12
```

After:

```toml
[review]
max_concurrent = 3

[review.loop]
# max_rounds = 50, run_timeout_secs = 3600

[roles.reviewer]
model = "opus"
timeout_secs = 900   # was lane_timeout_secs
max_turns = 40       # was lane_max_turns

[roles.executor]
model = "claude"
requester_id = 523456789012345678
check_commands = [["cargo", "test", "--offline"]]
```

- `lanes` becomes `[roles.reviewer]` and `[roles.executor]`: one model
  reviews, one checks and fixes; they may be the same model.
- `lane_timeout_secs` becomes `timeout_secs` and `lane_max_turns` becomes
  `max_turns` on each role, now per round. Their defaults are the old
  ones, 600 and 40.
- `[review.fact_check]` goes with nothing in its place: the executor checks
  each finding against the code and rejects what does not hold. Skills on
  a lane or the fact-check go too; a role takes skills of its own.
- `[roles.executor]` is required, since the loop pushes: on
  GitHub the App needs `Contents: write`, on GitLab Henk needs
  `[gitlab].token_env`. Its `check_commands` decide whether a round's fixes
  are pushed.
- `[workspace] review = true` is no longer needed: every review works in a
  workspace.
- Nothing is posted on the pull request any more. The run page, `henk runs
  show` and the check's summary say what the loop found, fixed and pushed.

Runs from before #309 stay readable: the run page and `henk runs show`
still show their lanes, drafts, the fact-check's verdicts and the findings
they posted. Markers on comments Henk already posted are still recognised.
Address runs, removed in #327, stay readable the same way: their kind
`address`, their stages and their `address` session.

### Migrating to roles

Since #322 each agent is configured once, as a role. `[address]`, the
`reviewer`, `fixer`, `round_timeout_secs` and `round_max_turns` keys of
`[review.loop]`, and the `model`, `timeout_secs`, `max_turns` and `skills`
keys of `[planning]` are refused by `henk config check` with `<key> was
removed: it is <new key> now`. Before:

```toml
[review.loop]
reviewer = "opus"
fixer = "claude"
round_timeout_secs = 600
round_max_turns = 40

[planning]
model = "claude"
timeout_secs = 1200
max_turns = 80
requester_id = 523456789012345678
skills = ["issue-sizing"]

[address]
model = "claude"
timeout_secs = 1800
max_turns = 80
requester_id = 523456789012345678
check_commands = [["cargo", "test", "--offline"]]
[address.identity]
name = "Meneer Henk"
email = "henk@example.com"
```

After:

```toml
[review.loop]

[planning]
requester_id = 523456789012345678

[roles.reviewer]
model = "opus"
timeout_secs = 600
max_turns = 40

[roles.planner]
model = "claude"
timeout_secs = 1200
max_turns = 80
skills = ["issue-sizing"]

[roles.executor]
model = "claude"
timeout_secs = 600
max_turns = 40
requester_id = 523456789012345678
check_commands = [["cargo", "test", "--offline"]]
[roles.executor.identity]
name = "Meneer Henk"
email = "henk@example.com"
```

- `[review.loop]` keeps `max_rounds` and `run_timeout_secs`; `reviewer`
  becomes `[roles.reviewer].model` and `fixer` becomes
  `[roles.executor].model`. `round_timeout_secs` and `round_max_turns`
  become `timeout_secs` and `max_turns` on both roles, so the reviewer and
  the executor can now have different limits.
- `[planning]` keeps `change_budget`, `sub_issue_cap` and `requester_id`;
  its `model`, limits and `skills` move to `[roles.planner]`.
- `[address]` becomes `[roles.executor]`, with its `identity`, `trailers`
  and `repositories` tables, which now apply to the commits the review loop
  pushes. Address runs (`henk address`, `POST /address`, `start_address`)
  were removed in #327; `[address]` is still refused, pointing at
  `[roles.executor]`. The deprecated `check_timeout_secs` is gone:
  `[workspace] command_secs` is the limit for every check.
- `[review.loop]` without `[roles.reviewer]` and `[roles.executor]`, or
  `[planning]` without `[roles.planner]` and `[roles.reviewer]` (#328), is
  refused. The reviewer reviews plans as well as pull requests, with the
  same `[roles.reviewer]` settings.

## Check a deployment

```sh
henk doctor            # secrets, models, GitHub App token, MCP servers, database, git
henk doctor --probe    # also one short prompt to every model
```

Every check is reported on its own line; the command fails when any
check fails. With `installation_id = 0`, or when the installation token
is refused, the GitHub check lists the App's installations so the right
id can be copied into the config.

## First live run

The order that gets Henk from a fresh checkout to his first real review,
on one machine, with one review loop:

1. Install the external servers: the `github-mcp-server` release binary
   (the version the `Dockerfile` pins; verify the release checksum) on the
   `PATH`, and Node for `npx @zereight/mcp-gitlab` if GitLab is in play.
2. Configure `[review.loop]`, `[roles.reviewer]` and `[roles.executor]`
   (see "Configure"); the loop pushes its fixes, so on GitHub the App needs `Contents: write`.
3. Export the secrets named in `henk.toml` in the shell that runs Henk:
   at least the model key and `GITHUB_APP_PRIVATE_KEY_PATH`. Nothing else
   reads them; they are never written anywhere.
4. `henk llm models --model <id>` lists what the endpoint serves; put the
   chosen name in `[models.<id>].model`.
5. `henk doctor --probe` until it reports `0 failing check(s)`.
6. `henk mcp probe --server github --show pull_request_read` shows what the
   planner's read-only session will see.
7. `RUST_LOG=info,henk=debug henk review <url>` on a pull request in an
   allowlisted repository, one whose branch may take Henk's fixes (on a
   fork, the default or a protected branch the reviewer reviews alone and
   nothing is pushed). The run id is printed first; `henk runs show <id>`
   prints the run, its rounds, sessions, tool calls, transcripts, findings
   with their verdicts and timeline afterwards, and `henk runs show <id>
   --transcript reviewer` what the reviewer's model saw and said (`executor`
   for the executor's; `fixer` in runs before #323).

Every tool call of every session (the reviewer, the executor, the planner)
is on the run record (#190): the session, model, turn, tool
and where it comes from, how it ended (`ok`, `error`, refused by the scope
or the repeat guard, an unknown tool, malformed arguments, not run,
cancelled), how long it ran, the size of its result, and its arguments as
the model sent them, cut at `[agent] record_argument_bytes` (4096 by
default, 0 for none) with their full length kept. Arguments are code, paths
and commands from the repository, never a credential (§8.4). The calls are
kept with the run, like its sessions and findings. `henk runs show` and the
dashboard's run page count them per session and tool, each session logs one
`tool usage` line at info, and the `tool_calls` table answers questions
across runs.

Every session's whole conversation is stored with its run too (#191): the
system prompt, every message, each tool call with its arguments and what it
returned, kept whole. `henk runs show <id>` lists them by session, and
`henk runs show <id> --transcript <session>` prints one. On the dashboard,
each session on the run page links to its transcript at
`/dashboard/runs/{id}/transcripts/{session}`, behind the same sign-in. A
transcript holds what the model was shown: code, diffs, comments and
command output from the repository, never a credential (§8.4); a test
fetches with a known token and checks that no transcript or tool call holds
it. Transcripts are large, so they follow `server.keep_events_days` like
inbound events; the run, its sessions, findings and tool calls are kept.
`HENK_TRANSCRIPT_DIR` still writes a local copy of each as a file.

### Reviewing a pull request from a laptop

With the setup above done once, a review is three commands. The secrets
come from OpenBao at the moment they are needed: `.env` (gitignored) holds
only `bao kv get` calls, never a value, and `source .env` runs them in the
current shell.

```sh
cd /path/to/meneerhenk
source .env                      # secrets into this shell; the App key becomes a 0600 file under $XDG_RUNTIME_DIR
cargo run -q -- doctor           # optional; every line should say ok
cargo run -q -- review https://github.com/owner/repo/pull/7
```

An `.env` for this looks like:

```sh
export LLM3_API_KEY="$(bao kv get -field=proxy-token secret/<path to the proxy token>)"
umask 077
mkdir -p "${XDG_RUNTIME_DIR:-/tmp}/henk"
bao kv get -field=github_app_private_key secret/<path to the App secret> > "${XDG_RUNTIME_DIR:-/tmp}/henk/app.pem"
export GITHUB_APP_PRIVATE_KEY_PATH="${XDG_RUNTIME_DIR:-/tmp}/henk/app.pem"
```

The first line of `henk review`'s output is the run id. The repository must
be on the allowlist; the App must be installed on it (`doctor` lists the
installations when the token is refused). Variants:

```sh
RUST_LOG=info,henk=debug HENK_TRANSCRIPT_DIR=transcripts cargo run -q -- review <url>   # debug log and a local transcript
cargo run -q -- review <url> --commit <sha>                                              # a specific commit, not the head: the reviewer alone, nothing pushed
cargo run -q -- runs show r-20261005-8f8b812b                                            # the run, rounds, findings, timeline
cargo run -q -- runs show r-20261005-8f8b812b --transcript reviewer                      # what the reviewer's model saw and said
cargo run -q -- llm models --model proxy                                                 # model names the endpoint serves
```

Another model is a name from `llm models` in `[models.<id>].model`, or a
second `[models.*]` block named as the `model` of `[roles.reviewer]` or
`[roles.executor]`. Another repository is one more entry in
`[allowlist].github_repositories`.

## Container image

The `Dockerfile` builds one image with `henk`, the `github-mcp-server`
binary from its official image, and `@zereight/mcp-gitlab` installed
under `/opt/mcp-gitlab`, on Debian slim with `git` and CA certificates
(Henk runs `git` itself for checkouts), running as user 65532. It
cross-compiles `henk`, so `docker buildx build --platform
linux/amd64,linux/arm64` works on an amd64 builder; only the runtime
stage's `apt-get install` runs under emulation, which needs QEMU
(`docker run --privileged --rm tonistiigi/binfmt --install arm64`).

```sh
docker build -t henk .
docker run --rm --read-only --cap-drop ALL --security-opt no-new-privileges \
  --tmpfs /tmp -v henk-data:/var/lib/henk \
  -v ./henk.toml:/etc/henk/henk.toml:ro -v ./app.pem:/run/secrets/app.pem:ro \
  -e GITHUB_APP_PRIVATE_KEY_PATH=/run/secrets/app.pem -e LLM3_API_KEY -e ANTHROPIC_API_KEY \
  -e GITLAB_PERSONAL_ACCESS_TOKEN -e HENK_GITHUB_WEBHOOK_SECRET -e HENK_GITLAB_WEBHOOK_TOKEN -e HENK_API_TOKEN \
  -p 127.0.0.1:8080:8080 henk
```

`deploy/henk.container.example.toml` is the example config with the
container's paths: the GitLab servers are started as `node
/opt/mcp-gitlab/.../build/index.js` so they do not depend on the
package's shebang. The only writable path is `/var/lib/henk`.

Released images are on `ghcr.io/stephanmeijer/meneerhenk` (`:X.Y.Z`,
`:X.Y`, `:latest`), signed with cosign and carrying SLSA provenance; each
GitHub Release also has signed Linux binaries. `RELEASING.md` says how a
release is cut and how to verify one.

## Run

```sh
henk serve                                        # webhooks, API, run pages
henk review https://github.com/owner/repo/pull/7  # one review, now
henk review URL1 URL2 URL3                        # several: at most 50, max_concurrent at a time
henk review https://gitlab.example/group/project/-/merge_requests/5
henk plan https://github.com/owner/repo/issues/9 --note "keep it small"
henk execute https://github.com/owner/repo/issues/9   # issue to pull request, then its review
henk llm probe --model proxy-fast                  # one prompt to a model
henk llm models --model proxy-fast                 # what that endpoint serves
henk mcp probe --server github --show pull_request_read
henk mcp probe --server other --platform gitlab    # an alias [github] and [gitlab] do not name
henk runs show r-20261005-1a2b3c4d                 # a run from the configured database
henk runs show r-20261005-1a2b3c4d --transcript reviewer   # one session's conversation
```

In serve mode everything is an event. Hooks receive and publish; listeners
decide. GitHub sends `pull_request`, `issue_comment` and
`pull_request_review_comment`; GitLab sends merge request and note hooks.
Point them at `/webhooks/github` and `/webhooks/gitlab` behind a reverse
proxy that terminates TLS. `POST /review {"url", "commit"?}` and
`POST /plan {"url", "note"?}` and `POST /execute {"url", "note"?}` with
`Authorization: Bearer $HENK_API_TOKEN`
publish events too and answer `202 {"event": id}`.

Every inbound event is recorded in Henk's own database with what each listener
did with it, and the payload as received (up to 256 KB). Recordings stay in
the service; nothing is sent anywhere. While serving, Henk deletes events
and their outcomes older than `server.keep_events_days` (30 by default) once
an hour, and session transcripts of that age with them. Runs are kept, since
their links are posted on the platforms. An
event and its outcomes are on the dashboard at `/dashboard/events/{id}`, a
run and the events that led to it at `/dashboard/runs/{id}`.

### Taking an issue to a pull request

With `[execution]` configured, a colleague can ask Henk to take an open
issue to a pull request (spec §4a, #329): from the dashboard's start form,
MCP `start_execute`, `POST /execute` or `henk execute <issue-url>`. Nothing
starts one on its own. The executor works in a workspace on the default
branch's head, reads the issue and any plan in it, changes the code, runs
the checks and ends with `propose_pull_request`: what it changed and what
it left out. Henk then checks the changeset as for a review loop's round
(below), commits it as "Resolve #N: <title>" with the same identity and
trailers, pushes it to a new branch `henk/<issue>-<title>`, never over a
branch that exists, and opens a ready pull request whose description
closes the issue. Then he asks for its review, a run of its own: the
reviewer and the executor work on it as on any pull request. When the
checks fail, nothing changed or the branch exists, there is no pull
request and one comment on the issue says why, with the run link.

```toml
[execution]
run_timeout_secs = 3600   # the executor's time for one issue
branch_prefix = "henk/"   # every branch Henk makes starts with it
```

`[execution]` needs `[roles.executor]`, whose model, `max_turns`, checks,
`max_changed_files`, identity and trailers it uses. On GitHub the App also
needs `Pull requests: write`. On GitLab Henk opens the merge request
through the `gitlab-write` MCP server, so `create_merge_request` must be in
its `GITLAB_TOOLS`. The bot's own "opened" event is ignored as Henk's own;
the review starts because the execute run asks for it.

### Pushing a review's fixes

The review loop's executor (spec §3.5) is the only thing that changes code.
It works in the review's workspace, and after each round Henk runs the
configured `[roles.executor].check_commands` there and pushes one commit to
the pull request's branch. Only a changeset leaves the workspace: Henk
refuses it whole when a path leads into `.git` or out of the repository,
becomes a symbolic link or a submodule, or changes more files than
`max_changed_files` allows, and otherwise applies it to a fresh checkout
nothing ran in and commits from there; the executor's edit tools refuse to
write a file larger than 1 MB. The
push is a fast-forward only. A fork, the default branch, a protected
branch, a closed pull request or a branch that moved while he worked gets
nothing. Nothing is posted as comments: the commits and the check's summary
are the review's report. The model never holds the token or a git command;
Henk's code commits and pushes. Henk recognises his own pushes by commit,
and the run that pushed reviews them in its next round (#284).

`[workspace]` picks the backend and its limits, per repository through
profiles. `host` runs the checks as Henk's user with an empty environment,
so they see none of Henk's secrets, but nothing else isolates them. `ssh`
runs each workspace as a throwaway user on a sandbox host (see "Sandbox
host" below), and `kubernetes` as a Pod of its own (see "Workspaces on
Kubernetes"). `henk config check` warns about what each one does not
isolate. Every command is on the run's timeline (`henk runs show`). On
GitHub the App needs `Contents: write`. On GitLab Henk reads
`[gitlab].token_env` himself: git pushes with it as `oauth2`, and the merge
request's projects, the branch's protection and his account id come from
the REST API with it in the `PRIVATE-TOKEN` header, because the MCP
server's merge request leaves the project ids out. Without it GitLab
reviews fail. The token's account needs Developer access to push.

The commit's subject is "Fix review findings". It ends in a trailer block,
in this order: `Henk-Run` (the run), `Requested-by` (the requester's
Discord id), the requester's `Co-authored-by` and `Signed-off-by`, and
Henk's `Signed-off-by` last, as
the committer's. By default Henk signs off, so DCO checks pass, and the
requester is credited as co-author; the requester does not sign off.
`[roles.executor.trailers]` changes the defaults and
`[roles.executor.repositories."owner/name"]` changes them for one repository on
the allowlist. Turning on `requester_signoff` for a repository means the
requester certifies the Developer Certificate of Origin for a change they
asked for but did not type: enabling it is the operator's statement that
requesters in that repository agree to that.

Henk commits as the App, `meneer-henk[bot]` with its GitHub noreply
address, unless `[roles.executor.identity]` sets a `name` and `email`; that
identity is author, committer and Henk's sign-off alike. The requester is
credited by the `github_id` or `gitlab_id` in their `[[people]]` entry,
for the platform of the run: Henk reads that account's current login by id
and uses `<id>+<login>@users.noreply.github.com` on GitHub and
`<id>-<username>@users.noreply.<host>` on GitLab, with the host of
`[gitlab].api_url`, as for Henk's own address there.
`commit_name` and `commit_email` in the same entry override both. A
requester with neither gets no trailers of their own, the run notes why,
and `henk config check` warns about it. Trailer values come only from ids
and configuration, never from a display name or a comment (spec §2, §8.3);
`config check` refuses a malformed name or email.

### Sandbox host

The `ssh` workspace backend (#84) runs a workspace's commands on a
machine of their own. Henk copies the pull request's checkout there, `.git`
and all, as a new Unix user `henk-w…` with its own home; every command and
file operation of the run is done as that user, with an empty environment
and the profile's time and output limits; the changeset comes from a record
only root can write, never from the tree's `.git`, fed from a tar the user
makes of its tree once its processes are stopped, so root never reads a path
the user controls; afterwards the user, its processes and its files are
removed. Henk's key and the platform token never
reach the host (§8.4), and `henk serve` removes what a crashed process left
when it starts. Runs share the host's kernel, `/tmp` and network, and
memory, cpu, pids and disk are not limited: use a host that holds nothing
else, and one Henk per host.

Henk signs in as root and installs nothing there: every request carries
Henk's own script (`crates/henk/src/workspace/sandbox.sh`), run with
`sh -c`, and its arguments are words of `A-Z a-z 0-9 + / = _ . -` only, so
nothing a model wrote is ever evaluated by a shell. Root is Henk's, for
making and removing each run's user; the model's commands always run as
that user. Henk's key is therefore root on the host: give the host nothing
else to lose.

To set one up (Debian or alike):
1. Install the tools the script uses:
   `apt-get install openssh-server bash git tar procps findutils grep passwd util-linux`
   (`runuser`, `useradd`/`userdel`, `pkill`, and GNU `grep` with `-P` on a
   PCRE2 with Unicode support, which the `search` tool uses; any version
   does, 3.8 and later are checked).
2. Put Henk's public key in `/root/.ssh/authorized_keys`, and allow root
   to sign in with a key (`PermitRootLogin prohibit-password`, Debian's
   default). Henk only runs commands, so start the line with `restrict`
   (no pty, no port, agent or X11 forwarding) and `from=` with the address
   Henk connects from; a stolen key is then of no use anywhere else:
   `restrict,from="203.0.113.7" ssh-ed25519 AAAA… henk`

Then, in `henk.toml`, with the private key's path in the variable
`key_path_env` names and the host key pinned (`ssh-keyscan -t ed25519 host`
prints it; there is no trust on first use):

```toml
[workspace.profiles.sandbox]
backend = "ssh"
[workspace.repositories]
"docspec/app" = "sandbox"
[workspace.ssh]
host = "sandbox.example.com"
host_key = "ssh-ed25519 AAAA…"
# port = 22, user = "root", key_path_env = "HENK_SANDBOX_KEY_PATH"
```

A profile can prepare the workspace before the model starts (#93): with
`toolchain = "mise"`, Henk runs `mise install` in the checkout, so the
tools and versions the repository's own `mise.toml` or `.tool-versions`
names are there, and every command of the run (setup, the checks, the
model's) then runs through `mise exec`. `setup` lists commands of the
operator's to run after that, in order, such as a dependency fetch. The
steps come from `henk.toml` only; the repository's files are data mise
reads, never a step (§8.3). mise always runs in its safe mode
(`MISE_SAFE=1`): it reads the tool versions but runs nothing the file
defines (`exec()` templates, `_.source`, hooks, tasks, plugin scripts) and
ignores its `[env]`. Henk never runs `mise trust`, and refuses a mise too
old to have safe mode. Each step is on the run's timeline, and the first
that fails ends the run before the model starts, with nothing pushed. mise
installs into the run user's home, outside the tree, and the tree as setup
leaves it is where the change starts, so neither what mise fetches nor what
a setup step writes (a lockfile, `node_modules`) is part of the change. On
a sandbox host, install mise where the run's PATH finds it
(`/usr/local/bin/mise`); `henk doctor --probe` says when a profile there
needs it and it is missing.

```toml
[workspace.profiles.sandbox]
backend = "ssh"
toolchain = "mise"
setup = [["npm", "ci"]]
```

Every review works in a workspace (#284): the reviewer and the executor
share one, holding the pull request's head, set up as above before the
reviewer starts; a reviewer alone gets the reviewed commit, fetched by its
sha, so a push during the review changes nothing. The reviewer's changes
are never exported, and a reviewer that changes a file stops the loop; only
the executor's changes, checked as above, become a commit. In the
workspace both list files (`list_files`, with a glob), search them by
regular expression (`search`) and read any file by line range
(`read_file`), and on a backend apart from Henk run a command of their own
(`bash`, #85): one test, a build or a grep, as the workspace's user, with the repository's toolchain
and the profile's time and output limits. Each command is on the run's
timeline, so `henk runs show <id>` lists what a review ran. A review whose
workspace cannot be opened or set up fails before any session starts.

`review = true` on a profile once gave the lanes workspaces (#170). The
loop opens its workspace whether or not it is set, so the flag no longer
changes how a review runs; `config check` still lists it and refuses it on
the `host` backend, and the dashboard's health page counts how many recent
reviews under such a profile got their workspace.

With `plan = true` on a profile, the planner gets a workspace too (#172):
one copy of the repository at its default branch, fetched by the sha Henk
read, set up as above and never exported, with the same code tools and
`bash` and a prompt that asks it to ground the plan in what it read. The
platform's tools stay for issues, pull requests and history. The executor
gets `bash` beside `run_checks` on any backend apart from Henk: one test or
build of its own choosing while it works, where every file it
changes is part of the commit and the checks still decide. Neither is
available on the `host` backend, where those commands would run as Henk's
own user: `plan = true` is refused there, and the executor there keeps
`run_checks` only.
`henk doctor --probe` says when `grep -P` on the host is missing or does
not read Unicode as the `search` tool needs.

A review runs the profile's setup on code that anyone who can open a pull
request chose, from a fork too, and the executor's checks on what it pushes.
On the `host` backend that code runs as Henk's own user, next to his
configuration and keys, so give repositories that take pull requests from
people you do not trust a profile on a backend apart from Henk.

`henk doctor --probe` connects and reports the connection, the host key and
the tools the script needs on lines of their own.

Coming from `henk-runner` (earlier versions signed in as a `henk` user
whose key could only run that program): put Henk's key in
`/root/.ssh/authorized_keys` with the options of step 2 and drop
`user = "henk"` from `[workspace.ssh]`. `/usr/local/sbin/henk-runner`,
`/etc/sudoers.d/henk` and the `henk` user are no longer used and can go;
records an older Henk left are still swept. The live tests run against such a
host: `deploy/sandbox/test-host.Containerfile` builds one, and
`HENK_TEST_SSH_HOST`, `_PORT`, `_USER`, `_KEY_PATH` and `_HOST_KEY` point
`cargo test -p henk -- --ignored live_` at it.

### Workspaces on Kubernetes

The `kubernetes` workspace backend (#89) makes one Pod per workspace in a
sandbox namespace of its own, apart from the namespace Henk runs in. Henk
talks to the API server himself, as his own ServiceAccount, and may do
nothing but make, read and remove Pods and run commands in them, and only
in that namespace. Each request is a `pods/exec` of the same sandbox
script the `ssh` backend sends, in its single-user mode: the Pod is the
workspace and its boundary, the record of changes is in the Pod with
everything else, and what leaves it is the changeset Henk checks as for
every backend.

Every Pod is locked down by its spec: not root (`run_as_user`), no
privilege escalation, every capability dropped, a read-only root
filesystem, seccomp `RuntimeDefault`, no ServiceAccount token and no
service links, and the profile's `memory_mib`, `cpus` and `disk_mib` as its
memory, cpu and scratch disk. The number of processes is the node's
setting (`podPidsLimit`). A command that runs out of memory ends the whole
Pod on Kubernetes 1.32 and later; the result says so, and the workspace is
gone for the rest of that run. Pods live at most six hours whatever
happens to Henk, and `henk serve` removes those of a Henk that died when
it starts: one Henk per sandbox namespace.

To set it up, apply `deploy/kubernetes/`:
1. `henk-serviceaccount.yaml`: Henk's ServiceAccount, in Henk's own
   namespace (`henk` there; change it to yours). Henk's Deployment runs as
   it.
2. `sandbox-namespace.yaml`: the sandbox namespace with Pod Security
   `restricted` enforced, a Role for Pods and `pods/exec` bound to Henk's
   ServiceAccount, a default ServiceAccount that never mounts a token, and
   a quota and default limits for the namespace as a whole.
3. `sandbox-network.yaml`: a `NetworkPolicy` that lets nothing in and lets
   the Pods reach DNS and the internet but not the cluster: fill in your
   Pod, Service and node ranges. It needs a CNI that enforces policies.
4. An image with `sh`, coreutils, findutils, GNU `grep` with PCRE2, `tar`,
   `git`, `bash` and `catatonit` (the Pod's first process, which reaps
   what a run leaves running), and `mise` for profiles with
   `toolchain = "mise"`:
   `deploy/kubernetes/sandbox-image/Containerfile` is a start. Pin it by
   digest. A profile may name its own with `image`, so each repository can
   start from the toolchains it needs.

```toml
[workspace]
backend = "kubernetes"
[workspace.kubernetes]
namespace = "henk-sandbox"
image = "registry.example/henk-sandbox@sha256:…"
# run_as_user = 1000
# runtime_class = "gvisor"           # gVisor or Kata, #87
# node_selector = { pool = "sandbox" }
# kubeconfig_env = "KUBECONFIG"      # outside the cluster only
```

`henk doctor --probe` asks the API server what Henk may do in the
namespace (it fails when he could read Secrets there, or lacks what he
needs), and starts a Pod from each image a profile uses to check its
tools. The live tests run against any cluster, kind included: apply the
manifests, load the image, and set `HENK_TEST_KUBE_NAMESPACE`,
`_IMAGE` and `_SERVICEACCOUNT` (`henk:henk`, which the tests act as) for
`cargo test -p henk -- --ignored live_kube`.

### Dashboard

With a `[dashboard]` table and its three secrets set, `henk serve` also
serves `/dashboard`, a single-page app (Svelte, Vite and TypeScript, in
`dashboard/`) built into the binary:
- the runs, with what is running now updating itself, filters by kind,
  status, platform and repository, and paging;
- each run with its sessions, tool calls, the review loop's findings and
  the executor's verdicts, timeline and the events that led to it, and each
  session's whole conversation; a running run updates as it happens:
  sessions start and end, tool calls, findings and verdicts appear,
  without a reload. A review from before #309 still shows its lanes, their
  drafts with the fact-check's verdicts, and the findings it posted;
- the inbound events with what each listener did;
- tools: how each tool fared per model and kind of session, with error
  and refusal rates, the calls that went wrong with their arguments, and on
  each run every call by turn, linked to that turn of the conversation;
- sessions: how often the reviewer and the executor (and the lanes of
  reviews from before #309) finished, timed out or did not, and why;
- a health page from configuration and the database, including whether
  `git` runs where Henk does and how many recent reviews got the
  workspaces their profile asks for;
- a form that starts what this Henk is configured for: a review, a plan,
  or an issue taken to a pull request (#329); and a button that cancels a
  running one.

Starting works exactly like `POST /review`, `/plan` and `/execute`: the
request becomes an event, the listeners apply the allowlist and every
refusal, and the browser goes to the event's page to see what they did.
Each event records who asked as `github:<user id>`. A cancelled run ends
`cancelled`: a review with a neutral check saying which GitHub account
cancelled it, a plan or an execute run with one comment on the issue
saying so.

The app reads and acts only through the JSON API at `/dashboard/api/v1`,
behind the same sign-in; every action carries the session's CSRF token in a
header, comes from `public_base_url` (its `Origin`, or else its `Referer`)
and has a JSON body, and anything else is refused. `docs/API.md` lists the
endpoints, for scripts too.

People sign in with GitHub, and only the GitHub user ids in
`allowed_github_ids` get in. The id is checked on every request, so taking an
id off the list ends that access at once. To set it up:
1. Create a GitHub OAuth App (Settings, Developer settings, OAuth Apps).
   Not Henk's GitHub App: signing in through a GitHub App makes GitHub ask
   people to let it act on their behalf, while the dashboard only needs to
   know who they are. `henk doctor` and the server's log warn when the client
   id is a GitHub App's.
2. Set its callback URL to `{public_base_url}/dashboard/auth/callback`.
3. Put its client id and secret in the two variables, and a random key in
   the third.

The sign-in asks for no scope, and GitHub's token is used once, to read who
signed in, and is not kept. The
session is a signed `HttpOnly` cookie scoped to `/dashboard`, and `Secure`
when the public URL is https. The app shows everything as text, since the
text in it is other people's words; its pages allow no inline script or
style and nothing from another origin, and cannot be framed. Without the
table there is no `/dashboard`; with a secret missing, the server starts
and logs why the dashboard is off. The run links Henk posts, `/runs/{id}`,
and `/events/{id}` lead to the same pages on the dashboard, behind sign-in:
they show findings, plans and webhook payloads of private repositories, and
an unguessable id is not access control. After signing in, the browser
returns to the page it asked for. Without a dashboard these links are not
served; `henk runs show <id>` reads a run on the server.

The release binary and the container image embed the app. To build it into
a binary of your own, with the Node of `dashboard/.node-version`:

```sh
cd dashboard && npm ci --ignore-scripts && npm run build && cd ..
cargo build --release   # build.rs embeds dashboard/dist
```

A binary built without it serves a page at `/dashboard` saying so. To work
on the app, run Henk, sign in on Henk's own `/dashboard/login`, and start
Vite's dev server on the same host: it forwards the API and sign-in to
`HENK_URL` (default `http://127.0.0.1:8080`).

```sh
cd dashboard && HENK_URL=http://127.0.0.1:8080 npm run dev
```

### MCP server

With `[mcp_server] enabled = true` and at least one token, `henk serve` also
serves the Model Context Protocol at `/mcp` (Streamable HTTP), so an agent
such as Claude Code can work with Henk:
- `start_review`, `start_plan`, `start_execute`: start work, exactly as
  `POST /review`, `/plan` and `/execute` do. The request becomes an event,
  the listeners apply the allowlist and every refusal, and the tool answers
  with the run id and link, or why nothing started;
- `cancel_run`;
- `list_runs`, `get_run`, `get_run_events`, `list_tool_calls`,
  `get_transcript`, `get_event`: what the dashboard shows, as JSON;
- `health`.

Each client has its own token, named in the configuration and kept in an
environment variable; `scope = "read"` may only read, `"write"` may also
start and cancel. What a client starts or cancels is asked by
`mcp:<token name>`, on the event and the run, as the dashboard records
`github:<user id>`. A request without a valid token gets 401 before any MCP
handling; with the server off or no token set, `/mcp` answers 503. Requests
must name the public host or loopback.

```toml
[mcp_server]
enabled = true
[[mcp_server.tokens]]
name = "claude-code"
env = "HENK_MCP_TOKEN_CLAUDE_CODE"
scope = "write"
```

Connect Claude Code with:

```sh
claude mcp add --transport http henk https://henk.example.com/mcp \
  --header "Authorization: Bearer $HENK_MCP_TOKEN_CLAUDE_CODE"
```

`henk config check` lists the tokens and whether each variable is set;
`henk doctor --probe` connects to `{public_base_url}/mcp` with the first
token that is set and lists the tools. Behind a reverse proxy, route `/mcp`
to Henk like the rest; the server answers each request with JSON, and a
proxy that buffers responses does no harm, though leaving it unbuffered
keeps it ready for streaming.

### llms.txt

[`llms.txt`](llms.txt) says how to use a running Henk from outside, in the
[llmstxt.org](https://llmstxt.org) format, for people and agents alike: the
dashboard, its API, the MCP server, what to ask on a pull request, and the
older bearer-token endpoints. `henk serve` serves it at `/llms.txt` without
sign-in; without a `[dashboard]` it says that the dashboard sections do not
apply. A test fails when it names a route or MCP tool Henk does not have.

## Decisions taken for this version

These answer open questions of the spec provisionally; each becomes a
line in `docs/SPEC.md` once the team confirms it.

- §3.1 GitHub drafts are not reviewed until marked ready (`review.github_drafts`).
- §3.1 One review per pull/merge request at a time: the same commit joins
  the running review, a newer commit supersedes it.
- §3.2 A review is the reviewer and executor loop (#309): nothing is posted
  on the pull request, and the check's summary carries the loop's report.
- §3.3 A session that reaches its round's time limit, or a loop that
  reaches `run_timeout_secs`, stops the loop: what was pushed stands,
  every finding still open ends unsettled, and the check says why the loop
  stopped. A plan whose planner reaches its time limit before it drafted a
  plan still fails (§4); a plan review a limit ends publishes the draft
  with what is still open.
- §3.3 A review a restart interrupted, running or waiting for a slot, or
  closed as orphaned after its process died, is reviewed again after the
  next start, on the pull request's current head, once per pull request
  (#160). Henk looks back 24 hours, a fixed window. The resumed run's
  trigger names the interrupted run; a closed pull request or one off the
  allowlist is not resumed. A resumed review closed as orphaned is not
  resumed again, so a review that takes Henk down cannot loop. Interrupted
  plans are not resumed: ask again.
- §4 A plan has an hour in all (`planning.run_timeout_secs`), and each
  turn the role's own `timeout_secs`; its review has at most 3 rounds
  (`planning.max_rounds`).
- §4 On GitHub, triage sets labels, type and links; priority, effort and
  target date are not set in this version.
- Everywhere: the only rate limit is `review.max_concurrent`. On GitHub, a
  review waiting for a slot shows its check as *queued* until it starts
  (§3.3).
