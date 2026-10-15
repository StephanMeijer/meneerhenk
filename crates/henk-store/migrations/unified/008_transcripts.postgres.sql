-- Every model session's whole conversation (#191): the system prompt, the
-- messages with their tool calls and results, how it stopped. Stored whole,
-- never cut. Pruned with inbound events (server.keep_events_days); the run,
-- its lanes, findings and tool calls are kept.
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
CREATE INDEX transcripts_run ON transcripts(run_id, session);
CREATE INDEX transcripts_recorded ON transcripts(recorded_at);
