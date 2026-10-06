# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.4](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.3...v0.1.4) - 2026-10-06

### Added

- *(address)* run henk address on GitLab merge requests
- *(address)* git credentials with a per-platform username
- *(skills)* operator-configured SKILL.md for lanes, the planner and the fact-checker
- *(domain)* workspace policy and changeset rules
- *(plan)* plan GitLab work items: type, hierarchy, links and triage fields
- *(address)* address review feedback on GitHub pull requests
- *(review)* review.ignore leaves lockfiles and generated files out
- *(marker)* name the fact-checking model in the hidden marker
- *(review)* fact-check findings with a second model, and withdraw wrong ones
- *(domain)* hidden notes for AI agents on every marked comment
- *(domain)* parse pull request diffs per file
- *(review)* a lane that reaches its time limit stops instead of failing
- *(events)* add henk-events with the event model, parsers, bus and local record
- *(llm)* effort and adaptive thinking for Anthropic models
- *(llm)* list the models an endpoint serves
- *(agent)* warn lanes three turns before the turn limit
- *(review)* get_file_diff reads several files per call; the file list opens the lane
- *(prompts)* review file by file with the lane's own diff tools
- *(agent)* continuations, one nudge before a lane ends
- *(agent)* keep the conversation under a size budget
- *(review)* hand out the diff per file and validate findings against it
- *(agent)* log each tool call and the final text at debug level
- *(store)* PostgreSQL as a run-store backend
- *(review)* withhold code search when the repository is not indexed
- *(session)* write a local transcript when HENK_TRANSCRIPT_DIR is set
- *(session)* add henk-session, one way to run a model session
- *(platform)* GitLab address writer for merge requests
- *(platform)* fetch the diff of a review at its commit
- *(github)* check the App identity and list installations in doctor
- *(dashboard)* a read-only web dashboard behind GitHub sign-in
- *(cli)* add runs show and print the run id before a review starts

### Fixed

- *(scope)* drop GitLab search_repositories and pin file reads to the reviewed commit
- *(scope)* refuse search queries that can escape the repository pin
- *(diff)* unquote git's C-quoted paths
- *(review)* an interrupted review ends, and orphaned runs are reaped
- *(review)* a superseded review ends neutral and posts no failure
- *(review)* a withdrawn finding keeps its author and stops asking for action
- *(scope)* refuse another repository instead of swapping it in
- *(scope)* GitLab lanes do not get whole-diff tools either
- *(scope)* pin reads to the reviewed commit and refuse whole-diff reads
- *(agent)* a model refusal ends a run as a refusal, not a clean review
- *(llm)* survive first contact with OpenAI-compatible proxies
- *(agent)* compaction drops thinking, hints for guessed tool names, no <think> on the run page
- *(agent)* keep debug log fields on one line
- *(prompts)* name the diff tools in the review lane prompt
- *(review)* recognise Henk's comments by author, not by marker
- *(gitlab)* page repo_labels through a shared helper
- *(plan)* refuse a sub-issue under a GitLab task before spending budget
- *(dashboard)* count every running run, not one page of them
- *(store)* refuse numbers beyond i64 on SQLite too

### Other

- Merge pull request #145 from StephanMeijer/fix/web-fetch-ssrf
- *(address)* run the address cases on GitLab, and document it
- Merge pull request #77 from StephanMeijer/fix/emoji-ranges
- Rust toolchain 1.93 -> 1.95
- initial commit
- *(llm,agent)* prompt caching on Anthropic and compaction to a low-water mark
- Merge pull request #20 from StephanMeijer/chore/deps-15
- Merge pull request #74 from StephanMeijer/perf/cache-and-compaction
- Merge pull request #71 from StephanMeijer/fix/refusal-not-clean
- Merge pull request #28 from StephanMeijer/perf/compact-keep-diffs
- *(deps)* drop the dependencies cargo machete reports as unused
- *(session)* say what finished means for a lane row at the time limit
- Merge pull request #19 from StephanMeijer/chore/remove-running-review
- *(store)* joining a run records one request per join
- *(store)* remove the unused RunStore::running_review

## [0.1.3](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.2...v0.1.3) - 2026-10-06

### Added

- *(dashboard)* a read-only web dashboard behind GitHub sign-in
- *(skills)* operator-configured SKILL.md for lanes, the planner and the fact-checker
- *(domain)* workspace policy and changeset rules
- *(plan)* plan GitLab work items: type, hierarchy, links and triage fields
- *(address)* address review feedback on GitHub pull requests
- *(review)* review.ignore leaves lockfiles and generated files out
- *(marker)* name the fact-checking model in the hidden marker
- *(review)* fact-check findings with a second model, and withdraw wrong ones
- *(domain)* hidden notes for AI agents on every marked comment
- *(domain)* parse pull request diffs per file
- *(review)* a lane that reaches its time limit stops instead of failing
- *(events)* add henk-events with the event model, parsers, bus and local record
- *(llm)* effort and adaptive thinking for Anthropic models
- *(llm)* list the models an endpoint serves
- *(agent)* warn lanes three turns before the turn limit
- *(review)* get_file_diff reads several files per call; the file list opens the lane
- *(prompts)* review file by file with the lane's own diff tools
- *(agent)* continuations, one nudge before a lane ends
- *(agent)* keep the conversation under a size budget
- *(review)* hand out the diff per file and validate findings against it
- *(agent)* log each tool call and the final text at debug level
- *(store)* PostgreSQL as a run-store backend
- *(review)* withhold code search when the repository is not indexed
- *(session)* write a local transcript when HENK_TRANSCRIPT_DIR is set
- *(session)* add henk-session, one way to run a model session
- *(platform)* fetch the diff of a review at its commit
- *(github)* check the App identity and list installations in doctor
- *(cli)* add runs show and print the run id before a review starts

