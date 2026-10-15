-- What review lanes wanted written (#189): each draft as the lane queued
-- it, and, once the fact-check has judged all of them after the lanes, its
-- verdict, the model that gave it, what it repeats and the comment written.
-- Kept with the run, like lanes and findings.
CREATE TABLE drafts (
    id         BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    run_id     TEXT NOT NULL REFERENCES runs(id),
    draft      TEXT NOT NULL,
    lane       TEXT NOT NULL,
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
CREATE UNIQUE INDEX drafts_run ON drafts(run_id, draft);
