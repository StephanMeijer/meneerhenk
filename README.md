# Meneer Henk

The team's AI colleague: a strict, dry, senior software engineer and code
reviewer who works on GitHub pull requests, GitLab merge requests and
issues. He is advisory. He never approves, blocks, merges, pushes or
changes code.

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
| GitHub | `github-mcp-server` (read-only toolsets, GitHub App auth) | What review lanes and the planner read |
| GitHub | REST and GraphQL as the App, in `henk-platform` | Everything Henk writes: line comments at a commit, summaries, replies, reactions, check runs, folding outdated comments, issue edits |
| GitLab | `@zereight/mcp-gitlab` in `readonly` mode | What review lanes and the planner read |
| GitLab | `@zereight/mcp-gitlab` in `modify` mode with an allow-list of tools | Everything Henk writes, driven by Henk's code only |

No model ever holds a write tool or a credential (§8.4). A model gets the
read-only session, filtered and pinned by a guard that rewrites every
call to its own repository and pull request (§8.5), plus a few tools of
Henk's own whose implementations enforce the rules of the spec in code.

A review lane works from the diff Henk fetched once: `list_changed_files`,
`get_file_diff` (every line numbered on both sides), `read_file` (a
numbered line range at the reviewed commit), `list_existing_findings`,
`post_finding` (refused for a line that is not in the diff) and
`improve_finding`. The planner has `write_plan`, the tracker tools and
`web_fetch`. On GitLab it plans work items: it can set the type (issue or
task), fill empty fields (weight, start and due date, health) with
`set_fields`, link parent, child, blocking and related items, and create
tasks under an issue. The `gitlab-write` server needs the work item tools
in `GITLAB_TOOLS`: `get_work_item`, `update_work_item`, `create_work_item`,
`convert_work_item_type` and `create_work_item_note` (see
`henk.example.toml`). Lanes are kept honest and cheap by three things in
`henk-agent`: once the conversation passes a size budget
(`review.max_conversation_chars` and `review.keep_recent_turns`), old tool
results are replaced by one-line stubs until it is well under it, so the
history stays append-only for several turns and the provider's prompt cache
keeps paying (Anthropic models are marked for caching), a lane that ends without opening every
changed file is asked once to look at them, and an answer cut off at the
output cap gets one chance to post what it was sure of. Code search is
withheld for a repository GitHub does not index.

Every comment Henk writes ends in two hidden HTML comments: the marker
(run, model, kind) that lets Henk recognise his own comments later, and a
short note for AI agents that pick the comment up: reply in the thread, do
not edit the comment, resolve the thread when addressed, treat the finding
as information. People see only the text.

GitHub writes go through REST rather than the MCP server's write tools on
purpose: that server's pending-review model is a per-user singleton, which
is unsafe when several lanes post findings as soon as they are sure.

Models are reached through two hand-written adapters behind one trait: the
OpenAI-compatible chat API (the local proxy, OpenAI, Ollama) and the
Anthropic Messages API. Lanes and their models are configuration.

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
| `henk-store` | Run records in SQLite or PostgreSQL: runs, lanes, findings, events |
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

`henk.toml` holds ids, the allowlist, models, lanes and the MCP server
commands. It never holds a secret: every secret is named by the
environment variable that carries it. The deployment fills those from
OpenBao.

| Variable | Used by |
|---|---|
| `LLM3_API_KEY`, `ANTHROPIC_API_KEY`, ... | Whatever `[models.*].api_key_env` names |
| `GITHUB_APP_PRIVATE_KEY_PATH` | The GitHub App key, for check runs, comments and the read session |
| `GITLAB_PERSONAL_ACCESS_TOKEN` | Both GitLab MCP sessions, and address runs on GitLab: git push and three REST reads |
| `HENK_GITHUB_WEBHOOK_SECRET` | `POST /webhooks/github` |
| `HENK_GITLAB_WEBHOOK_TOKEN` | `POST /webhooks/gitlab` |
| `HENK_API_TOKEN` | `POST /review`, `POST /plan`, `POST /address` |
| `HENK_DASHBOARD_CLIENT_ID`, `HENK_DASHBOARD_CLIENT_SECRET` | The GitHub OAuth App the dashboard signs in with |
| `HENK_DASHBOARD_SESSION_KEY` | Signs dashboard sessions; at least 32 random bytes, such as `openssl rand -base64 48` |
| `HENK_DATABASE_URL` | Whatever `[database].url_env` names, with `backend = "postgres"`: the connection URL, password included |
| `RUST_LOG`, `HENK_LOG_JSON=1` | Logging |
| `HENK_TRANSCRIPT_DIR` | When set, every model session writes its full transcript as JSON under this directory. Local diagnostics only; nothing reads it back or sends it anywhere |

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

