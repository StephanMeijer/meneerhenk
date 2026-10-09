# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