### Fixed

- *(dashboard)* trace the dashboard routes once, not the whole router again
- *(dashboard)* set workspace_provider in the test fixture
- *(dashboard)* set live_runs and test_issue_writer in the test fixture
- *(dashboard)* count every running run, not one page of them
- *(dashboard)* judge GitHub addresses by their parsed host
- *(diff)* unquote git's C-quoted paths
- *(review)* an interrupted review ends, and orphaned runs are reaped
- *(review)* a superseded review ends neutral and posts no failure
- *(review)* a withdrawn finding keeps its author and stops asking for action
- *(scope)* refuse another repository instead of swapping it in
- *(scope)* GitLab lanes do not get whole-diff tools either
- *(scope)* pin reads to the reviewed commit and refuse whole-diff reads
- *(agent)* a model refusal ends a run as a refusal, not a clean review
- *(llm)* survive first contact with OpenAI-compatible proxies
- *(agent)* compaction drops thinking, hints for guessed tool names, no <think> on the run page
- *(agent)* keep debug log fields on one line
- *(prompts)* name the diff tools in the review lane prompt
- *(gitlab)* page repo_labels through a shared helper
- *(plan)* refuse a sub-issue under a GitLab task before spending budget
- *(store)* refuse numbers beyond i64 on SQLite too

### Other

- Merge pull request #77 from StephanMeijer/fix/emoji-ranges
- Rust toolchain 1.93 -> 1.95
- initial commit
- *(llm,agent)* prompt caching on Anthropic and compaction to a low-water mark
- Merge pull request #20 from StephanMeijer/chore/deps-15
- Merge pull request #74 from StephanMeijer/perf/cache-and-compaction
- Merge pull request #71 from StephanMeijer/fix/refusal-not-clean
- Merge pull request #28 from StephanMeijer/perf/compact-keep-diffs
- *(deps)* drop the dependencies cargo machete reports as unused
- *(session)* say what finished means for a lane row at the time limit
- Merge pull request #19 from StephanMeijer/chore/remove-running-review
- *(store)* joining a run records one request per join
- *(store)* remove the unused RunStore::running_review

## [0.1.2](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.1...v0.1.2) - 2026-10-06

### Added

- *(skills)* operator-configured SKILL.md for lanes, the planner and the fact-checker
- *(domain)* workspace policy and changeset rules
- *(plan)* plan GitLab work items: type, hierarchy, links and triage fields
- *(address)* address review feedback on GitHub pull requests
- *(review)* review.ignore leaves lockfiles and generated files out
- *(marker)* name the fact-checking model in the hidden marker
- *(review)* fact-check findings with a second model, and withdraw wrong ones
- *(domain)* hidden notes for AI agents on every marked comment
- *(domain)* parse pull request diffs per file
- *(review)* a lane that reaches its time limit stops instead of failing
- *(events)* add henk-events with the event model, parsers, bus and local record
- *(llm)* effort and adaptive thinking for Anthropic models
- *(llm)* list the models an endpoint serves
- *(agent)* warn lanes three turns before the turn limit
- *(review)* get_file_diff reads several files per call; the file list opens the lane
- *(prompts)* review file by file with the lane's own diff tools
- *(agent)* continuations, one nudge before a lane ends
- *(agent)* keep the conversation under a size budget
- *(review)* hand out the diff per file and validate findings against it
- *(agent)* log each tool call and the final text at debug level
- *(store)* PostgreSQL as a run-store backend
- *(review)* withhold code search when the repository is not indexed
- *(session)* write a local transcript when HENK_TRANSCRIPT_DIR is set
- *(session)* add henk-session, one way to run a model session
- *(platform)* fetch the diff of a review at its commit
- *(github)* check the App identity and list installations in doctor
- *(cli)* add runs show and print the run id before a review starts

### Fixed

- *(skills)* refuse a SKILL.md that is a symbolic link
- *(skills)* list links in a skill folder without following them
- *(review)* an interrupted review ends, and orphaned runs are reaped
- *(review)* a superseded review ends neutral and posts no failure
- *(review)* a withdrawn finding keeps its author and stops asking for action
- *(scope)* refuse another repository instead of swapping it in
- *(scope)* GitLab lanes do not get whole-diff tools either
- *(scope)* pin reads to the reviewed commit and refuse whole-diff reads
- *(agent)* a model refusal ends a run as a refusal, not a clean review
- *(llm)* survive first contact with OpenAI-compatible proxies
- *(agent)* compaction drops thinking, hints for guessed tool names, no <think> on the run page
- *(agent)* keep debug log fields on one line
- *(prompts)* name the diff tools in the review lane prompt
- *(plan)* refuse a sub-issue under a GitLab task before spending budget
- *(store)* refuse numbers beyond i64 on SQLite too

