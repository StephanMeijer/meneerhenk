-- The dashboard lists runs newest first and by status (#36).
CREATE INDEX runs_started ON runs(started_at);
CREATE INDEX runs_status ON runs(status, started_at);
