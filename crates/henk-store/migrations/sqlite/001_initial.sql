CREATE TABLE runs (
    id          TEXT PRIMARY KEY,
    kind        TEXT NOT NULL,
    platform    TEXT NOT NULL,
    repo        TEXT NOT NULL,
    target      INTEGER NOT NULL,
    commit_sha  TEXT,
    requester   TEXT,
    trigger     TEXT NOT NULL,
    status      TEXT NOT NULL,
    started_at  TEXT NOT NULL,
    finished_at TEXT,
    link        TEXT NOT NULL,
    summary     TEXT,
    error       TEXT
);

CREATE TABLE lanes (
    run_id        TEXT NOT NULL REFERENCES runs(id),
    name          TEXT NOT NULL,
    model         TEXT NOT NULL,
    status        TEXT NOT NULL,
    turns         INTEGER NOT NULL DEFAULT 0,
    input_tokens  INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    started_at    TEXT NOT NULL,
    finished_at   TEXT,
    error         TEXT,
    PRIMARY KEY (run_id, name)
);

CREATE TABLE findings (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id     TEXT NOT NULL REFERENCES runs(id),
    lane       TEXT NOT NULL,
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

CREATE INDEX runs_repo_target ON runs(platform, repo, target, started_at);
