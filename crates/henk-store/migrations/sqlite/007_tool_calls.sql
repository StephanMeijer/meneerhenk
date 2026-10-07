-- Every tool call of every session (#190): what a lane, check, planner or
-- address session called, how it ended and what it cost, for the run record
-- and for metrics across runs. Kept with the run, as lanes and findings are.
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
CREATE INDEX tool_calls_run ON tool_calls(run_id, session);
CREATE INDEX tool_calls_at ON tool_calls(at);
