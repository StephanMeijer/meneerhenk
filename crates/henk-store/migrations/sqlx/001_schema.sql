-- Step 1 of the one sequence: the schema SQLite 001-019 and PostgreSQL
-- 001-015 end in, written out whole (the history is not replayed: a database
-- that has it is adopted, see src/candidate.rs).
--
-- A step is one file with a section per dialect. `-- @both` is run on both,
-- `-- @sqlite` and `-- @postgres` on one. A step with no section for a
-- dialect is not run there and leaves a hole in that database's numbering.

-- @sqlite
CREATE TABLE runs (
    id            TEXT PRIMARY KEY,
    kind          TEXT NOT NULL,
    platform      TEXT NOT NULL,
    repo          TEXT NOT NULL,
    target        INTEGER NOT NULL,
    commit_sha    TEXT,
    requester     TEXT,
    trigger       TEXT NOT NULL,
    status        TEXT NOT NULL,
    started_at    TEXT NOT NULL,
    finished_at   TEXT,
    link          TEXT NOT NULL,
    summary       TEXT,
    error         TEXT,
    heartbeat_at  TEXT,
    check_id      TEXT,
    superseded_by TEXT,
    loop_stop     TEXT,
    loop_rounds   INTEGER
);

CREATE TABLE sessions (
    run_id             TEXT NOT NULL REFERENCES runs(id),
    name               TEXT NOT NULL,
    model              TEXT NOT NULL,
    status             TEXT NOT NULL,
    turns              INTEGER NOT NULL DEFAULT 0,
    input_tokens       INTEGER NOT NULL DEFAULT 0,
    output_tokens      INTEGER NOT NULL DEFAULT 0,
    started_at         TEXT NOT NULL,
    finished_at        TEXT,
    error              TEXT,
    cache_read_tokens  INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    role               TEXT NOT NULL DEFAULT 'lane',
    reasoning_tokens   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (run_id, name)
);

