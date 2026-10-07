# Architecture

How Meneer Henk is built. The spec in [docs/SPEC.md](docs/SPEC.md) says what he
does and promises; this file says how the code keeps those promises. The README
says how to configure and run him. Section numbers (§3.2 and so on) refer to the
spec.

This version covers code review (§3) and issue planning (§4) on GitHub and
GitLab. Discord (§5), email (§6) and voice (§5.5) are later passes; they appear
dashed in the first diagram so the shape of the whole is visible.

The diagrams are Mermaid, which GitHub renders in place.

## 1. System context

Henk is one process that is an MCP *client*. The platforms are reached through
external MCP servers started as child processes; models are reached over HTTPS.
Nothing a model says reaches a platform without passing through Henk's own code.

```mermaid
flowchart LR
    subgraph platforms["Platforms"]
        GH["GitHub"]
        GL["GitLab"]
    end
    subgraph edge["Edge"]
        RP["Reverse proxy, TLS"]
    end
    subgraph process["henk serve, one process"]
        SRV["HTTP server<br/>webhooks, API, run pages"]
        COORD["Coordinator<br/>one review per PR at a time"]
        REV["Review orchestrator<br/>lanes in parallel"]
        PLAN["Planner"]
        DB[("SQLite or PostgreSQL<br/>run records")]
    end
    subgraph children["Child processes, started by Henk"]
        GHR["github-mcp-server<br/>read-only toolsets"]
        GLR["mcp-gitlab<br/>readonly mode"]
        GLW["mcp-gitlab<br/>modify mode, tool allow-list"]
    end
    subgraph models["Model endpoints"]
        PROXY["llm3.9xx.nl<br/>OpenAI and Anthropic APIs"]
        ANTH["Anthropic"]
        OTHER["OpenAI, Ollama"]
    end
    BAO["OpenBao"]
    DISC["Discord"]:::later
    MAIL["Email"]:::later
    VOICE["Voice"]:::later

    GH -- "webhooks, signed" --> RP
    GL -- "webhooks, token" --> RP
    RP --> SRV
    SRV --> COORD
    COORD --> REV
    SRV --> PLAN
    REV --> DB
    PLAN --> DB
    REV -- "MCP over stdio" --> GHR
    REV -- "MCP over stdio" --> GLR
    PLAN -- "MCP over stdio" --> GHR
    PLAN -- "MCP over stdio" --> GLR
    REV -- "REST and GraphQL as the App" --> GH
    REV -- "MCP over stdio" --> GLW
    PLAN -- "REST as the App" --> GH
    PLAN -- "MCP over stdio" --> GLW
    GHR -- "App installation token" --> GH
    GLR -- "personal access token" --> GL
    GLW -- "personal access token" --> GL
    REV -- "HTTPS" --> PROXY
    REV -- "HTTPS" --> ANTH
    REV -- "HTTPS" --> OTHER
    PLAN -- "HTTPS" --> PROXY
    BAO -. "environment variables at start" .-> process
    DISC -.-> SRV
    MAIL -.-> SRV
    VOICE -.-> DISC

    classDef later stroke-dasharray: 5 5,fill:none
```

What to notice: two sessions per platform. The read-only session is the only
one a model can reach, and even that only through the guard of section 4. The
write-mode session (GitLab) and the App REST client (GitHub) are driven by
Henk's code with arguments it constructs.

## 2. Crates

