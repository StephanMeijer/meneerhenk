-- Each round of a session in a pairing (#374): when the role was resumed,
-- with what, and how many turns it took. One-shot sessions have none.
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
