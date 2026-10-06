# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