```mermaid
flowchart TB
    HENK["henk<br/>binary: hooks, listeners, orchestration, server, CLI"]
    EVENTS["henk-events<br/>event model, parsers, bus, local record"]
    SESSION["henk-session<br/>SessionSpec, run_session, guarded platform tools"]
    AGENT["henk-agent<br/>the tool-calling loop, prompts"]
    PLATFORM["henk-platform<br/>webhooks, GitHub App, writers"]
    STORE["henk-store<br/>run records: SQLite or PostgreSQL"]
    LLM["henk-llm<br/>ModelClient: OpenAI-compatible and Anthropic"]
    MCP["henk-mcp<br/>MCP sessions on rmcp"]
    DOMAIN["henk-domain<br/>the spec's rules<br/>no I/O, no async, no credentials"]

    HENK --> EVENTS
    HENK --> SESSION
    HENK --> AGENT
    HENK --> PLATFORM
    HENK --> STORE
    HENK --> LLM
    HENK --> MCP
    HENK --> DOMAIN
    EVENTS --> DOMAIN
    SESSION --> AGENT
    SESSION --> LLM
    SESSION --> MCP
    SESSION --> STORE
    SESSION --> DOMAIN
    AGENT --> LLM
    AGENT --> MCP
    AGENT --> DOMAIN
    PLATFORM --> MCP
    PLATFORM --> LLM
    PLATFORM --> DOMAIN
    STORE --> DOMAIN
```

| Crate | Modules | Depends on |
|---|---|---|
| `henk-domain` | `identity`, `allowlist`, `review`, `finding`, `marker`, `scope`, `queue`, `plan`, `run`, `text`, `skill`, `discord`, `mail` | serde, thiserror, serde_json |
| `henk-llm` | `types`, `client`, `openai`, `anthropic`, `http`, `schema`, `testing` | reqwest with rustls (ring) |
| `henk-mcp` | `session`, `config`, `names`, `testing` | rmcp 3.5 (client features only) |
| `henk-agent` | `agent`, `tool`, `mcp_tools`, `prompts` and `prompts/*.md` | domain, llm, mcp |
| `henk-session` | `lib`: `SessionSpec`, `run_session`, `platform_tools` | domain, llm, mcp, agent, store |
| `henk-events` | `event`, `github`, `gitlab`, `bus` | domain, tokio |
| `henk-platform` | `webhook`, `writer`, `issue`, `github/{app,api,writer,issues}`, `gitlab/{writer,issues}` | domain, llm, mcp, ring, hmac |
| `henk-store` | `store` (the async `RunStore` trait), `sqlite`, `postgres`, `migrations/{sqlite,postgres}/` | rusqlite (bundled), tokio-postgres with deadpool, rustls (ring) |
| `henk` | `main`, `config`, `app`, `hooks/{github,gitlab,api}`, `listeners/{filter,review,mention,plan}`, `recorder`, `review`, `review_tools`, `plan`, `plan_tools`, `skills`, `skill_tools`, `web_fetch`, `coordinator`, `server`, `pages`, `dashboard/{auth,session,views,api,app}`, `urls`, `doctor`, `ids` | all of the above, axum |

`henk-platform` depends on `henk-llm` for one function, `ensure_tls_provider`,
so every HTTPS client in the process shares the same rustls setup.

## 3. A review, end to end

A `synchronize` webhook from GitHub, start to finish. The GitLab path is the
same with the writer swapped (section 10 lists the differences).

