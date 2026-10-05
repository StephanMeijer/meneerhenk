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
Henk's own (`post_finding`, `improve_finding`, `write_plan`, ...) whose
implementations enforce the rules of the spec in code.

GitHub writes go through REST rather than the MCP server's write tools on
purpose: that server's pending-review model is a per-user singleton, which
is unsafe when several lanes post findings as soon as they are sure.

Models are reached through two hand-written adapters behind one trait: the
OpenAI-compatible chat API (the local proxy, OpenAI, Ollama) and the
Anthropic Messages API. Lanes and their models are configuration.

### Crates

| Crate | Purpose |
|---|---|
| `henk-domain` | The spec's vocabulary and rules, no I/O: identities and standing, allowlist, findings, markers, scope guard, review outcomes, plan sections, queue decisions, style rules |
| `henk-llm` | `ModelClient` with the OpenAI-compatible and Anthropic adapters, retries, schema cleaning |
| `henk-mcp` | MCP sessions over stdio child processes or streamable HTTP, tool-name mapping, an in-process fake server for tests |
| `henk-agent` | The tool-calling loop with turn limit, deadline and cancellation; prompts |
| `henk-session` | One way to run a model session: `SessionSpec`, `run_session` with run bookkeeping, scope-guarded platform tools |
| `henk-events` | The event model, the webhook parsers, the bus that delivers events to listeners, and the local record of both |
| `henk-platform` | Webhook verification and parsing, GitHub App auth and API, the GitHub and GitLab writers for pull requests and issues |
| `henk-store` | SQLite run records: runs, lanes, findings, events |
| `henk` | The binary: configuration, hooks, listeners, review and planning orchestration, coordinator, HTTP server, CLI |

## Build and test

Requires Rust 1.93 (pinned in `rust-toolchain.toml`).

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
| `GITLAB_PERSONAL_ACCESS_TOKEN` | Both GitLab MCP sessions |
| `HENK_GITHUB_WEBHOOK_SECRET` | `POST /webhooks/github` |
| `HENK_GITLAB_WEBHOOK_TOKEN` | `POST /webhooks/gitlab` |
| `HENK_API_TOKEN` | `POST /review`, `POST /plan` |
| `RUST_LOG`, `HENK_LOG_JSON=1` | Logging |
| `HENK_TRANSCRIPT_DIR` | When set, every model session writes its full transcript as JSON under this directory. Local diagnostics only; nothing reads it back or sends it anywhere |

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

## Run

```sh
henk serve                                        # webhooks, API, run pages
henk review https://github.com/owner/repo/pull/7  # one review, now
henk review https://gitlab.example/group/project/-/merge_requests/5
henk plan https://github.com/owner/repo/issues/9 --note "keep it small"
henk llm probe --model proxy-fast                  # one prompt to a model
henk llm models --model proxy-fast                 # what that endpoint serves
henk mcp probe --server github --show pull_request_read
henk runs show r-20261005-1a2b3c4d                 # a run from the local database
```

In serve mode everything is an event. Hooks receive and publish; listeners
decide. GitHub sends `pull_request`, `issue_comment` and
`pull_request_review_comment`; GitLab sends merge request and note hooks.
Point them at `/webhooks/github` and `/webhooks/gitlab` behind a reverse
proxy that terminates TLS. `POST /review {"url", "commit"?}` and
`POST /plan {"url", "note"?}` with `Authorization: Bearer $HENK_API_TOKEN`
publish events too and answer `202 {"event": id}`.

Every inbound event is recorded in Henk's own SQLite with what each listener
did with it, and the payload as received (up to 256 KB). Recordings stay in
the service; nothing is sent anywhere. `GET /events/{id}` shows an event and
its outcomes, `GET /runs/{id}` a run and the events that led to it. Pruning
old recordings is a follow-up.

## Decisions taken for this version

These answer open questions of the spec provisionally; each becomes a
line in `docs/SPEC.md` once the team confirms it.

- §3.1 GitHub drafts are not reviewed until marked ready (`review.github_drafts`).
- §3.1 One review per pull/merge request at a time: the same commit joins
  the running review, a newer commit supersedes it.
- §3.2 When two lanes claim the same line, the first claim posts.
- §3.3 Dropped lanes are named by their configured lane name, never a model.
- §3.3 Findings in resolved threads do not count towards N.
- §4 A plan has 20 minutes (`planning.timeout_secs`).
- §4 On GitLab, triage sets labels, type and links; parent and child
  relations and project fields are not set in this version.
- Everywhere: the only rate limit is `review.max_concurrent`.