CREATE TABLE findings (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id     TEXT NOT NULL REFERENCES runs(id),
    session    TEXT NOT NULL,
    path       TEXT NOT NULL,
    line       INTEGER NOT NULL,
    comment_id TEXT NOT NULL,
    action     TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE events (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id  TEXT NOT NULL REFERENCES runs(id),
    at      TEXT NOT NULL,
    level   TEXT NOT NULL,
    message TEXT NOT NULL
);

CREATE TABLE requests (
    run_id TEXT NOT NULL REFERENCES runs(id),
    at     TEXT NOT NULL,
    source TEXT NOT NULL
);

CREATE TABLE inbound_events (
    id          TEXT PRIMARY KEY,
    received_at TEXT NOT NULL,
    source      TEXT NOT NULL,
    kind        TEXT NOT NULL,
    repo        TEXT,
    target      INTEGER,
    payload     TEXT,
    requester   TEXT
);

CREATE TABLE event_outcomes (
    event_id TEXT NOT NULL REFERENCES inbound_events(id),
    listener TEXT NOT NULL,
    outcome  TEXT NOT NULL,
    detail   TEXT NOT NULL,
    run_id   TEXT,
    at       TEXT NOT NULL
);

CREATE TABLE tool_calls (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id        TEXT NOT NULL REFERENCES runs(id),
    session       TEXT NOT NULL,
    model         TEXT NOT NULL,
    turn          INTEGER NOT NULL,
    tool          TEXT NOT NULL,
    origin        TEXT NOT NULL,
    outcome       TEXT NOT NULL,
    arguments     TEXT NOT NULL,
    arguments_len INTEGER NOT NULL,
    result_chars  INTEGER NOT NULL,
    elapsed_ms    INTEGER NOT NULL,
    at            TEXT NOT NULL
);

CREATE TABLE transcripts (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id      TEXT NOT NULL REFERENCES runs(id),
    session     TEXT NOT NULL,
    model       TEXT NOT NULL,
    stop        TEXT NOT NULL,
    turns       INTEGER NOT NULL,
    bytes       INTEGER NOT NULL,
    body        TEXT NOT NULL,
    recorded_at TEXT NOT NULL
);

CREATE TABLE drafts (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id     TEXT NOT NULL REFERENCES runs(id),
    draft      TEXT NOT NULL,
    session    TEXT NOT NULL,
    model      TEXT NOT NULL,
    kind       TEXT NOT NULL,
    path       TEXT NOT NULL,
    line       INTEGER NOT NULL,
    target     TEXT NOT NULL,
    body       TEXT NOT NULL,
    created_at TEXT NOT NULL,
    verdict    TEXT,
    checker    TEXT NOT NULL DEFAULT '',
    reason     TEXT NOT NULL DEFAULT '',
    same_as    TEXT NOT NULL DEFAULT '',
    comment_id TEXT NOT NULL DEFAULT '',
    decided_at TEXT
);

CREATE TABLE stages (
    run_id     TEXT NOT NULL REFERENCES runs(id),
    stage      TEXT NOT NULL,
    state      TEXT NOT NULL,
    started_at TEXT NOT NULL,
    ended_at   TEXT,
    detail     TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (run_id, stage)
);

CREATE TABLE session_rounds (
    run_id       TEXT NOT NULL REFERENCES runs(id),
    session      TEXT NOT NULL,
    role         TEXT NOT NULL,
    round        INTEGER NOT NULL,
    started_at   TEXT NOT NULL,
    ended_at     TEXT,
    first_turn   INTEGER NOT NULL,
    turns        INTEGER NOT NULL DEFAULT 0,
    handover     TEXT NOT NULL,
    handover_len INTEGER NOT NULL,
    PRIMARY KEY (run_id, session, round)
);

CREATE INDEX runs_repo_target ON runs(platform, repo, target, started_at);
CREATE INDEX runs_started ON runs(started_at);
CREATE INDEX runs_status ON runs(status, started_at);
CREATE INDEX event_outcomes_run ON event_outcomes(run_id);
CREATE INDEX event_outcomes_event ON event_outcomes(event_id);
CREATE INDEX inbound_events_received ON inbound_events(received_at);
CREATE INDEX tool_calls_run ON tool_calls(run_id, session);
CREATE INDEX tool_calls_at ON tool_calls(at);
CREATE INDEX transcripts_run ON transcripts(run_id, session);
CREATE INDEX transcripts_recorded ON transcripts(recorded_at);
CREATE UNIQUE INDEX drafts_run ON drafts(run_id, draft);
CREATE INDEX findings_created ON findings(created_at);
CREATE INDEX drafts_created ON drafts(created_at);

-- @postgres
CREATE TABLE runs (
    id            TEXT PRIMARY KEY,
    kind          TEXT NOT NULL,
    platform      TEXT NOT NULL,
    repo          TEXT NOT NULL,
    target        BIGINT NOT NULL,
    commit_sha    TEXT,
    requester     TEXT,
    trigger       TEXT NOT NULL,
    status        TEXT NOT NULL,
    started_at    TIMESTAMPTZ NOT NULL,
    finished_at   TIMESTAMPTZ,
    link          TEXT NOT NULL,
    summary       TEXT,
    error         TEXT,
    heartbeat_at  TIMESTAMPTZ,
    check_id      TEXT,
    superseded_by TEXT,
    loop_stop     TEXT,
    loop_rounds   INTEGER
);

CREATE TABLE sessions (
    run_id             TEXT NOT NULL REFERENCES runs(id),
    name               TEXT NOT NULL,
    model              TEXT NOT NULL,
    status             TEXT NOT NULL,
    turns              BIGINT NOT NULL DEFAULT 0,
    input_tokens       BIGINT NOT NULL DEFAULT 0,
    output_tokens      BIGINT NOT NULL DEFAULT 0,
    started_at         TIMESTAMPTZ NOT NULL,
    finished_at        TIMESTAMPTZ,
    error              TEXT,
    cache_read_tokens  BIGINT NOT NULL DEFAULT 0,
    cache_write_tokens BIGINT NOT NULL DEFAULT 0,
    role               TEXT NOT NULL,
    reasoning_tokens   BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (run_id, name)
);

CREATE TABLE findings (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    run_id     TEXT NOT NULL REFERENCES runs(id),
    session    TEXT NOT NULL,
    path       TEXT NOT NULL,
    line       BIGINT NOT NULL,
    comment_id TEXT NOT NULL,
    action     TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL
);

CREATE TABLE events (
    id      BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    run_id  TEXT NOT NULL REFERENCES runs(id),
    at      TIMESTAMPTZ NOT NULL,
    level   TEXT NOT NULL,
    message TEXT NOT NULL
);

CREATE TABLE requests (
    id     BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES runs(id),
    at     TIMESTAMPTZ NOT NULL,
    source TEXT NOT NULL
);

CREATE TABLE inbound_events (
    id          TEXT PRIMARY KEY,
    received_at TIMESTAMPTZ NOT NULL,
    source      TEXT NOT NULL,
    kind        TEXT NOT NULL,
    repo        TEXT,
    target      BIGINT,
    payload     TEXT,
    requester   TEXT
);

CREATE TABLE event_outcomes (
    id       BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    event_id TEXT NOT NULL REFERENCES inbound_events(id),
    listener TEXT NOT NULL,
    outcome  TEXT NOT NULL,
    detail   TEXT NOT NULL,
    run_id   TEXT,
    at       TIMESTAMPTZ NOT NULL
);

CREATE TABLE tool_calls (
    id            BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    run_id        TEXT NOT NULL REFERENCES runs(id),
    session       TEXT NOT NULL,
    model         TEXT NOT NULL,
    turn          BIGINT NOT NULL,
    tool          TEXT NOT NULL,
    origin        TEXT NOT NULL,
    outcome       TEXT NOT NULL,
    arguments     TEXT NOT NULL,
    arguments_len BIGINT NOT NULL,
    result_chars  BIGINT NOT NULL,
    elapsed_ms    BIGINT NOT NULL,
    at            TIMESTAMPTZ NOT NULL
);

CREATE TABLE transcripts (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    run_id      TEXT NOT NULL REFERENCES runs(id),
    session     TEXT NOT NULL,
    model       TEXT NOT NULL,
    stop        TEXT NOT NULL,
    turns       BIGINT NOT NULL,
    bytes       BIGINT NOT NULL,
    body        TEXT NOT NULL,
    recorded_at TIMESTAMPTZ NOT NULL
);

CREATE TABLE drafts (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    run_id     TEXT NOT NULL REFERENCES runs(id),
    draft      TEXT NOT NULL,
    session    TEXT NOT NULL,
    model      TEXT NOT NULL,
    kind       TEXT NOT NULL,
    path       TEXT NOT NULL,
    line       BIGINT NOT NULL,
    target     TEXT NOT NULL,
    body       TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL,
    verdict    TEXT,
    checker    TEXT NOT NULL DEFAULT '',
    reason     TEXT NOT NULL DEFAULT '',
    same_as    TEXT NOT NULL DEFAULT '',
    comment_id TEXT NOT NULL DEFAULT '',
    decided_at TIMESTAMPTZ
);

CREATE TABLE stages (
    run_id     TEXT NOT NULL REFERENCES runs(id),
    stage      TEXT NOT NULL,
    state      TEXT NOT NULL,
    started_at TIMESTAMPTZ NOT NULL,
    ended_at   TIMESTAMPTZ,
    detail     TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (run_id, stage)
);

CREATE TABLE session_rounds (
    run_id       TEXT NOT NULL REFERENCES runs(id),
    session      TEXT NOT NULL,
    role         TEXT NOT NULL,
    round        BIGINT NOT NULL,
    started_at   TIMESTAMPTZ NOT NULL,
    ended_at     TIMESTAMPTZ,
    first_turn   BIGINT NOT NULL,
    turns        BIGINT NOT NULL DEFAULT 0,
    handover     TEXT NOT NULL,
    handover_len BIGINT NOT NULL,
    PRIMARY KEY (run_id, session, round)
);

CREATE INDEX runs_repo_target ON runs(platform, repo, target, started_at);
CREATE INDEX runs_started ON runs(started_at);
CREATE INDEX runs_status ON runs(status, started_at);
CREATE INDEX event_outcomes_run ON event_outcomes(run_id);
CREATE INDEX event_outcomes_event ON event_outcomes(event_id);
CREATE INDEX inbound_events_received ON inbound_events(received_at);
CREATE INDEX sessions_run ON sessions(run_id);
CREATE INDEX findings_run ON findings(run_id);
CREATE INDEX events_run ON events(run_id);
CREATE INDEX tool_calls_run ON tool_calls(run_id, session);
CREATE INDEX tool_calls_at ON tool_calls(at);
CREATE INDEX transcripts_run ON transcripts(run_id, session);
CREATE INDEX transcripts_recorded ON transcripts(recorded_at);
CREATE UNIQUE INDEX drafts_run ON drafts(run_id, draft);
CREATE INDEX findings_created ON findings(created_at);
CREATE INDEX drafts_created ON drafts(created_at);
