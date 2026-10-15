-- A running process refreshes heartbeat_at; a run whose heartbeat stopped
-- was left by a process that died, and is closed on the next start (#7).
ALTER TABLE runs ADD COLUMN heartbeat_at TEXT;
-- The platform's id for the review's check, so a reaped run can close it.
ALTER TABLE runs ADD COLUMN check_id TEXT;