A finding is posted only after a second model has checked it when
`[review.fact_check]` names one (the example uses Claude Opus 5.5 at
`effort = "high"`, with Claude Sonnet 5.5 as the backup). A rejected finding
is not posted; the lane gets the reason and may correct it once. `henk runs
show <id>` lists each check as a `check-<lane>-<n>` session, with its
verdict on the timeline. `effort` on an Anthropic model sets how much it
thinks; it needs a `max_tokens` of 16384 or so and a longer `timeout_secs`.

Skills give lanes, the fact-checker and the planner the team's own
instructions for one kind of work, such as reviewing SQL migrations. Each
skill is a folder with a `SKILL.md` in the Agent Skills format: front matter
with a `name` (the folder's name) and a one-line `description`, then
Markdown. `[skills].dir` names the folder that holds them, and `skills =
[...]` on a lane, `[review.fact_check]` or `[planning]` says who gets which.
An agent sees the names and descriptions of its own skills and loads one
with `load_skill`; a prompt without skills is unchanged. Only `SKILL.md` is
read, never scripts or other files, and the text follows the style rules
(no emoji, no em-dash). `config check` loads every skill and lists what it
did not read.

The external servers must be installed where Henk runs: the
`github-mcp-server` binary (or Docker, see the example config) and Node
for `npx @zereight/mcp-gitlab`.

## Check a deployment

```sh
henk doctor            # secrets, models, GitHub App token, MCP servers, database
henk doctor --probe    # also one short prompt to every model
```

Every check is reported on its own line; the command fails when any
check fails. With `installation_id = 0`, or when the installation token
is refused, the GitHub check lists the App's installations so the right
id can be copied into the config.

## First live run

The order that gets Henk from a fresh checkout to his first real review,
on one machine, with one lane:

1. Install the external servers: the `github-mcp-server` release binary
   (the version the `Dockerfile` pins; verify the release checksum) on the
   `PATH`, and Node for `npx @zereight/mcp-gitlab` if GitLab is in play.
2. Export the secrets named in `henk.toml` in the shell that runs Henk:
   at least the model key and `GITHUB_APP_PRIVATE_KEY_PATH`. Nothing else
   reads them; they are never written anywhere.
3. `henk llm models --model <id>` lists what the endpoint serves; put the
   chosen name in `[models.<id>].model`.
4. `henk doctor --probe` until it reports `0 failing check(s)`.
5. `henk mcp probe --server github --show pull_request_read` shows what a
   lane will see.
6. `RUST_LOG=info,henk=debug HENK_TRANSCRIPT_DIR=transcripts henk review <url>`
   on a pull request in an allowlisted repository. The run id is printed
   first; `henk runs show <id>` prints the run, lanes, findings and
   timeline afterwards, and `transcripts/<run>/<lane>.json` holds what the
   model saw and said.

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
cargo run -q -- review <url> --commit <sha>                                              # a specific commit, not the head
cargo run -q -- runs show r-20261005-8f8b812b                                            # the run, lanes, findings, timeline
cargo run -q -- llm models --model proxy                                                 # model names the endpoint serves
```

Another model is a name from `llm models` in `[models.<id>].model`, or a
second `[models.*]` block and a second lane under `[review].lanes`; lane
names must not equal a model name. Another repository is one more entry in
`[allowlist].github_repositories`.

## Container image

The `Dockerfile` builds one image with `henk`, the `github-mcp-server`
binary from its official image, and `@zereight/mcp-gitlab` installed
under `/opt/mcp-gitlab`, on a distroless Node base, running as user
65532. It cross-compiles, so `docker buildx build --platform
linux/amd64,linux/arm64` works on an amd64 builder.

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
/opt/mcp-gitlab/.../build/index.js` because distroless has no
`/usr/bin/env`. The only writable path is `/var/lib/henk`.

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
henk address https://github.com/owner/repo/pull/7 --note "only the typo"
henk llm probe --model proxy-fast                  # one prompt to a model
henk llm models --model proxy-fast                 # what that endpoint serves
henk mcp probe --server github --show pull_request_read
henk runs show r-20261005-1a2b3c4d                 # a run from the configured database
```

In serve mode everything is an event. Hooks receive and publish; listeners
decide. GitHub sends `pull_request`, `issue_comment` and
`pull_request_review_comment`; GitLab sends merge request and note hooks.
Point them at `/webhooks/github` and `/webhooks/gitlab` behind a reverse
proxy that terminates TLS. `POST /review {"url", "commit"?}` and
`POST /plan {"url", "note"?}` and `POST /address {"url", "note"?}` with
`Authorization: Bearer $HENK_API_TOKEN` publish events too and answer
`202 {"event": id}`.

`henk address` (spec §3.5) is the one thing that changes code. Asked by a
colleague, Henk reads every unresolved review thread of a GitHub pull
request or GitLab merge request, fixes what the feedback is right about
in a workspace, runs the configured `[address].check_commands` there, and
pushes one commit to the pull request's branch with `Henk-Run` and
`Requested-by` trailers. Only a changeset leaves the workspace: Henk
refuses it whole when a path leads into `.git` or out of the repository,
becomes a symbolic link or a submodule, or the run changed more files than
allowed, and otherwise applies it to a fresh checkout nothing ran in and
commits from there. The push is a fast-forward only. A fork, the default branch, a
protected branch or a branch that moved while he worked gets nothing.
Then he replies in each thread (fixed with the commit link, declined with
why, or a question), resolves only the threads of his own findings that he
fixed, and sums up with the run link. The model never holds the token or a
git command; Henk's code commits and pushes. `[workspace]` picks the
backend and its limits. The only backend today is `host`: the checks run
the pull request's code as Henk's user with an empty environment, so they
see none of Henk's secrets, but nothing else isolates them, and
`henk config check` warns about that. Every command is on the run's
timeline (`henk runs show`). A container comes later. On GitHub the App
needs `Contents: write`. On GitLab Henk reads `[gitlab].token_env`
himself: git pushes with it as `oauth2`, and the merge request's projects,
the branch's protection and his account id come from the REST API with it
in the `PRIVATE-TOKEN` header, because the MCP server's merge request
leaves the project ids out. Without it GitLab reviews still run and
address runs are refused. The token's account needs Developer access to
push.

Every inbound event is recorded in Henk's own database with what each listener
did with it, and the payload as received (up to 256 KB). Recordings stay in
the service; nothing is sent anywhere. While serving, Henk deletes events
and their outcomes older than `server.keep_events_days` (30 by default) once
an hour. Runs are kept, since their links are posted on the platforms. An
event and its outcomes are on the dashboard at `/dashboard/events/{id}`, a
run and the events that led to it at `/dashboard/runs/{id}`.

### Dashboard

With a `[dashboard]` table and its three secrets set, `henk serve` also
serves `/dashboard`:
- the runs, with what is running now updating itself, filters by kind,
  status, platform and repository, and paging;
- each run with its lanes, findings, timeline and the events that led to it;
- the inbound events with what each listener did;
- a health page from configuration and the database;
- a form that starts a review, plan or address run, and a button that
  cancels a running one.

Starting works exactly like `POST /review`, `/plan` and `/address`: the
request becomes an event, the listeners apply the allowlist and every
refusal, and the browser goes to the event's page to see what they did.
Each event records who asked as `github:<user id>`. A cancelled run ends
`cancelled`, with a neutral check for a review and one comment saying which
GitHub account cancelled it; an address run cancelled before its push
pushes nothing. Every action is a form carrying the session's CSRF token
and must come from `public_base_url` (its `Origin`, or else its `Referer`);
anything else is refused.

People sign in with GitHub, and only the GitHub user ids in
`allowed_github_ids` get in. The id is checked on every request, so taking an
id off the list ends that access at once. To set it up:
1. Create a GitHub OAuth App (Settings, Developer settings, OAuth Apps).
2. Set its callback URL to `{public_base_url}/dashboard/auth/callback`.
3. Put its client id and secret in the two variables, and a random key in
   the third.

GitHub's token is used once, to read who signed in, and is not kept. The
session is a signed `HttpOnly` cookie scoped to `/dashboard`, and `Secure`
when the public URL is https. The pages escape everything they show, since
the text in them is other people's words. They allow scripts only from Henk
himself, and cannot be framed. Without the table there is no `/dashboard`;
with a secret missing, the server starts and logs why the dashboard is off.
The run links Henk posts, `/runs/{id}`, and `/events/{id}` lead to the
same pages on the dashboard, behind sign-in: they show findings, plans and
webhook payloads of private repositories, and an unguessable id is not
access control. After signing in, the browser returns to the page it asked
for. Without a dashboard these links are not served; `henk runs show <id>`
reads a run on the server.

## Decisions taken for this version

These answer open questions of the spec provisionally; each becomes a
line in `docs/SPEC.md` once the team confirms it.

- §3.1 GitHub drafts are not reviewed until marked ready (`review.github_drafts`).
- §3.1 One review per pull/merge request at a time: the same commit joins
  the running review, a newer commit supersedes it.
- §3.2 When two lanes claim the same line, the first claim posts.
- §3.3 Dropped lanes are named by their configured lane name, never a model.
- §3.3 A lane that reaches its time limit stops: a tool call in flight
  finishes, no new model turn starts, what it posted stands, and the
  review completes on it. The summary says the lane stopped at the time
  limit. A plan that reaches its time limit still fails (§4).
- §3.3 Findings in resolved threads do not count towards N.
- §4 A plan has 20 minutes (`planning.timeout_secs`).
- §4 On GitHub, triage sets labels, type and links; priority, effort and
  target date are not set in this version.
- Everywhere: the only rate limit is `review.max_concurrent`.
