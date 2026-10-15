-- The rows the golden fixtures (tests/golden.rs) are read from, for both
-- SQLite and PostgreSQL: plain INSERTs with explicit columns, fixed RFC 3339
-- times and no ids. PostgreSQL's identity columns are GENERATED ALWAYS and
-- refuse an id, so the order of the INSERTs is the recording order on both.
--
-- It is written against the schema SQLite 020 and PostgreSQL 016 end in. A
-- migration that changes a table these rows go into changes this file in
-- the same pull request, and the goldens are blessed again
-- (HENK_BLESS=1 cargo test -p henk-store --test golden).

INSERT INTO runs (id, kind, platform, repo, target, commit_sha, requester, trigger, status, started_at, finished_at, link, summary, error, heartbeat_at, check_id, superseded_by, loop_stop, loop_rounds) VALUES
 -- Three reviews of o/r#7 within one second, and one a second later:
 -- equal seconds break on the id, and ".4Z" is after "Z" as a time (#216).
 ('r-001', 'review', 'github', 'o/r', 7, 'aaaaaaa1', NULL, 'opened', 'finished', '2026-10-01T10:00:00Z', '2026-10-01T10:05:00Z', 'https://henk.example/runs/r-001', 'two findings', NULL, '2026-10-01T10:04:00Z', 'chk-1', NULL, NULL, NULL),
 ('r-002', 'review', 'github', 'o/r', 7, 'aaaaaaa2', 'github:42', 'synchronize', 'failed', '2026-10-01T10:00:00Z', '2026-10-01T10:02:00Z', 'https://henk.example/runs/r-002', NULL, 'the model gave up', '2026-10-01T10:01:30Z', NULL, NULL, NULL, NULL),
 ('r-003', 'review', 'github', 'o/r', 7, 'aaaaaaa3', 'github:42', 'dashboard', 'finished', '2026-10-01T10:00:00.4Z', '2026-10-01T10:20:00Z', 'https://henk.example/runs/r-003', 'clean after two rounds', NULL, '2026-10-01T10:19:00Z', 'chk-3', NULL, 'converged', 2),
 ('r-004', 'review', 'github', 'o/r', 8, 'bbbbbbb1', NULL, 'opened', 'running', '2026-10-01T10:00:00.123456789Z', NULL, 'https://henk.example/runs/r-004', NULL, NULL, '2026-10-01T10:00:30Z', 'chk-4', NULL, NULL, NULL),
 ('r-005', 'review', 'github', 'o/r', 8, 'bbbbbbb2', NULL, 'synchronize', 'superseded', '2026-10-01T10:00:01Z', '2026-10-01T10:00:02Z', 'https://henk.example/runs/r-005', NULL, 'Superseded by a newer commit.', NULL, 'chk-5', NULL, NULL, NULL),
 ('r-006', 'review', 'github', 'o/r', 8, 'bbbbbbb3', NULL, 'synchronize', 'finished', '2026-10-01T10:00:02Z', '2026-10-01T10:09:00Z', 'https://henk.example/runs/r-006', 'one finding', NULL, '2026-10-01T10:08:00Z', 'chk-6', NULL, NULL, NULL),
 -- Issue runs on o/r#9, a rebase of the pull request it opened, and a
 -- ticket run owning three of them.
 ('r-007', 'plan', 'github', 'o/r', 9, NULL, 'github:42', 'dashboard', 'finished', '2026-10-02T09:00:00Z', '2026-10-02T09:20:00Z', 'https://henk.example/runs/r-007', 'plan written', NULL, '2026-10-02T09:19:00Z', NULL, NULL, 'converged', 3),
 ('r-008', 'execute', 'github', 'o/r', 9, NULL, 'github:42', 'dashboard', 'finished', '2026-10-02T09:30:00Z', '2026-10-02T09:50:00Z', 'https://henk.example/runs/r-008', 'pull request #12 opened: https://github.com/o/r/pull/12', NULL, '2026-10-02T09:49:00Z', NULL, NULL, 'converged', 1),
 ('r-009', 'rebase', 'github', 'o/r', 12, 'ccccccc1', NULL, 'rebase command', 'finished', '2026-10-02T10:00:00Z', '2026-10-02T10:03:00Z', 'https://henk.example/runs/r-009', 'rebased onto main', NULL, '2026-10-02T10:02:00Z', NULL, NULL, NULL, NULL),
 ('r-010', 'ticket', 'github', 'o/r', 9, NULL, 'github:42', 'dashboard', 'running', '2026-10-02T11:00:00Z', NULL, 'https://henk.example/runs/r-010', NULL, NULL, '2026-10-02T11:00:05Z', NULL, NULL, NULL, NULL),
 -- A running run that never had a heartbeat.
 ('r-011', 'review', 'github', 'o/r', 10, 'ddddddd1', NULL, 'opened', 'running', '2026-10-02T12:00:00Z', NULL, 'https://henk.example/runs/r-011', NULL, NULL, NULL, NULL, NULL, NULL, NULL),
 -- Another platform and another repository, in the same second.
 ('r-012', 'review', 'gitlab', '9xxlab/tools/cli', 3, 'eeeeeee1', NULL, 'update', 'finished', '2026-10-03T08:00:00Z', '2026-10-03T08:10:00Z', 'https://henk.example/runs/r-012', 'clean', NULL, '2026-10-03T08:09:00Z', NULL, NULL, NULL, NULL),
 ('r-013', 'review', 'github', 'other/repo', 1, 'fffffff1', NULL, 'opened', 'cancelled', '2026-10-03T08:00:00Z', '2026-10-03T08:00:30Z', 'https://henk.example/runs/r-013', NULL, 'Cancelled from the dashboard.', '2026-10-03T08:00:20Z', NULL, NULL, NULL, NULL),
 -- Either side of midnight, for the days.
 ('r-014', 'review', 'github', 'o/r', 7, 'aaaaaaa4', NULL, 'synchronize', 'finished', '2026-10-03T23:59:59Z', '2026-10-04T00:05:00Z', 'https://henk.example/runs/r-014', 'clean', NULL, '2026-10-04T00:04:00Z', NULL, NULL, NULL, NULL),
 ('r-015', 'review', 'github', 'o/r', 7, 'aaaaaaa5', NULL, 'synchronize', 'failed', '2026-10-04T00:00:00Z', '2026-10-04T00:01:00Z', 'https://henk.example/runs/r-015', NULL, 'the model refused', NULL, NULL, NULL, NULL, NULL);

UPDATE runs SET superseded_by = 'r-006' WHERE id = 'r-005';

INSERT INTO run_children (parent, stage, child) VALUES
 ('r-010', 'plan', 'r-007'),
 ('r-010', 'execute', 'r-008'),
 ('r-010', 'review', 'r-006');

-- Sessions: every role and status, tokens and cache counts.
INSERT INTO sessions (run_id, name, model, status, turns, input_tokens, output_tokens, started_at, finished_at, error, cache_read_tokens, cache_write_tokens, role, reasoning_tokens) VALUES
 ('r-001', 'lane-a', 'claude-m', 'finished', 4, 1200, 300, '2026-10-01T10:00:01Z', '2026-10-01T10:03:00Z', NULL, 800, 100, 'lane', 0),
 ('r-001', 'lane-b', 'gpt-m', 'timed_out', 6, 900, 120, '2026-10-01T10:00:01Z', '2026-10-01T10:04:00Z', NULL, 0, 0, 'lane', 50),
 ('r-001', 'check-1', 'claude-m', 'finished', 2, 400, 80, '2026-10-01T10:04:00Z', '2026-10-01T10:04:30Z', NULL, 300, 0, 'check', 0),
 ('r-002', 'lane-a', 'claude-m', 'did_not_finish', 1, 100, 0, '2026-10-01T10:00:01Z', '2026-10-01T10:02:00Z', 'the model gave up', 0, 0, 'lane', 0),
 ('r-003', 'reviewer', 'claude-m', 'finished', 9, 4000, 900, '2026-10-01T10:00:01Z', '2026-10-01T10:19:00Z', NULL, 3000, 500, 'reviewer', 200),
 ('r-003', 'executor', 'claude-m', 'finished', 12, 6000, 1500, '2026-10-01T10:05:00Z', '2026-10-01T10:18:00Z', NULL, 5000, 700, 'executor', 0),
 ('r-004', 'lane-a', 'claude-m', 'running', 0, 0, 0, '2026-10-01T10:00:01Z', NULL, NULL, 0, 0, 'lane', 0),
 ('r-006', 'lane-a', 'claude-m', 'finished', 3, 800, 200, '2026-10-01T10:00:03Z', '2026-10-01T10:08:00Z', NULL, 0, 0, 'lane', 0),
 ('r-007', 'planner', 'claude-m', 'finished', 20, 9000, 2500, '2026-10-02T09:00:01Z', '2026-10-02T09:19:00Z', NULL, 7000, 1000, 'planner', 300),
 ('r-007', 'reviewer', 'claude-m', 'finished', 8, 3000, 700, '2026-10-02T09:05:00Z', '2026-10-02T09:18:00Z', NULL, 2000, 300, 'reviewer', 0),
 ('r-008', 'executor', 'claude-m', 'finished', 30, 20000, 6000, '2026-10-02T09:30:01Z', '2026-10-02T09:49:00Z', NULL, 15000, 2000, 'executor', 0),
 ('r-012', 'lane-a', 'claude-m', 'finished', 2, 500, 100, '2026-10-03T08:00:01Z', '2026-10-03T08:09:00Z', NULL, 0, 0, 'lane', 0),
 ('r-014', 'lane-a', 'claude-m', 'finished', 2, 500, 100, '2026-10-03T23:59:59.5Z', '2026-10-04T00:04:00Z', NULL, 0, 0, 'lane', 0),
 ('r-015', 'lane-a', 'claude-m', 'did_not_finish', 1, 100, 0, '2026-10-04T00:00:01Z', '2026-10-04T00:01:00Z', 'refused', 0, 0, 'lane', 0);

-- Stages, written out of the order they come in.
INSERT INTO stages (run_id, stage, state, started_at, ended_at, detail) VALUES
 ('r-001', 'done', 'done', '2026-10-01T10:05:00Z', '2026-10-01T10:05:00Z', '2 findings'),
 ('r-001', 'requested', 'done', '2026-10-01T10:00:00Z', '2026-10-01T10:00:00Z', 'opened'),
 ('r-001', 'lanes', 'done', '2026-10-01T10:00:02Z', '2026-10-01T10:04:00Z', 'two lanes'),
 ('r-001', 'started', 'done', '2026-10-01T10:00:00Z', '2026-10-01T10:00:01Z', ''),
 ('r-001', 'publish', 'done', '2026-10-01T10:04:30Z', '2026-10-01T10:05:00Z', ''),
 ('r-002', 'requested', 'done', '2026-10-01T10:00:00Z', '2026-10-01T10:00:00Z', 'synchronize'),
 ('r-002', 'lanes', 'failed', '2026-10-01T10:00:02Z', '2026-10-01T10:02:00Z', 'the model gave up'),
 ('r-004', 'requested', 'done', '2026-10-01T10:00:00.123456789Z', '2026-10-01T10:00:00.2Z', ''),
 ('r-004', 'lanes', 'running', '2026-10-01T10:00:02Z', NULL, ''),
 ('r-007', 'requested', 'done', '2026-10-02T09:00:00Z', '2026-10-02T09:00:00Z', 'dashboard'),
 ('r-007', 'session', 'done', '2026-10-02T09:00:01Z', '2026-10-02T09:19:00Z', 'planner and reviewer'),
 ('r-007', 'publish', 'done', '2026-10-02T09:19:30Z', '2026-10-02T09:20:00Z', ''),
 ('r-010', 'requested', 'done', '2026-10-02T11:00:00Z', '2026-10-02T11:00:00Z', ''),
 ('r-010', 'review', 'running', '2026-10-02T11:30:00Z', NULL, 'run r-006'),
 ('r-010', 'plan', 'done', '2026-10-02T11:00:01Z', '2026-10-02T11:20:00Z', 'run r-007'),
 ('r-010', 'execute', 'done', '2026-10-02T11:20:00Z', '2026-10-02T11:30:00Z', 'run r-008'),
 ('r-012', 'requested', 'done', '2026-10-03T08:00:00Z', '2026-10-03T08:00:00Z', 'update');

-- A review loop's rounds: reviewer and executor taking turns.
INSERT INTO session_rounds (run_id, session, role, round, started_at, ended_at, first_turn, turns, handover, handover_len) VALUES
 ('r-003', 'reviewer', 'reviewer', 1, '2026-10-01T10:00:01Z', '2026-10-01T10:05:00Z', 1, 4, 'Review the pull request.', 24),
 ('r-003', 'executor', 'executor', 1, '2026-10-01T10:05:00Z', '2026-10-01T10:12:00Z', 1, 6, 'Round 1. The reviewer found 1 finding.', 38),
 ('r-003', 'reviewer', 'reviewer', 2, '2026-10-01T10:12:00Z', '2026-10-01T10:15:00Z', 5, 3, 'Round 2. The executor answered.', 31),
 ('r-003', 'executor', 'executor', 2, '2026-10-01T10:15:00Z', '2026-10-01T10:18:00Z', 7, 6, 'Round 2. Nothing new.', 21);

INSERT INTO findings (run_id, session, path, line, comment_id, action, created_at) VALUES
 ('r-001', 'lane-a', 'src/a.rs', 4, 'c-1', 'posted', '2026-10-01T10:03:00Z'),
 ('r-001', 'lane-b', 'src/b.rs', 9, 'c-2', 'refused', '2026-10-01T10:04:00Z'),
 ('r-001', 'lane-a', 'src/a.rs', 12, 'c-3', 'rejected', '2026-10-01T10:04:10Z'),
 ('r-006', 'lane-a', 'src/c.rs', 2, 'c-4', 'posted', '2026-10-01T10:08:00Z');

-- Drafts of every kind, decided and not.
INSERT INTO drafts (run_id, draft, session, model, kind, path, line, target, body, created_at, verdict, checker, reason, same_as, comment_id, decided_at) VALUES
 ('r-001', 'd1', 'lane-a', 'claude-m', 'finding', 'src/a.rs', 4, '', 'x is never set.', '2026-10-01T10:02:00Z', 'confirmed', 'check-1', 'it holds', '', 'c-1', '2026-10-01T10:04:00Z'),
 ('r-001', 'd2', 'lane-a', 'claude-m', 'finding', 'src/a.rs', 12, '', 'y leaks.', '2026-10-01T10:02:30Z', 'rejected', 'check-1', 'it does not', '', '', '2026-10-01T10:04:10Z'),
 ('r-001', 'd3', 'lane-b', 'gpt-m', 'rewrite', 'src/b.rs', 9, 'c-9', 'Better text.', '2026-10-01T10:03:00Z', NULL, '', '', '', '', NULL),
 ('r-003', 'f1', 'reviewer', 'claude-m', 'loop_finding', 'src/a.rs', 2, '', 'x must be 3.', '2026-10-01T10:04:00Z', 'fixed', 'executor', 'changed', '', 'abc', '2026-10-01T10:11:00Z'),
 ('r-003', 'f2', 'reviewer', 'claude-m', 'loop_finding', 'src/b.rs', 5, '', 'z is wrong.', '2026-10-01T10:04:10Z', 'rejected', 'executor', 'it holds elsewhere', '', '', '2026-10-01T10:11:10Z'),
 ('r-007', 'p1', 'reviewer', 'claude-m', 'plan_point', 'step 2', 0, '', 'Step 2 misses the migration.', '2026-10-02T09:06:00Z', 'taken', 'planner', 'added', '', '', '2026-10-02T09:10:00Z'),
 ('r-008', 'q1', 'reviewer', 'claude-m', 'requirement_point', 'criterion 2', 0, '', 'No header row.', '2026-10-02T09:40:00Z', 'unsettled', '', 'the review stopped', '', '', '2026-10-02T09:49:00Z');

INSERT INTO events (run_id, at, level, message) VALUES
 ('r-001', '2026-10-01T10:00:00Z', 'info', 'review started'),
 ('r-001', '2026-10-01T10:04:00Z', 'warn', 'lane-b reached its time limit'),
 ('r-001', '2026-10-01T10:05:00Z', 'info', 'review ended'),
 ('r-003', '2026-10-01T10:00:00.4Z', 'info', 'loop started'),
 ('r-007', '2026-10-02T09:00:00Z', 'info', 'plan started');

-- Tool calls over several runs, models and outcomes; one from an invented
-- tool name, counted as unknown.
INSERT INTO tool_calls (run_id, session, model, turn, tool, origin, outcome, arguments, arguments_len, result_chars, elapsed_ms, at) VALUES
 ('r-001', 'lane-a', 'claude-m', 1, 'read_file', 'henk', 'ok', '{"path":"src/a.rs"}', 19, 900, 12, '2026-10-01T10:00:05Z'),
 ('r-001', 'lane-a', 'claude-m', 2, 'search', 'henk', 'ok', '{"pattern":"x"}', 15, 300, 40, '2026-10-01T10:00:10Z'),
 ('r-001', 'lane-a', 'claude-m', 3, 'report_finding', 'henk', 'ok', '{"path":"src/a.rs"}', 19, 10, 3, '2026-10-01T10:02:00Z'),
 ('r-001', 'lane-b', 'gpt-m', 1, 'read_file', 'henk', 'error', '{"path":"nope"}', 15, 40, 5, '2026-10-01T10:00:05Z'),
 ('r-001', 'lane-b', 'gpt-m', 2, 'read_file', 'henk', 'refused_scope', '{"path":"../x"}', 15, 30, 1, '2026-10-01T10:00:05Z'),
 ('r-001', 'lane-b', 'gpt-m', 3, 'Review complete. End.', 'unknown_tool', 'unknown_tool', '', 0, 0, 0, '2026-10-01T10:03:00Z'),
 ('r-003', 'reviewer', 'claude-m', 1, 'read_file', 'henk', 'ok', '{"path":"src/a.rs"}', 19, 900, 11, '2026-10-01T10:00:10Z'),
 ('r-003', 'reviewer', 'claude-m', 2, 'report_finding', 'henk', 'ok', '{"path":"src/a.rs"}', 19, 10, 2, '2026-10-01T10:04:00Z'),
 ('r-003', 'executor', 'claude-m', 1, 'edit_file', 'henk', 'ok', '{"path":"src/a.rs"}', 19, 20, 8, '2026-10-01T10:06:00Z'),
 ('r-003', 'executor', 'claude-m', 2, 'bash', 'henk', 'error', '{"command":"cargo test"}', 24, 400, 9000, '2026-10-01T10:07:00Z'),
 ('r-007', 'planner', 'claude-m', 1, 'github__issue_read', 'mcp:github', 'ok', '{"issue":9}', 11, 2000, 300, '2026-10-02T09:01:00Z'),
 ('r-007', 'planner', 'claude-m', 2, 'write_plan', 'henk', 'ok', '{}', 2, 20, 2, '2026-10-02T09:10:00Z'),
 ('r-008', 'executor', 'claude-m', 1, 'write_file', 'henk', 'ok', '{"path":"src/csv.rs"}', 21, 20, 4, '2026-10-02T09:31:00Z'),
 ('r-008', 'executor', 'claude-m', 2, 'propose_pull_request', 'henk', 'ok', '{}', 2, 30, 6, '2026-10-02T09:48:00Z');

-- Two sessions' conversations kept whole.
INSERT INTO transcripts (run_id, session, model, stop, turns, bytes, body, recorded_at) VALUES
 ('r-001', 'lane-a', 'claude-m', 'end_turn', 4, 12, '[{"role":"user"}]', '2026-10-01T10:03:00Z'),
 ('r-003', 'reviewer', 'claude-m', 'end_turn', 9, 12, '[{"role":"user"}]', '2026-10-01T10:15:00Z'),
 ('r-003', 'reviewer', 'claude-m', 'end_turn', 9, 12, '[{"role":"user"},{"role":"assistant"}]', '2026-10-01T10:19:00Z');

-- Inbound events: ties on the second, sources, kinds and repositories.
INSERT INTO inbound_events (id, received_at, source, kind, repo, target, payload, requester) VALUES
 ('e-001', '2026-10-01T10:00:00Z', 'github_webhook', 'pull_request', 'o/r', 7, '{}', NULL),
 ('e-002', '2026-10-01T10:00:00Z', 'github_webhook', 'comment', 'o/r', 7, '{}', NULL),
 ('e-003', '2026-10-01T10:00:00.4Z', 'github_webhook', 'pull_request', 'o/r', 8, '{}', NULL),
 ('e-004', '2026-10-02T09:00:00Z', 'api', 'plan_requested', 'o/r', 9, '{}', 'github:42'),
 ('e-005', '2026-10-02T09:30:00Z', 'api', 'execute_requested', 'o/r', 9, '{}', 'github:42'),
 ('e-006', '2026-10-02T10:00:00Z', 'github_webhook', 'comment', 'o/r', 12, '{}', NULL),
 ('e-007', '2026-10-03T08:00:00Z', 'gitlab_webhook', 'pull_request', '9xxlab/tools/cli', 3, '{}', NULL),
 ('e-008', '2026-10-03T08:00:00Z', 'github_webhook', 'push', 'o/r', NULL, '{}', NULL);

INSERT INTO event_outcomes (event_id, listener, outcome, detail, run_id, at) VALUES
 ('e-001', 'review', 'started', 'review started', 'r-001', '2026-10-01T10:00:00Z'),
 ('e-001', 'rebase', 'ignored', 'not a rebase command', NULL, '2026-10-01T10:00:00Z'),
 ('e-001', 'mention', 'ignored', 'no mention', NULL, '2026-10-01T10:00:01Z'),
 ('e-002', 'mention', 'greeted', '', NULL, '2026-10-01T10:00:02Z'),
 ('e-003', 'review', 'started', 'review started', 'r-004', '2026-10-01T10:00:01Z'),
 ('e-004', 'plan', 'started', 'plan started', 'r-007', '2026-10-02T09:00:01Z'),
 ('e-005', 'execute', 'started', 'execution started', 'r-008', '2026-10-02T09:30:01Z'),
 ('e-006', 'rebase', 'started', 'rebase started', 'r-009', '2026-10-02T10:00:01Z'),
 ('e-007', 'review', 'started', 'review started', 'r-012', '2026-10-03T08:00:01Z');
