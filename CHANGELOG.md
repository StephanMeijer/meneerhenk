# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.1](https://github.com/StephanMeijer/meneerhenk/compare/v0.1.0...v0.1.1) - 2026-10-05

### Added

- *(domain)* hidden notes for AI agents on every marked comment
- *(domain)* parse pull request diffs per file
- *(review)* a lane that reaches its time limit stops instead of failing
- *(events)* add henk-events with the event model, parsers, bus and local record
- *(llm)* list the models an endpoint serves
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

- *(scope)* GitLab lanes do not get whole-diff tools either
- *(scope)* pin reads to the reviewed commit and refuse whole-diff reads
- *(llm)* survive first contact with OpenAI-compatible proxies
- *(agent)* keep debug log fields on one line
- *(prompts)* name the diff tools in the review lane prompt

### Other

- initial commit
- *(deps)* drop the dependencies cargo machete reports as unused
- *(session)* say what finished means for a lane row at the time limit
