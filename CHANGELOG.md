# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
