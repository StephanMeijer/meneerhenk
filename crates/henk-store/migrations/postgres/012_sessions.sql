-- Sessions with a role, not lanes (#324). The lanes table keeps every
-- session of a run; it is now named for that, and each row says the role
-- that ran it. New runs write planner, reviewer or executor; older rows are
-- read from their names: fixer (before #323) is the executor, check-N a
-- fact-check, address an address run, and any other name a review lane.
ALTER TABLE lanes RENAME TO sessions;
ALTER INDEX lanes_run RENAME TO sessions_run;
ALTER TABLE sessions ADD COLUMN role TEXT NOT NULL DEFAULT 'lane';
UPDATE sessions SET role = CASE
    WHEN name = 'reviewer' THEN 'reviewer'
    WHEN name IN ('executor', 'fixer') THEN 'executor'
    WHEN name = 'planner' THEN 'planner'
    WHEN name = 'address' THEN 'address'
    WHEN name LIKE 'check-%' THEN 'check'
    ELSE 'lane'
END;
ALTER TABLE sessions ALTER COLUMN role DROP DEFAULT;
ALTER TABLE drafts RENAME COLUMN lane TO session;
ALTER TABLE findings RENAME COLUMN lane TO session;
