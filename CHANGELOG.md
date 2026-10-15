# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.7](https://github.com/StephanMeijer/meneerhenk/compare/v0.4.6...v0.4.7) - 2026-10-15

### Added

- *(platform)* the open pull requests of a base that are behind it ([#579](https://github.com/StephanMeijer/meneerhenk/pull/579))

### Fixed

- *(review)* the commits of a review loop carry no finding ids ([#585](https://github.com/StephanMeijer/meneerhenk/pull/585))

### Other

- Merge pull request #599 from StephanMeijer/fix/commit-no-finding-ids

## [0.4.6](https://github.com/StephanMeijer/meneerhenk/compare/v0.4.5...v0.4.6) - 2026-10-15

### Added

- *(execute)* the roles' tools for a requirements review, and its section in the pull request body ([#535](https://github.com/StephanMeijer/meneerhenk/pull/535))

### Other

- *(plan)* the point tools and their draft kind can be about an execute run's changes ([#535](https://github.com/StephanMeijer/meneerhenk/pull/535))

## [0.4.5](https://github.com/StephanMeijer/meneerhenk/compare/v0.4.4...v0.4.5) - 2026-10-15

### Added

- *(config)* an [autonomous] section turns on ticket runs ([#536](https://github.com/StephanMeijer/meneerhenk/pull/536))
- *(store)* link a run to the runs it started ([#536](https://github.com/StephanMeijer/meneerhenk/pull/536))
- *(domain)* the ticket run kind and its plan, execute and review stages ([#536](https://github.com/StephanMeijer/meneerhenk/pull/536))
- *(executor)* apply_patch for GPT-family models, chosen by edit_format ([#561](https://github.com/StephanMeijer/meneerhenk/pull/561))

### Other

- Merge pull request #578 from StephanMeijer/feat/apply-patch
- Merge pull request #588 from StephanMeijer/feat/ticket-awaited-review
- Merge pull request #590 from StephanMeijer/feat/ticket-kind

## [0.4.4](https://github.com/StephanMeijer/meneerhenk/compare/v0.4.3...v0.4.4) - 2026-10-15

### Added

- *(queue)* one run per pull request, rebases first ([#580](https://github.com/StephanMeijer/meneerhenk/pull/580))

### Fixed

- *(queue)* drop a review held for a rebase when a newer one starts

## [0.4.3](https://github.com/StephanMeijer/meneerhenk/compare/v0.4.2...v0.4.3) - 2026-10-15

### Added

- *(rebase)* rebase a pull request onto its base on request ([#579](https://github.com/StephanMeijer/meneerhenk/pull/579))
- *(git)* rebase a checkout onto its base and push it with a lease ([#579](https://github.com/StephanMeijer/meneerhenk/pull/579))
- *(domain)* the rebase run kind, its stage and the @meneer-henk rebase command ([#579](https://github.com/StephanMeijer/meneerhenk/pull/579))

### Fixed

- *(rebase)* stop on conflicts that leave no markers to resolve

### Other

- *(runs)* say how a run ended on its issue or its pull request ([#579](https://github.com/StephanMeijer/meneerhenk/pull/579))
- *(review)* the loop takes its deadline when it starts and reads the executor's file limit from its edit context

## [0.4.2](https://github.com/StephanMeijer/meneerhenk/compare/v0.4.1...v0.4.2) - 2026-10-15

### Added

- *(executor)* commit in the repository's convention ([#400](https://github.com/StephanMeijer/meneerhenk/pull/400))
- *(config)* effort on OpenAI models, shown with how it is sent ([#366](https://github.com/StephanMeijer/meneerhenk/pull/366))
- *(review)* the reviewer reads the pull request's CI ([#420](https://github.com/StephanMeijer/meneerhenk/pull/420))
- *(llm)* reasoning effort for OpenAI models ([#366](https://github.com/StephanMeijer/meneerhenk/pull/366))
- *(platform)* read a pull request's CI: its jobs, a job's log and annotations ([#420](https://github.com/StephanMeijer/meneerhenk/pull/420))

### Other

- Merge pull request #441 from StephanMeijer/refactor/small-std-helpers
- Merge pull request #440 from StephanMeijer/refactor/review-queued-endings
- Merge pull request #438 from StephanMeijer/refactor/config-role-views
- Merge pull request #436 from StephanMeijer/refactor/plan-finish-and-tools
- Fix review findings
- Merge pull request #432 from StephanMeijer/feat/openai-effort
- Fix review findings
- Fix review findings
- *(llm)* one place for tool arguments as text and from the wire
- *(mcp)* truncate names with String::truncate
- Merge pull request #435 from StephanMeijer/refactor/llm-tool-arguments
- *(session)* end_events records repeat firings and derives failure itself
- *(events)* borrow GitLab object_attributes instead of cloning
- *(platform)* GitHub calls the percent-encoder by its own name
- *(platform)* truncate with str::floor_char_boundary
- *(platform)* share GitLab's number and text readers
- *(platform)* one percent-encoder for GitLab path segments and GitHub query values
- *(platform)* one helper for a GitHub answer's body or status error
- Fix review findings
- *(store)* share stored-value checks and empty-time defaults

## [0.4.1](https://github.com/StephanMeijer/meneerhenk/compare/v0.4.0...v0.4.1) - 2026-10-13

### Added

- *(queue)* a waiting review can be moved to any place in the queue, by the API, MCP and the dashboard

### Fixed

- *(review)* a merged pull request ends the review as success, and no model session starts

### Other

- Merge pull request #408 from StephanMeijer/feat/queue-order

## [0.4.0](https://github.com/StephanMeijer/meneerhenk/compare/v0.3.1...v0.4.0) - 2026-10-13

### Added

- *(executor)* [**breaking**] the executor runs each repository's own checks; run_checks and check_commands are removed

### Fixed

- *(push)* commits name only a requester Henk knows, and none when nobody known asked
- *(github)* a failing token mint is retried once per request, under its own lock
- *(github)* a passing 5xx no longer loses a run's writes

### Other

- *(executor)* the sandbox-host wait test takes no check exit code after #398
- Merge pull request #405 from StephanMeijer/feat/ssh-host-limit
- Fix review findings
- Merge remote-tracking branch 'origin/main' into pr-405-fix
- Merge pull request #404 from StephanMeijer/fix/known-requester
- Merge pull request #391 from StephanMeijer/fix/workspace-error-causes
- Merge pull request #387 from StephanMeijer/fix/github-retries
- Merge remote-tracking branch 'origin/main' into pr-398
- Fix review findings
- Merge pull request #388 from StephanMeijer/fix/gitlab-retries
- Merge remote-tracking branch 'origin/main' into pr-387
- Fix review findings

## [0.3.1](https://github.com/StephanMeijer/meneerhenk/compare/v0.3.0...v0.3.1) - 2026-10-13

### Added

- *(workspace)* the ssh backend serves a pool of sandbox hosts

### Other

- Merge pull request #390 from StephanMeijer/feat/ssh-pool

## [0.3.0](https://github.com/StephanMeijer/meneerhenk/compare/v0.2.0...v0.3.0) - 2026-10-13

### Added

- *(github)* [**breaking**] serve several App installations
- *(config)* [**breaking**] endpoints and models are declared separately
- *(llm)* a client for OpenAI's Responses API
- *(usage)* sessions record the tokens their model spent reasoning
- *(runs)* a pairing's rounds are recorded and shown in order, with what was handed over

### Fixed

- *(config)* [**breaking**] the OpenAI provider is spelled openai
- *(agent)* a tool name a model invents is cut and cleaned, and unknown tools count as one

### Other

- Merge pull request #384 from StephanMeijer/feat/responses-client
- Merge pull request #397 from StephanMeijer/fix/invented-tool-names
- Merge pull request #394 from StephanMeijer/fix/review-tool-arguments
- Merge pull request #395 from StephanMeijer/fix/workspace-eviction-cause
- Merge pull request #382 from StephanMeijer/feat/installations
- Fix review findings
- Merge pull request #377 from StephanMeijer/fix/openai-spelling
- Fix review findings
- *(llm)* build_endpoint_client's docs do not link a private item

## [0.2.0](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.29...v0.2.0) - 2026-10-12

### Added

- *(api)* [**breaking**] the API, streams and MCP speak of sessions and roles, not lanes
- *(store)* [**breaking**] the store keeps sessions with a role, not lanes
- *(session)* pairings keep their prompt cache warm, and every session records its cache
- *(execute)* the executor takes an issue to a pull request
- *(plan)* [**breaking**] the reviewer reviews the plan before it is published
- [**breaking**] address runs are removed; the executor fixes what the reviewer finds
- *(config)* [**breaking**] roles are configured in [roles.planner], [roles.reviewer] and [roles.executor]
- *(domain)* roles and a tool ceiling per role
- *(dashboard)* [**breaking**] the fact-check quality analytics are removed; sessions are reliability per role
- *(review)* [**breaking**] the loop is the review; lanes and the fact-check are removed
- *(config)* the example configurations review with the loop, and config check says so

### Fixed

- *(review)* a review loop cancelled during setup stops at once and ends cancelled
- *(plan)* the planner's copy prompt names only the tools it was granted

### Other

- Merge remote-tracking branch 'origin/feat/warm-cache' into feat/sessions-role
- *(review)* the loop's push carries the address run's coverage
- shared push and edit code loses its address names
- *(review)* [**breaking**] the review loop's fixer is the executor
- *(review)* the lifecycle tests run on the review loop, the lane tests in their own module
- Merge branch 'feat/execute' into feat/warm-cache
- Merge branch 'feat/plan-review' into feat/execute
- Merge branch 'feat/executor' into feat/plan-review
- Merge branch 'feat/roles-config' into feat/executor
- *(platform)* GitHubApi loses minimize_comment, which only folding summaries used

## [0.1.29](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.28...v0.1.29) - 2026-10-12

### Added

- *(review)* a pull request the loop may not push to gets a reviewer-only review

### Fixed

- *(review)* a reviewer-only review on GitLab posts its summary note

### Other

- *(review)* ReviewRun::posts no longer describes the removed lane fallback

## [0.1.28](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.27...v0.1.28) - 2026-10-12

### Added

- *(review)* the reviewer reports findings to the fixer, who gives each a verdict
- *(review)* a reviewer↔fixer loop with sessions that resume each round
- *(review)* the loop ends for a recorded reason and keeps its conversations in budget
- *(review)* [review.loop] config and sessions that resume across rounds

### Fixed

- *(session)* a compaction keeps the transcript whole and drops replayed thinking
- *(review)* a loop out of rounds with findings open is no pass
- *(store)* the overview's drafts per day leave loop findings out

### Other

- Merge pull request #305 from StephanMeijer/fix/flaky-cancel-during-setup
- Merge pull request #301 from StephanMeijer/fix/review-address-push
- Merge pull request #291 from StephanMeijer/feat/loop-limits
- Merge remote-tracking branch 'origin/main' into feat/review-loop
- Merge remote-tracking branch 'origin/main' into feat/review-loop
- Merge remote-tracking branch 'origin/feat/review-loop' into feat/loop-handoff

## [0.1.27](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.26...v0.1.27) - 2026-10-10

### Added

- *(review)* resume reviews a restart interrupted

### Fixed

- *(review)* give back a resume claim when the pull request cannot be read
- *(dashboard)* announce a claimed resume as the store holds it
- *(review)* claim a resume in the store so one process resumes a review
- *(review)* resume nothing once Henk is told to stop
- *(review)* do not resume a resumed review the reaper closed
- *(runs)* a cancel over MCP is recorded as over MCP
- *(urls)* take the web host and scheme Henk's own links use
- *(store)* compare review start times as instants when resuming

### Other

- Merge pull request #297 from StephanMeijer/fix/cancel-source
- Merge pull request #302 from StephanMeijer/fix/plan-previous-as-material
- Merge pull request #300 from StephanMeijer/fix/coordinator-key-case
- Merge pull request #299 from StephanMeijer/fix/url-hosts
- Merge pull request #295 from StephanMeijer/docs/llms-txt
- *(review)* a review cancelled over MCP or the dashboard is not resumed

## [0.1.26](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.25...v0.1.26) - 2026-10-09

### Added

- Henk as an MCP server at /mcp, behind named read and write tokens

### Fixed

- *(cancel)* the cancel notice says where the cancel came from
- *(config)* one env per MCP token, and a secret two tokens share is refused
- *(store)* SQLite orders runs by time, not text

### Other

- Merge pull request #266 from StephanMeijer/feat/mcp-server
- Merge branch 'fix/health-after-refactor' into feat/mcp-server
- Merge remote-tracking branch 'origin/main' into feat/mcp-server
- Merge pull request #267 from StephanMeijer/feat/mcp-operators
- *(mcp)* a read token may not cancel a run that is still running
- *(review)* two lanes on one line at once draft once
- Merge pull request #282 from StephanMeijer/fix/sqlite-run-order
- *(store)* index runs by started_at as an instant on SQLite

## [0.1.25](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.24...v0.1.25) - 2026-10-09

### Fixed

- *(git)* scratch directories unique per process and call

### Other

- Merge pull request #277 from StephanMeijer/fix/unique-scratch-dirs
- *(git)* two scratch directories with one name keep their own paths

## [0.1.24](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.23...v0.1.24) - 2026-10-09

### Added

- *(github)* the review check shows queued while a review waits for a slot

### Fixed

- *(review)* a waiting review keeps its place while its check is queued
- *(review)* a queued check closes when its run cannot be recorded
- *(review)* a queued check that could not be started still completes
- *(mcp)* build a child server's environment in a tested pure function ([#62](https://github.com/StephanMeijer/meneerhenk/pull/62))

### Other

- Merge pull request #272 from StephanMeijer/fix/mcp-client-env-and-probe
- Merge pull request #264 from StephanMeijer/feat/queued-check
- Merge remote-tracking branch 'origin/main' into feat/queued-check
- Merge main into feat/queued-check
- *(mcp)* prove a stdio MCP child gets only child_env's variables, at the spawn

## [0.1.23](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.22...v0.1.23) - 2026-10-09

### Fixed

- *(dashboard)* sign in for identity only, and warn about a GitHub App's client id

## [0.1.22](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.21...v0.1.22) - 2026-10-08

### Fixed

- *(dashboard)* a checkout failed by the run ending does not count as going without
- *(dashboard)* count only reviews that reached their checkout in the workspaces row

### Other

- Merge pull request #259 from StephanMeijer/fix/visible-checkout
- Merge main into fix/visible-checkout

## [0.1.21](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.20...v0.1.21) - 2026-10-08

### Other

- Merge main into feat/queued-section

## [0.1.20](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.19...v0.1.20) - 2026-10-08

### Added

- *(dashboard)* review quality with the check funnel and rate over time
- *(dashboard)* a lane's conversation as it happens
- *(dashboard)* lane reliability on the overview, and a page to compare lanes
- *(dashboard)* events as a list with an inspector beside it

### Fixed

- *(dashboard)* the daily quality chart picks its groups from the days it draws
- *(store)* leave cancelled reviews out of lane reliability

### Other

- Merge pull request #253 from StephanMeijer/feat/live-lane
- Merge pull request #246 from StephanMeijer/feat/lane-reliability
- Merge pull request #244 from StephanMeijer/feat/notice-pages
- Merge pull request #243 from StephanMeijer/feat/events-inspector
- Merge pull request #242 from StephanMeijer/feat/quality-flow
- *(dashboard)* the daily quality chart test no longer depends on the date

## [0.1.19](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.18...v0.1.19) - 2026-10-08

### Added

- *(dashboard)* the overview as a status board

### Fixed

- *(dashboard)* send the review slots on the running stream when they change

### Other

- Merge pull request #241 from StephanMeijer/feat/overview

## [0.1.18](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.17...v0.1.18) - 2026-10-08

### Added

- *(runs)* the run page as a pipeline of stages
- *(dashboard)* start dialog that checks the URL, cancel with confirmation, review again

### Fixed

- *(runs)* a review cancelled while queued shows its queue stage skipped

## [0.1.17](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.16...v0.1.17) - 2026-10-08

### Added

- *(runs)* superseded as a run status, and lanes that timed out or did not finish

## [0.1.16](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.15...v0.1.16) - 2026-10-07

### Added

- *(dashboard)* tool calls per run and across runs, with what went wrong

### Fixed

- *(dashboard)* link a plan run's tool calls to its issue, not a pull request
- *(dashboard)* link a listed tool call's turn only while its conversation is kept

### Other

- Merge pull request #222 from StephanMeijer/feat/dashboard-tools
- Merge branch 'feat/dashboard-live' into feat/dashboard-tools

## [0.1.15](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.14...v0.1.15) - 2026-10-07

### Added

- *(dashboard)* review quality: drafts, verdicts and rejection rates across runs
- *(dashboard)* a running run updates as it happens, over Server-Sent Events

### Fixed

- *(dashboard)* a run stream neither repeats nor loses a change around its snapshot

### Other

- Merge pull request #220 from StephanMeijer/feat/dashboard-live

## [0.1.14](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.13...v0.1.14) - 2026-10-07

### Added

- *(dashboard)* a Svelte app embedded in the henk binary, at /dashboard/app/

### Fixed

- *(dashboard)* hashed app files cached privately, never by a shared cache

### Other

- Merge pull request #218 from StephanMeijer/feat/dashboard-pages

## [0.1.13](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.12...v0.1.13) - 2026-10-07

### Added

- *(dashboard)* a JSON API behind the dashboard's sign-in, with CSRF
- *(review)* lanes draft, and the drafts are fact-checked together after the lanes
- *(store)* every session's whole conversation is kept with its run
- *(store)* every tool call of every session is on the run record

### Fixed

- *(dashboard)* JSON errors for a bad query, time bounds compared as times in SQLite, and a doc link that resolves
- *(agent)* a cancel records the calls after the one it cut off as not run

### Other

- Merge pull request #215 from StephanMeijer/feat/dashboard-api
- Merge pull request #193 from StephanMeijer/feat/tool-call-records

## [0.1.12](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.11...v0.1.12) - 2026-10-07

### Added

- *(workspace)* search shows context around each hit, or the files and counts
- *(plan,address)* the planner gets a copy of the default branch, the address run a shell

### Fixed

- *(workspace)* the host search stops reading once it has cap matches
- *(plan)* the planner's copy is one commit, so its texts send history to list_commits

### Other

- Merge branch 'main' into feat/plan-and-address-bash

## [0.1.11](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.10...v0.1.11) - 2026-10-07

### Added

- *(workspace)* kubernetes backend, a Pod per workspace in a sandbox namespace

### Fixed

- *(workspace)* a sandbox Pod's first process reaps what a run orphans

### Other

- Merge branch 'main' into feat/workspace-kubernetes
- *(workspace)* the script workspace is shared, ssh keeps only its connection

## [0.1.10](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.9...v0.1.10) - 2026-10-06

### Added

- *(review)* lanes and the fact-checker run commands in their own copy
- *(workspace)* no runner on the sandbox host; Henk signs in as root
- *(review)* lanes and the fact-checker list, search and read their own copy
- *(review)* every lane and the fact-checker in a workspace at the reviewed commit

### Fixed

- *(review)* each fact-check gets its own time for commands
- *(review)* the fact-checker's bash says its copy is shared
- *(workspace)* a glob narrows the files before grep, CRLF lines agree
- *(workspace)* search patterns mean the same on every grep
- *(workspace)* a search grep cannot finish is an error, and patterns agree
- *(review)* refuse review workspaces on the host backend
- a model's glob is matched in linear time, the probe checks Unicode

## [0.1.9](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.8...v0.1.9) - 2026-10-06

### Added

- *(workspace)* setup stage before the model, with mise for the toolchain

### Fixed

- *(workspace)* setup leftovers stay out of the change, mise runs in safe mode

### Other

- Merge branch 'feat/workspace-ssh' into feat/workspace-setup-mise

## [0.1.8](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.7...v0.1.8) - 2026-10-06

### Added

- *(workspace)* ssh backend, a throwaway user and checkout per run

### Fixed

- *(workspace)* a silent sandbox host no longer hangs the connection
- *(workspace)* no sweep of a live workspace, no user left by a failed create
- *(workspace)* root never reads the tree a run's user controls

### Other

- Merge pull request #163 from StephanMeijer/fix/read-every-page

## [0.1.7](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.6...v0.1.7) - 2026-10-06

### Added

- *(dashboard)* start and cancel runs, with CSRF
- *(agent)* refuse repeated tool calls and stop a stuck session
- record repeat-guard firings on the run and configure the limit

### Fixed

- *(plan)* end a plan as cancelled only when the cancel stopped it
- *(dashboard)* end a review cancelled while queued at once, with a run
- *(address)* end a run as cancelled only when the cancel stopped it
- *(agent)* end a session as stuck only after the model saw the refusal

### Other

- Merge pull request #109 from StephanMeijer/feat/repeat-guard
- Merge pull request #152 from StephanMeijer/feat/dashboard-actions
- *(address)* pass the platform to the dashboard cancel test helpers

## [0.1.6](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.5...v0.1.6) - 2026-10-06

### Added

- *(address)* sign off and credit the requester on the address-run commit
- *(domain)* commit identities and the trailer block of Henk's commit
- *(address)* credit a GitLab requester by their gitlab_id

### Fixed

- *(domain)* build GitLab noreply addresses with the instance's host

### Other

- Merge pull request #110 from StephanMeijer/feat/commit-trailers
- Merge pull request #157 from StephanMeijer/fix/flaky-trace-test

## [0.1.5](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.4...v0.1.5) - 2026-10-06

### Fixed

- *(planner)* stream web_fetch bodies and stop at the cap

## [0.1.4](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.3...v0.1.4) - 2026-10-06

### Added

- *(dashboard)* sign-in for run and event pages, and pruning of old events
- *(address)* run henk address on GitLab merge requests
- *(address)* git credentials with a per-platform username
- *(platform)* GitLab address writer for merge requests

### Fixed

- *(scope)* drop GitLab search_repositories and pin file reads to the reviewed commit
- *(scope)* refuse search queries that can escape the repository pin
- *(review)* recognise Henk's comments by author, not by marker

### Other

- Merge pull request #145 from StephanMeijer/fix/web-fetch-ssrf
- *(address)* run the address cases on GitLab, and document it
- update Cargo.toml dependencies

## [0.1.3](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.2...v0.1.3) - 2026-10-06

### Added

- *(dashboard)* a read-only web dashboard behind GitHub sign-in

### Fixed

- *(dashboard)* trace the dashboard routes once, not the whole router again
- *(dashboard)* set workspace_provider in the test fixture
- *(dashboard)* set live_runs and test_issue_writer in the test fixture
- *(dashboard)* count every running run, not one page of them
- *(dashboard)* judge GitHub addresses by their parsed host
- *(gitlab)* page repo_labels through a shared helper
- *(diff)* unquote git's C-quoted paths

## [0.1.2](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.1...v0.1.2) - 2026-10-06

### Added

- *(skills)* operator-configured SKILL.md for lanes, the planner and the fact-checker

### Fixed

- *(skills)* set the cache token counts in the skill tool's Usage
- *(skills)* refuse a SKILL.md that is a symbolic link
- *(skills)* list links in a skill folder without following them

### Other

- *(agent)* stop linking the private low-water constant from compact
- *(llm,agent)* prompt caching on Anthropic and compaction to a low-water mark

## [0.1.1](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.0...v0.1.1) - 2026-10-06

### Added

- *(address)* export a changeset and push from a clean checkout
- *(workspace)* Workspace trait with host and fake backends, exec trace
- *(config)* [workspace] section
- *(domain)* workspace policy and changeset rules
- *(plan)* plan GitLab work items: type, hierarchy, links and triage fields
- *(address)* address review feedback on GitHub pull requests
- *(store)* PostgreSQL as a run-store backend
- *(agent)* warn lanes three turns before the turn limit
- *(cli)* henk review takes several pull requests
- *(review)* get_file_diff reads several files per call; the file list opens the lane
- *(review)* review.ignore leaves lockfiles and generated files out
- *(marker)* name the fact-checking model in the hidden marker
- *(review)* fact-check findings with a second model, and withdraw wrong ones
- *(llm)* effort and adaptive thinking for Anthropic models

### Fixed

- *(address)* register address runs with the live-run set
- *(address)* a model refusal fails an address run
- *(plan)* refuse a sub-issue under a GitLab task before spending budget
- *(domain)* flag emoji outside the four checked ranges
- *(liveness)* reap stale runs every minute while serving
- *(agent)* a model refusal ends a run as a refusal, not a clean review
- *(store)* refuse numbers beyond i64 on SQLite too
- *(review)* an interrupted review ends, and orphaned runs are reaped
- *(review)* a superseded review ends neutral and posts no failure
- *(review)* a withdrawn finding keeps its author and stops asking for action
- *(cli)* a crashed review counts as a failed one
- *(scope)* refuse another repository instead of swapping it in
- *(agent)* compaction drops thinking, hints for guessed tool names, no <think> on the run page

### Other

- *(address)* tools and checks on the Workspace interface
- *(review)* lane_opening builds a lane's first message
- *(review)* run_review end to end on fakes
- *(store)* joining a run records one request per join
- *(agent)* compaction trims file reads before diffs
- *(review)* ReviewRun and LaneInputs instead of 14-argument functions
- *(deps)* drop rand; ids use ring's SystemRandom
- *(deps)* base64 0.23
- *(deps)* rusqlite 0.40 and rusqlite_migration 2.6
- Rust toolchain 1.93 -> 1.95
- *(store)* remove the unused RunStore::running_review
- give every non-test #[allow] a reason (#[expect])
