-- The SQLite schema of migrations 001 to 003 in one step, with native
-- types: timestamps are TIMESTAMPTZ, numbers BIGINT, and every table a
-- recording order is read from has an identity column (SQLite's rowid).

CREATE TABLE runs (
    id           TEXT PRIMARY KEY,
    kind         TEXT NOT NULL,
    platform     TEXT NOT NULL,
    repo         TEXT NOT NULL,
    target       BIGINT NOT NULL,
    commit_sha   TEXT,
    requester    TEXT,
    trigger      TEXT NOT NULL,
    status       TEXT NOT NULL,
    started_at   TIMESTAMPTZ NOT NULL,
    finished_at  TIMESTAMPTZ,
    link         TEXT NOT NULL,
    summary      TEXT,
    error        TEXT,
    -- A running process refreshes heartbeat_at; a run whose heartbeat
    -- stopped was left by a process that died (#7).
    heartbeat_at TIMESTAMPTZ,
    -- The platform's id for the review's check, so a reaped run can close it.
    check_id     TEXT
);

CREATE TABLE lanes (
    run_id        TEXT NOT NULL REFERENCES runs(id),
    name          TEXT NOT NULL,
    model         TEXT NOT NULL,
    status        TEXT NOT NULL,
    turns         BIGINT NOT NULL DEFAULT 0,
    input_tokens  BIGINT NOT NULL DEFAULT 0,
    output_tokens BIGINT NOT NULL DEFAULT 0,
    started_at    TIMESTAMPTZ NOT NULL,
    finished_at   TIMESTAMPTZ,
    error         TEXT,
    PRIMARY KEY (run_id, name)
);

CREATE TABLE findings (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    run_id     TEXT NOT NULL REFERENCES runs(id),
    lane       TEXT NOT NULL,
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

CREATE INDEX runs_repo_target ON runs(platform, repo, target, started_at);

CREATE TABLE inbound_events (
    id          TEXT PRIMARY KEY,
    received_at TIMESTAMPTZ NOT NULL,
    source      TEXT NOT NULL,
    kind        TEXT NOT NULL,
    repo        TEXT,
    target      BIGINT,
    payload     TEXT
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

CREATE INDEX event_outcomes_run ON event_outcomes(run_id);
CREATE INDEX event_outcomes_event ON event_outcomes(event_id);
CREATE INDEX inbound_events_received ON inbound_events(received_at);
CREATE INDEX lanes_run ON lanes(run_id);
CREATE INDEX findings_run ON findings(run_id);
CREATE INDEX events_run ON events(run_id);