```mermaid
sequenceDiagram
    autonumber
    participant GH as GitHub
    participant S as hooks/github.rs
    participant B as EventBus
    participant D as listeners/review.rs
    participant C as coordinator.rs
    participant R as review.rs
    participant L as Lane (henk-agent)
    participant F as Fact-check (another model)
    participant M as Model
    participant X as read-only MCP
    participant W as GitHubWriter
    participant DB as RunStore

    GH->>S: POST /webhooks/github
    S->>S: verify_github_signature, parse_github into an Event
    S->>B: publish(event)
    S-->>GH: 202 Accepted, event id
    B->>DB: record the event
    B->>D: handle(event), every listener at once
    D->>D: filter: not a bot, not Henk, no marker, allowlisted, not a draft
    D->>C: submit_review(request, head)
    B->>DB: record each listener's outcome
    C->>C: queue::decide: Start, Join or Supersede
    C->>R: run_review (background task)
    R->>W: pull_request, must be open
    R->>DB: create_run
    R->>W: start_review: check run in progress
    R->>W: existing_findings
    R->>R: FindingRegistry::seeded
    R->>X: RmcpSession::connect (child process)
    par one task per configured lane
        R->>L: Agent::run
        loop until end of turn, turn limit or deadline
            L->>M: complete(messages, tools)
            M-->>L: tool calls
            alt MCP read tool
                L->>L: scope::guard pins owner, repo, pullNumber
                L->>X: call_tool
                X-->>L: text, truncated
            else post_finding
                L->>L: check style and the line, add a draft to the DraftBook
                L->>DB: record_draft
            end
        end
        L-->>R: LaneResult Finished, Stopped or Dropped
    end
    opt fact_check configured
        loop one session per checking model and 10 drafts
            R->>F: drafts, diff once, code tools + give_verdict per draft
            F-->>R: confirmed, rejected or same_as, per draft
        end
    end
    R->>R: draft::settle
    R->>W: post_finding, update_finding, resolve_finding, Marker attached
    R->>DB: record_finding, decide_draft
    R->>W: existing_findings again
    R->>R: open = in diff and not resolved
    R->>W: fold earlier summaries and resolved findings
    R->>W: post_comment: summary + run link + Marker
    R->>W: finish_review: success, neutral or failure
    R->>DB: finish_run
```

