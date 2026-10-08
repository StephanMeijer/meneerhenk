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
`henk.example.toml`). Lanes are kept honest and cheap by four things in
`henk-agent`: once the conversation passes a size budget
(`review.max_conversation_chars` and `review.keep_recent_turns`), old tool
results are replaced by one-line stubs until it is well under it, so the
history stays append-only for several turns and the provider's prompt cache
keeps paying (Anthropic models are marked for caching), a lane that ends without opening every
changed file is asked once to look at them, an answer cut off at the
output cap gets one chance to post what it was sure of, and a session
that repeats one tool call with the same arguments is refused and then
stopped as stuck (`agent.max_repeated_calls`, for every session, recorded
on the run). Code search is
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

Lanes draft their findings; nothing is posted while they work. When
`[review.fact_check]` names a second model (the example uses Claude Opus 5.5
at `effort = "high"`, with Claude Sonnet 5.5 as the backup), it checks the
drafts of all lanes together once the lanes have finished: one session per
checking model and ten drafts, with the diff once, never the lane's own model
first (#189). It confirms or rejects each draft, or calls it the same as an
earlier draft or an existing finding, which is then merged. Only what it
confirms is posted. `henk runs show <id>` lists the checks as `check-<n>`
sessions and every draft with its verdict, checker and comment; so does the
dashboard's run page. `effort` on an Anthropic model sets how much it
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
henk doctor            # secrets, models, GitHub App token, MCP servers, database, git
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
6. `RUST_LOG=info,henk=debug henk review <url>` on a pull request in an
   allowlisted repository. The run id is printed first; `henk runs show <id>`
   prints the run, lanes, tool calls, transcripts, findings and timeline
   afterwards, and `henk runs show <id> --transcript lane-a` what that lane's
   model saw and said.

Every tool call of every session (review lanes, fact-checks, the planner,
address runs) is on the run record (#190): the session, model, turn, tool
and where it comes from, how it ended (`ok`, `error`, refused by the scope
or the repeat guard, an unknown tool, malformed arguments, not run,
cancelled), how long it ran, the size of its result, and its arguments as
the model sent them, cut at `[agent] record_argument_bytes` (4096 by
default, 0 for none) with their full length kept. Arguments are code, paths
and commands from the repository, never a credential (§8.4). The calls are
kept with the run, like its lanes and findings. `henk runs show` and the
dashboard's run page count them per lane and tool, each session logs one
`tool usage` line at info, and the `tool_calls` table answers questions
across runs.

Every session's whole conversation is stored with its run too (#191): the
system prompt, every message, each tool call with its arguments and what it
returned, kept whole. `henk runs show <id>` lists them by session, and
`henk runs show <id> --transcript <session>` prints one. On the dashboard,
each lane on the run page links to its transcript at
`/dashboard/runs/{id}/transcripts/{session}`, behind the same sign-in. A
transcript holds what the model was shown: code, diffs, comments and
command output from the repository, never a credential (§8.4); a test
fetches with a known token and checks that no transcript or tool call holds
it. Transcripts are large, so they follow `server.keep_events_days` like
inbound events; the run, its lanes, findings and tool calls are kept.
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
cargo run -q -- review <url> --commit <sha>                                              # a specific commit, not the head
cargo run -q -- runs show r-20261005-8f8b812b                                            # the run, lanes, findings, timeline
cargo run -q -- runs show r-20261005-8f8b812b --transcript lane-a                        # what lane-a's model saw and said
cargo run -q -- llm models --model proxy                                                 # model names the endpoint serves
```

Another model is a name from `llm models` in `[models.<id>].model`, or a
second `[models.*]` block and a second lane under `[review].lanes`; lane
names must not equal a model name. Another repository is one more entry in
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
henk address https://github.com/owner/repo/pull/7 --note "only the typo"
henk llm probe --model proxy-fast                  # one prompt to a model
henk llm models --model proxy-fast                 # what that endpoint serves
henk mcp probe --server github --show pull_request_read
henk runs show r-20261005-1a2b3c4d                 # a run from the configured database
henk runs show r-20261005-1a2b3c4d --transcript lane-a   # one session's conversation
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
pushes one commit to the pull request's branch. Only a changeset leaves the workspace: Henk
refuses it whole when a path leads into `.git` or out of the repository,
becomes a symbolic link or a submodule, or the run changed more files than
allowed, and otherwise applies it to a fresh checkout nothing ran in and
commits from there. The push is a fast-forward only. A fork, the default branch, a
protected branch or a branch that moved while he worked gets nothing.
Then he replies in each thread (fixed with the commit link, declined with
why, or a question), resolves only the threads of his own findings that he
fixed, and sums up with the run link. The model never holds the token or a
git command; Henk's code commits and pushes. `[workspace]` picks the
backend and its limits, per repository through profiles. `host` runs the
checks as Henk's user with an empty environment, so they see none of
Henk's secrets, but nothing else isolates them. `ssh` runs each run as a
throwaway user on a sandbox host (see "Sandbox host" below). `henk config
check` warns about what each one does not isolate. Every command is on the
run's timeline (`henk runs show`). On GitHub the App
needs `Contents: write`. On GitLab Henk reads `[gitlab].token_env`
himself: git pushes with it as `oauth2`, and the merge request's projects,
the branch's protection and his account id come from the REST API with it
in the `PRIVATE-TOKEN` header, because the MCP server's merge request
leaves the project ids out. Without it GitLab reviews still run and
address runs are refused. The token's account needs Developer access to
push.

The commit ends in a trailer block, in this order: `Henk-Run` (the run),
`Requested-by` (the requester's Discord id), the requester's
`Co-authored-by` and `Signed-off-by`, and Henk's `Signed-off-by` last, as
the committer's. By default Henk signs off, so DCO checks pass, and the
requester is credited as co-author; the requester does not sign off.
`[address.trailers]` changes the defaults and
`[address.repositories."owner/name"]` changes them for one repository on
the allowlist. Turning on `requester_signoff` for a repository means the
requester certifies the Developer Certificate of Origin for a change they
asked for but did not type: enabling it is the operator's statement that
requesters in that repository agree to that.

Henk commits as the App, `meneer-henk[bot]` with its GitHub noreply
address, unless `[address.identity]` sets a `name` and `email`; that
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

Every inbound event is recorded in Henk's own database with what each listener
did with it, and the payload as received (up to 256 KB). Recordings stay in
the service; nothing is sent anywhere. While serving, Henk deletes events
and their outcomes older than `server.keep_events_days` (30 by default) once
an hour, and session transcripts of that age with them. Runs are kept, since
their links are posted on the platforms. An
event and its outcomes are on the dashboard at `/dashboard/events/{id}`, a
run and the events that led to it at `/dashboard/runs/{id}`.

### Sandbox host

The `ssh` workspace backend (#84) runs an address run's commands on a
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
review = true
```

With `review = true` on a profile, reviews of its repositories get
workspaces too: every lane and the fact-checker get their own, holding the
reviewed commit (fetched by its sha, so a push during the review changes
nothing), set up as above before the lanes start. They are opened in
parallel, so a review waits about one setup, and closed when the lanes
end. A review workspace is never exported, and a lane whose workspace
could not be opened or set up reviews through the platform as before, with
the reason on the run's timeline. In its workspace a lane, and the
fact-checker, list files (`list_files`, with a glob), search them by
regular expression (`search`) and read any file by line range
(`read_file`), the same code tools an address run has, and run a command
of their own (`bash`, #85): one test, a build or a grep, as the
workspace's user, with the repository's toolchain and the profile's time
and output limits. Each command is on the run's timeline with the lane
that ran it, so `henk runs show <id>` lists what a review ran. Without a
workspace a lane reads through the platform as before.

With `plan = true` on a profile, the planner gets a workspace too (#172):
one copy of the repository at its default branch, fetched by the sha Henk
read, set up as above and never exported, with the same code tools and
`bash` and a prompt that asks it to ground the plan in what it read. The
platform's tools stay for issues, pull requests and history. An address
run gets `bash` beside `run_checks` on any backend apart from Henk: one
test or build of its own choosing while it works, where every file it
changes is part of the commit and the checks still decide. Neither is
available on the `host` backend, where those commands would run as Henk's
own user: `plan = true` is refused there, and an address run there keeps
`run_checks` only.
`henk doctor --probe` says when `grep -P` on the host is missing or does
not read Unicode as the `search` tool needs.

A review runs the setup stage on code that anyone who can open a pull
request chose, from a fork too, so `review = true` needs a backend apart
from Henk: `henk config check` refuses it on the `host` backend, where that
code would run as Henk's own user next to his configuration and keys.

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
review = true
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
- each run with its lanes, tool calls, drafts and their verdicts, findings,
  timeline and the events that led to it, and each lane's whole
  conversation; a running run updates as it happens: lanes start and end,
  tool calls, drafts and verdicts appear, without a reload;
- the inbound events with what each listener did;
- tools: how each tool fared per model and kind of session, with error
  and refusal rates, the calls that went wrong with their arguments, and on
  each run every call by turn, linked to that turn of the conversation;
- review quality: what the fact-check made of the lanes' drafts per model,
  lane, repository or pull request, the rejection rate, and the rejected
  drafts with the checker's reason;
- a health page from configuration and the database, including whether
  `git` runs where Henk does and how many recent reviews got the
  workspaces their profile asks for;
- a form that starts a review, plan or address run, and a button that
  cancels a running one.

Starting works exactly like `POST /review`, `/plan` and `/address`: the
request becomes an event, the listeners apply the allowlist and every
refusal, and the browser goes to the event's page to see what they did.
Each event records who asked as `github:<user id>`. A cancelled run ends
`cancelled`, with a neutral check for a review and one comment saying which
GitHub account cancelled it; an address run cancelled before its push
pushes nothing.

The app reads and acts only through the JSON API at `/dashboard/api/v1`,
behind the same sign-in; every action carries the session's CSRF token in a
header, comes from `public_base_url` (its `Origin`, or else its `Referer`)
and has a JSON body, and anything else is refused. `docs/API.md` lists the
endpoints, for scripts too.

People sign in with GitHub, and only the GitHub user ids in
`allowed_github_ids` get in. The id is checked on every request, so taking an
id off the list ends that access at once. To set it up:
1. Create a GitHub OAuth App (Settings, Developer settings, OAuth Apps).
2. Set its callback URL to `{public_base_url}/dashboard/auth/callback`.
3. Put its client id and secret in the two variables, and a random key in
   the third.

GitHub's token is used once, to read who signed in, and is not kept. The
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
- Everywhere: the only rate limit is `review.max_concurrent`. On GitHub, a
  review waiting for a slot shows its check as *queued* until it starts
  (§3.3).
