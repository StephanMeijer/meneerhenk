CREATE TABLE inbound_events (
    id          TEXT PRIMARY KEY,
    received_at TEXT NOT NULL,
    source      TEXT NOT NULL,
    kind        TEXT NOT NULL,
    repo        TEXT,
    target      INTEGER,
    payload     TEXT
);

CREATE TABLE event_outcomes (
    event_id TEXT NOT NULL REFERENCES inbound_events(id),
    listener TEXT NOT NULL,
    outcome  TEXT NOT NULL,
    detail   TEXT NOT NULL,
    run_id   TEXT,
    at       TEXT NOT NULL
);

CREATE INDEX event_outcomes_run ON event_outcomes(run_id);
CREATE INDEX inbound_events_received ON inbound_events(received_at);