Three details carry the spec's weight. The count comes from the second
`existing_findings` call, what the platform reports rather than memory, so
findings of earlier reviews whose line is still in the diff count too (§3.3).
The diff is fetched once per review (`PlatformWriter::diff`, parsed by
`henk_domain::diff`, minus the files `review.ignore` matches, see
`henk_domain::ignore`) and handed to lanes per file with line numbers; a lane
that ends without opening every changed file is asked once to look at them,
and the conversation is kept under a size budget by stubbing old tool
results (`henk_agent::compact`). Stubbing edits earlier turns, and current
Claude models refuse a thinking block replayed after its history changed, so
a compaction that stubs anything also drops every thinking block. An edit
also restarts the provider's prefix cache, so once over budget compaction
stubs down to a low-water mark (75% of the budget): the history then stays
append-only for several turns between edits. On an Anthropic model with
`prompt_cache` (the default), the system prompt and the end of the
conversation carry a cache marker, so each turn reads what the last one
wrote; the run timeline records how much of the prompt came from the cache.
Lanes draft inside the loop with `post_finding`, `improve_finding` and
`withdraw_finding`, and never write (§3.2, #189): a draft goes into the
review's `henk_domain::draft::DraftBook`, one per line across lanes, and
other lanes see it in `list_existing_findings`. Once every lane has ended,
`crate::fact_check` checks the drafts, with `[review.fact_check]`, in one
session per checking model and chunk of ten, never on the lane's own model
first, run one after another in the fact-checker's workspace. Each session
gets the diff of its files once and gives a verdict per draft: confirmed,
rejected, or the same as an earlier draft or an existing finding. Drafts
without a verdict go to the backup model, then out unchecked, recorded as
`unverified`. `draft::settle` turns the verdicts into writes, and
`crate::drafts` writes them with the checking model on the marker. A lane
that fails comes back as `Dropped` and the review stands on the others
(§3.3). A lane that reaches its time limit comes back as `Stopped`: what it
drafted is still checked, and the review completes.

## 4. Where a tool call goes

The most important picture in this file. A model never receives a credential
or a write tool. What it receives is a list of tool names; every call goes
through Henk's dispatcher, and every MCP call through the guard.

```mermaid
flowchart TD
    M["Model emits a tool call<br/>name + JSON arguments"] --> A["Agent::dispatch<br/>look up the name in the ToolSet"]
    A -- "unknown name" --> E1["error result back to the model<br/>naming the tool it probably meant"]
    A -- "malformed JSON" --> E1
    A -- "native tool" --> N{"which one"}
    A -- "MCP tool" --> G["scope::guard(platform, tool, args, scope)"]
    G -- "tool not in the read table" --> E2["Refused: not available in this task"]
    G -- "names another repository or target" --> E2
    G -- "allowed" --> P["pin owner and repo when left out<br/>pin pullNumber or merge_request_iid<br/>confine search queries to repo:owner/name<br/>pin file reads to the reviewed commit<br/>refuse whole-diff methods in a review"]
    P --> X["McpSession::call_tool<br/>child process over stdio"]
    X --> T["flatten content to text<br/>truncate to max_tool_output_chars"]
    T --> M
    N -- "list_changed_files, get_file_diff" --> F0["the ReviewDiff fetched once per review<br/>numbered per file, opened files tracked"]
    N -- "read_file" --> F6["a numbered line range<br/>through the guarded file read"]
    N -- "list_existing_findings" --> F1["read the shared FindingRegistry<br/>and the other lanes' drafts"]
    N -- "post_finding" --> F2["style check<br/>line must be in the diff<br/>line free of findings and other lanes' drafts<br/>a draft, written after the lanes and the fact-check"]
    N -- "improve_finding" --> F3["refuse when a person answered<br/>a draft, written after the lanes and the fact-check"]
    N -- "withdraw_finding" --> F7["refuse when a person answered<br/>a draft, written after the lanes and the fact-check"]
    N -- "write_plan, set_title, add_labels, set_fields, link_issue, ..." --> F4["ChangeBudget::spend<br/>IssueWriter call"]
    N -- "web_fetch" --> F5["https only, no private hosts<br/>GET with nothing but the URL"]
    N -- "load_skill" --> F8["a body from the agent's own skills<br/>read from SKILL.md at start, no path"]
    F0 --> M
    F6 --> M
    F1 --> M
    F2 --> M
    F3 --> M
    F7 --> M
    F4 --> M
    F5 --> M
    F8 --> M
    E1 --> M
    E2 --> M
```

The guard table lives in `crates/henk-domain/src/scope.rs`. It is a constant
list per platform of read tools and the arguments each one gets pinned. A
value the model gives that names another repository, pull request or search
scope is refused with the reason, never swapped for the right one: a silent
swap answers a question the model did not ask. A tool
that is not in the table does not exist as far as the model is concerned, and
`henk mcp probe` shows which of a server's tools the table exposes. The server
itself is also started restricted (`GITHUB_READ_ONLY=1`, `GITLAB_PERMISSION_MODE=readonly`),
so the guard is the second fence, not the only one.

## 5. Review lifecycle and overlap

```mermaid
stateDiagram-v2
    [*] --> Requested: webhook, command, CLI or API
    Requested --> Joined: same commit already under review
    Requested --> Running: Start, or Supersede after cancelling the old run
    Joined --> [*]
    state Running {
        [*] --> Lanes
        Lanes --> Summarising: every lane Finished, Stopped or Dropped
    }
    Running --> Cancelled: a newer commit arrived
    Summarising --> Finished: at least one lane finished or stopped
    Summarising --> Failed: no lane finished, or a platform write failed
    Cancelled --> [*]: check closed, no comment
    Finished --> [*]: summary posted, check success or neutral
    Failed --> [*]: failure comment, check failure
```

The decision between Start, Join and Supersede is the pure function
`henk_domain::queue::decide`; the coordinator applies it under a lock and
cancels the superseded run's token. On GitHub, `Finished` with zero open
findings concludes the check `success`, with findings `neutral`, and only
`Failed` concludes `failure`, which is Henk's failure, not the code's (§3.3).
On GitLab the commit status is always `success` with the count in its
description, because a status must never block a pipeline (§8.2).

## 6. Hooks, events and listeners

In serve mode nothing calls an orchestrator directly. A hook turns what it
receives into an `Event` and publishes it; the bus records the event, hands it
to every listener at once, and records what each listener did. Listeners never
see each other; the one rule that used to be implicit in an if/else chain,
that a review command is a command and not a mention, is now a test.

```mermaid
flowchart LR
    subgraph hooks["Hooks (henk/hooks)"]
        GHH["GitHubHook<br/>POST /webhooks/github"]
        GLH["GitLabHook<br/>POST /webhooks/gitlab"]
        API["ApiHook<br/>POST /review, POST /plan"]
        LATER["Discord, mail, timers"]:::later
    end
    BUS["EventBus<br/>record, deliver, record outcomes"]
    REC[("inbound_events<br/>event_outcomes")]
    subgraph listeners["Listeners (henk/listeners)"]
        RL["ReviewListener"]
        ML["MentionListener"]
        PL["PlanListener"]
    end
    COORD["Coordinator"]
    GHH --> BUS
    GLH --> BUS
    API --> BUS
    LATER -.-> BUS
    BUS --> REC
    BUS --> RL
    BUS --> ML
    BUS --> PL
    RL --> COORD
    PL --> COORD
    classDef later stroke-dasharray: 5 5,fill:none
```

The event model (`henk-events`): `Event { id, received_at, source, kind, payload }`,
`EventSource` (GitHub webhook with delivery id, GitLab webhook, API with
requester), `EventKind` (pull request change, comment, review requested, plan
requested, unmodelled, ignored) and `Handled` (ignored, started, joined,
superseded, greeted, failed with a reason). A webhook kind no parser models is
`Unmodelled` and still recorded, so a future listener can be written against
real recordings. Recordings never leave the service.

A session (`henk-session`) is the other abstraction: `SessionSpec` says what
varies (model, system prompt, opening messages, tools, limits) and
`run_session` does what every session shares (the lane row, the loop, the stop
mapping, the timeline line). A review is N sessions on one read-only MCP
session; a plan is one.

## 7. Planning

```mermaid
sequenceDiagram
    autonumber
    participant U as CLI or POST /plan
    participant P as plan.rs
    participant I as IssueWriter
    participant A as Agent
    participant X as read-only MCP
    participant WEB as web_fetch
    participant DB as RunStore

    U->>P: run_plan(issue, note)
    P->>I: issue: open, not a pull request
    P->>DB: create_run
    P->>P: extract_plan: previous plan, if any
    P->>A: Agent::run with guarded reads, web_fetch, tracker tools
    loop study, triage, ask, write
        A->>X: guarded reads of issue, code, related items
        A->>WEB: documentation
        A->>I: set_title, add_labels, link_issue, create_sub_issue, ask_questions
        Note over A,I: each spends ChangeBudget, sub-issues are capped
        A->>I: write_plan: with_plan_section replaces the folded section
    end
    alt plan written
        P->>I: append SessionEntry under the plan
        P->>DB: finish_run Finished
    else timeout, model error, or no plan
        P->>I: comment: Planning failed + run link
        P->>DB: finish_run Failed
    end
```

The plan section and the session log are a region delimited by HTML comments,
parsed and rendered by `henk_domain::plan`. `set_description` keeps that region
intact; `write_plan` keeps the session log and replaces the plan; the session
entry is appended by code after the run, never by the model.

## 7a. Addressing review feedback

```mermaid
sequenceDiagram
    autonumber
    participant U as CLI or POST /address
    participant R as address.rs
    participant W as AddressWriter (GitHub, GitLab)
    participant G as git.rs (checkout)
    participant S as workspace (host)
    participant A as Agent
    participant DB as RunStore

    U->>R: run_address(pull request, note)
    R->>W: pull_facts; push_refusal: fork, default, protected, closed
    R->>DB: create_run (kind address)
    R->>W: open_threads (Henk's notes by author, never by marker)
    R->>G: clone_at head, credential via env only
    R->>S: open: copy the files without .git; the clone is removed
    R->>A: tools on the workspace: read, search, edit, write, run_checks, settle_thread
    A-->>R: edits in the workspace, an outcome per thread
    R->>S: run_checks (each one on the run's timeline), export the changeset
    R->>R: changeset_refusal: .git, escapes, links, submodules, file limit
    R->>S: close (also on failure, cancel or drop)
    R->>W: user_login of the requester's configured id, for the noreply address
    R->>W: pull_facts again: head unchanged?
    R->>G: fresh clone_at head, apply the changeset, commit with Henk-Run, Requested-by and the trailers
    R->>G: push, fast-forward only
    R->>W: reply per thread; resolve Henk's own fixed findings; summary
    R->>DB: finish_run
```

The model's tools talk only to a `Workspace` (`crates/henk/src/workspace`);
the backend is a configuration choice (`[workspace]`, per repository
through profiles): `host`, or `ssh` to a sandbox host where each run is a
throwaway user, made and removed by a script Henk sends with every request
as root (`workspace/sandbox.sh`; nothing is installed there), or
`kubernetes`, a Pod per workspace in a sandbox namespace, where Henk runs
the same script through `pods/exec` in its single-user mode and the Pod is
the boundary (`workspace/kubernetes.rs`; both on `workspace/remote.rs`).
On the `host` and `ssh` backends
the record of changes is a git directory outside the tree that nothing a
check runs can write, `HOME` is outside the tree, and `.git` (the `ssh`
backend leaves the checkout's own in the tree for the run's commands) is
never something a tool reads or the changeset carries. What is pushed is
never the tree the checks ran in, only the checked changeset applied to a
fresh checkout. Every backend passes the same contract tests
(`workspace/contract.rs`).

A review opens workspaces only when its repository's profile has
`review = true` (`review_workspace.rs`): one per lane and one for the
fact-checker, at the reviewed commit, through the same setup stage
(`workspace/setup.rs`). They are wrapped by `workspace::for_lane`, which
refuses `export` for every run but an address run
(`henk_domain::workspace::EnvLane::exports`), so nothing a lane does in one
can become a commit. The code tools on a workspace (`list_files`,
`read_file`, `search`) are written once in `code_tools.rs` and shared by
the address run, the review lanes, the fact-checker and the planner
(`lane_workspace.rs` opens the read-only lanes' workspaces), with `bash`
for every lane on a backend where the model may run commands
(`BackendKind::runs_model_commands`: not the host). A `search`
pattern is checked in Henk (`workspace::Pattern`) in the syntax Rust's
regex and PCRE read alike, since the host backend matches with the one and
the `ssh` backend with `grep -P`.

Every error happens before the push: a failed run posts a failure comment
saying nothing was pushed. After the push, replies and the summary are
best-effort, so a run that pushed is never reported as failed.

## 8. Run records

```mermaid
erDiagram
    runs ||--o{ lanes : has
    runs ||--o{ findings : records
    runs ||--o{ events : logs
    runs ||--o{ requests : joined_by
    inbound_events ||--o{ event_outcomes : handled_by
    event_outcomes }o--o| runs : led_to
    runs {
        text id PK "r-YYYYMMDD-xxxxxxxx"
        text kind "review or plan"
        text platform
        text repo
        int target "PR, MR or issue number"
        text commit_sha
        text requester "stable id, when known"
        text trigger
        text status "running, finished, failed, cancelled"
        text started_at
        text finished_at
        text link "public_base_url/runs/id"
        text summary
        text error
    }
    lanes {
        text run_id FK
        text name
        text model
        text status "running, finished, dropped"
        int turns
        int input_tokens
        int output_tokens
        text error
    }
    findings {
        int id PK
        text run_id FK
        text lane
        text path
        int line
        text comment_id
        text action "posted, improved, refused"
    }
    events {
        int id PK
        text run_id FK
        text at
        text level
        text message
    }
    requests {
        text run_id FK
        text at
        text source
    }
    inbound_events {
        text id PK "e-YYYYMMDD-xxxxxxxx"
        text received_at
        text source
        text kind
        text repo
        int target
        text payload "as received, local only"
    }
    event_outcomes {
        text event_id FK
        text listener
        text outcome
        text detail
        text run_id
        text at
    }
```

Every comment Henk posts ends with a hidden marker
(`<!-- meneer-henk run=... model=... kind=... -->`) rendered by
`henk_domain::marker`. It is how he recognises his own words, how the writer
finds earlier summaries and findings, and how a comment links back to its run
(§8.1, §8.6).

## 9. Trust boundaries

```mermaid
flowchart LR
    subgraph untrusted["Untrusted: words are information"]
        MODEL["Model output<br/>tool calls, text"]
        INPUT["Diffs, comments, issues,<br/>web pages"]
    end
    subgraph henk["Henk's process: enforces"]
        GUARD["scope::guard"]
        TOOLS["review_tools, plan_tools"]
        STYLE["text::style_violations"]
        ALLOW["Allowlist"]
        MARK["Marker"]
    end
    subgraph holders["Hold credentials"]
        MCPS["MCP child processes"]
        APP["GitHub App client"]
        ENDPOINTS["Model endpoints"]
    end
    INPUT --> MODEL
    MODEL --> GUARD
    MODEL --> TOOLS
    GUARD --> MCPS
    TOOLS --> STYLE
    TOOLS --> MARK
    TOOLS --> APP
    TOOLS --> MCPS
    ALLOW --> GUARD
    henk --> ENDPOINTS
```

| Invariant (§8) | Enforced by |
|---|---|
| 1. Henk never triggers himself | `listeners/filter.rs`: events from bots, from Henk's own login, or whose body carries a `Marker` are rejected before any listener acts |
| 2. Henk is advisory | No write tool can approve, merge or push; `GitLabWriter::finish_review` always sets `success`; `ReviewOutcome::check_conclusion` only fails for an incomplete review |
| 3. Words are information | Prompts say so, but the code does not rely on it: every write goes through tools that validate arguments; `web_fetch` sends nothing but the URL |
| 4. The model never holds credentials | Secrets are read from the environment by `App::build`, `RmcpSession::connect` (passed to the child only), and `GitHubAuth`; the model sees tool names |
| 5. Scope is fixed per task | `scope::guard` pins repository, pull request and commit for reviews, repository and issue for plans; `plan_tools` refuse other issues except through `link_issue` |
| 6. Every visible action is traceable | `Marker` on every comment, `RunStore` for every run and every inbound event with its outcomes (events pruned after `server.keep_events_days`, runs kept), `/runs/{id}` leading to the run on the dashboard |
| 7. Allowlists bound the world | `Allowlist::allows` in `listeners/filter.rs`, `run_review` and `run_plan` |
| 8. Failure is visible | `report_failure` posts a failure comment and closes the check; `run_plan` posts "Planning failed"; both record the error on the run |

The dashboard (`/dashboard`) is a second way in, for people. GitHub OAuth
says who someone is; the configured GitHub user ids decide, on every
request, whether they may look and act. Starting work publishes the same
event as the API on the same bus, so the listeners decide as they do for
any request; cancelling fires one run's token through the coordinator, and
the run ends `cancelled`. Every action is a POST with the session's CSRF
token from the dashboard's own origin. Every value it shows is escaped. The run and event links posted in comments, `/runs/{id}` and
`/events/{id}`, redirect to the dashboard's pages and so need sign-in too;
without a dashboard they are not served. The dashboard's JSON API, `/dashboard/api/v1` (`dashboard/api`,
`docs/API.md`), reads the same store behind the same sign-in, and its
actions need the session's CSRF token in a header, the dashboard's own
origin and a JSON body. The dashboard app (`dashboard/`, Svelte and Vite)
is built to static files that `crates/henk/build.rs` embeds; `dashboard/app`
serves them at `/dashboard/app/` from memory, behind the same sign-in, with a
policy that allows no inline script or style.

## 10. GitHub and GitLab differences

| Concern | GitHub | GitLab |
|---|---|---|
| Reads for models | `github-mcp-server` with `GITHUB_READ_ONLY=1` | `mcp-gitlab` in `readonly` mode |
| Writes | REST and GraphQL as the App (`github/api.rs`), because the MCP server's pending-review model is a per-user singleton and unsafe for concurrent lanes | `mcp-gitlab` in `modify` mode with a tool allow-list, driven by `gitlab/writer.rs` |
| Review started | check run `in_progress` | award emoji on the merge request |
| Finding | review comment at `commit_id`, `path`, `line`, `side` | diff thread with a `position` built from the merge request's `diff_refs` |
| Folding | GraphQL `minimizeComment` as `OUTDATED` | the earlier summary note is edited to `*Outdated.*` plus its marker |
| Review ended | check `success`, `neutral` or `failure` | commit status always `success`, count in the description |
| Thread state | GraphQL review threads: `isResolved`, who replied | `mr_discussions`: `resolved`, who replied |
| Mention reply | reply in the review thread, or a conversation comment | reply in the discussion, or a note |

## 11. Configuration and secrets

`henk.toml` holds ids, the allowlist, models, lanes and the MCP server commands.
It never holds a secret; every secret is named by the environment variable that
carries it, and the deployment fills those variables from OpenBao.

| Variable | Read by | Reaches |
|---|---|---|
| `[models.*].api_key_env` | `App::build` | the model client, as a bearer token or `x-api-key` |
| `GITHUB_APP_PRIVATE_KEY_PATH` | `App::build` and, by name, the GitHub MCP child | `AppCredentials` (RS256 on ring) and the child's own token minting |
| `GITLAB_PERSONAL_ACCESS_TOKEN` | passed by name to both GitLab MCP children; `App::build` for address runs | the children; `GitLabRest` in the `PRIVATE-TOKEN` header, and git's environment as `oauth2` Basic; never argv, a log or a model |
| `HENK_GITHUB_WEBHOOK_SECRET`, `HENK_GITLAB_WEBHOOK_TOKEN` | `server::router` | signature and token checks |
| `HENK_API_TOKEN` | `server::router` | `POST /review` and `POST /plan` |

Child processes get a cleared environment plus a short inherited list (`PATH`,
`HOME`, locale, temp) and the variables their configuration names
(`RmcpSession::connect`). A token meant for one server never reaches another.

## 12. Deployment

```mermaid
flowchart LR
    subgraph host["Host"]
        RP["Reverse proxy<br/>TLS, public_base_url"]
        subgraph ctr["Container: distroless cc, user 65532, read-only root"]
            HENKBIN["/usr/local/bin/henk"]
            GHBIN["/usr/local/bin/github-mcp-server"]
            NODE["/nodejs/bin/node<br/>/opt/mcp-gitlab"]
            CFG["/etc/henk/henk.toml (mounted)"]
            VOL[("/var/lib/henk<br/>the one writable path")]
        end
        ENV["Environment from OpenBao"]
    end
    RP -- ":8080" --> HENKBIN
    HENKBIN --> GHBIN
    HENKBIN --> NODE
    HENKBIN --> VOL
    CFG --> HENKBIN
    ENV --> HENKBIN
```

The image is built by the `Dockerfile` with a cross-compiling Rust stage, so an
arm64 image comes off an amd64 builder. `henk doctor` runs inside the container
and starts both child servers, which is the quickest way to find a missing
variable before the first webhook arrives.

## 13. Decisions and deviations

- **rmcp with a small loop of our own, not goose.** The published goose crates
  are alphas without MCP client wiring, the crate with the Agent is unpublished
  and reads global configuration, and its MCP layer is rmcp anyway. Owning the
  dispatcher is what makes §8.4 and §8.5 properties of code.
- **Hand-written model adapters.** Two wire formats cover the proxy, Anthropic,
  OpenAI and Ollama in about a thousand lines, including the details an MCP
  host needs that the libraries lack: error flags on tool results, tool-name
  mapping, untouched schemas, opaque thinking blocks echoed back.
- **GitHub writes over REST.** See section 10.
- **GitLab's built-in MCP server is not used.** It is OAuth-only in its
  documentation and lacks commit statuses and award emoji; `mcp-gitlab` takes a
  personal access token and covers everything needed.
- **Provisional answers to the spec's open questions** are listed at the end of
  the README and move into `docs/SPEC.md` once confirmed.
