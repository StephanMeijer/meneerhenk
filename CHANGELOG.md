# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.0...v0.1.1) - 2026-10-06

### Added

- *(address)* address review feedback on GitHub pull requests
- *(store)* PostgreSQL as a run-store backend
- *(review)* get_file_diff reads several files per call; the file list opens the lane
- *(marker)* name the fact-checking model in the hidden marker
- *(review)* fact-check findings with a second model, and withdraw wrong ones
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
- initial commit
- *(deps)* drop the dependencies cargo machete reports as unused
- *(session)* say what finished means for a lane row at the time limit
- Merge pull request #19 from StephanMeijer/chore/remove-running-review
- *(store)* joining a run records one request per join
- *(store)* remove the unused RunStore::running_review