### Other

- Merge pull request #77 from StephanMeijer/fix/emoji-ranges
- Rust toolchain 1.93 -> 1.95
- initial commit
- Merge pull request #20 from StephanMeijer/chore/deps-15
- Merge pull request #71 from StephanMeijer/fix/refusal-not-clean
- Merge pull request #28 from StephanMeijer/perf/compact-keep-diffs
- *(deps)* drop the dependencies cargo machete reports as unused
- *(session)* say what finished means for a lane row at the time limit
- Merge pull request #19 from StephanMeijer/chore/remove-running-review
- *(store)* joining a run records one request per join
- *(store)* remove the unused RunStore::running_review

## [0.1.1](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.0...v0.1.1) - 2026-10-06

### Added

- *(address)* export a changeset and push from a clean checkout
- *(workspace)* Workspace trait with host and fake backends, exec trace
- *(config)* [workspace] section
- *(plan)* plan GitLab work items: type, hierarchy, links and triage fields
- *(store)* PostgreSQL as a run-store backend
- *(review)* get_file_diff reads several files per call; the file list opens the lane
- *(marker)* name the fact-checking model in the hidden marker
- *(review)* fact-check findings with a second model, and withdraw wrong ones
- *(domain)* workspace policy and changeset rules
- *(address)* address review feedback on GitHub pull requests
- *(review)* review.ignore leaves lockfiles and generated files out
- *(domain)* hidden notes for AI agents on every marked comment
- *(domain)* parse pull request diffs per file
- *(review)* a lane that reaches its time limit stops instead of failing
- *(events)* add henk-events with the event model, parsers, bus and local record
- *(llm)* effort and adaptive thinking for Anthropic models
- *(llm)* list the models an endpoint serves
- *(agent)* warn lanes three turns before the turn limit
- *(prompts)* review file by file with the lane's own diff tools
- *(agent)* continuations, one nudge before a lane ends
- *(agent)* keep the conversation under a size budget
- *(review)* hand out the diff per file and validate findings against it
- *(agent)* log each tool call and the final text at debug level
- *(review)* withhold code search when the repository is not indexed
- *(session)* write a local transcript when HENK_TRANSCRIPT_DIR is set
- *(session)* add henk-session, one way to run a model session
- *(platform)* fetch the diff of a review at its commit
- *(github)* check the App identity and list installations in doctor
- *(cli)* add runs show and print the run id before a review starts

### Fixed

- *(address)* register address runs with the live-run set
- *(plan)* refuse a sub-issue under a GitLab task before spending budget
- *(address)* a model refusal fails an address run
- *(agent)* a model refusal ends a run as a refusal, not a clean review
- *(review)* an interrupted review ends, and orphaned runs are reaped
- *(review)* a superseded review ends neutral and posts no failure
- *(review)* a withdrawn finding keeps its author and stops asking for action
- *(scope)* refuse another repository instead of swapping it in
- *(scope)* GitLab lanes do not get whole-diff tools either
- *(scope)* pin reads to the reviewed commit and refuse whole-diff reads
- *(llm)* survive first contact with OpenAI-compatible proxies
- *(agent)* compaction drops thinking, hints for guessed tool names, no <think> on the run page
- *(agent)* keep debug log fields on one line
- *(prompts)* name the diff tools in the review lane prompt
- *(store)* refuse numbers beyond i64 on SQLite too

### Other

- *(address)* tools and checks on the Workspace interface
- Merge pull request #75 from StephanMeijer/fix/periodic-reaper
- Merge pull request #71 from StephanMeijer/fix/refusal-not-clean
- Merge pull request #28 from StephanMeijer/perf/compact-keep-diffs
- Merge pull request #29 from StephanMeijer/fix/withdraw-provenance
- Merge pull request #23 from StephanMeijer/feat/turn-warning
- Merge pull request #27 from StephanMeijer/feat/review-many
- *(review)* run_review end to end on fakes
- Merge pull request #24 from StephanMeijer/feat/review-ignore-reland
- Merge pull request #21 from StephanMeijer/refactor/review-context
- Merge pull request #20 from StephanMeijer/chore/deps-15
- *(deps)* drop rand; ids use ring's SystemRandom
- Rust toolchain 1.93 -> 1.95
- Merge pull request #77 from StephanMeijer/fix/emoji-ranges
- initial commit
- *(deps)* drop the dependencies cargo machete reports as unused
- *(session)* say what finished means for a lane row at the time limit
- Merge pull request #19 from StephanMeijer/chore/remove-running-review
- *(store)* joining a run records one request per join
- *(store)* remove the unused RunStore::running_review
