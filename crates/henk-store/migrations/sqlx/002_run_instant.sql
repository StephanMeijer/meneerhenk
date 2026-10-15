-- Step 2 (SQLite 013): the run listing orders and pages by started_at as an
-- instant (#216), not as text. SQLite uses an expression index only when the
-- query's expression is the same, so this is `instant_of` in
-- src/candidate.rs written out, and a test there checks that the listing's
-- plan uses it. PostgreSQL compares timestamps and needs nothing: this step
-- has no section for it, so its numbering has a hole at 2.

-- @sqlite
CREATE INDEX runs_started_instant ON runs(
    substr(started_at, 1, 19) || substr(CASE WHEN substr(started_at, 20, 1) = '.' THEN substr(started_at, 21, length(started_at) - 21) ELSE '' END || '000000000', 1, 9),
    id
);
