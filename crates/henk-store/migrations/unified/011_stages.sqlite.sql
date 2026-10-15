-- The stages of a run (#226): requested, queued, started, diff, checkout,
-- lanes, fact check, publish, done for a review; fewer for a plan or an
-- address run. One row per stage, updated as the run moves on.
CREATE TABLE stages (
    run_id     TEXT NOT NULL REFERENCES runs(id),
    stage      TEXT NOT NULL,
    state      TEXT NOT NULL,
    started_at TEXT NOT NULL,
    ended_at   TEXT,
    detail     TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (run_id, stage)
);
