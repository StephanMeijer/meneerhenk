// Shapes the API returns, for the view tests.
import type { Draft, Me, RunDetail, RunSummary } from '$lib/api/types';

export const me: Me = { github_id: 1234, login: 'alice', csrf: 'tok', startable: ['review', 'plan'] };

export function runSummary(id: string, status = 'finished'): RunSummary {
  return {
    id,
    kind: 'review',
    platform: 'github',
    repo: 'docspec/app',
    target: 7,
    target_url: 'https://github.com/docspec/app/pull/7',
    commit: 'abc1234',
    status,
    trigger: 'opened',
    requester: null,
    started_at: '2026-10-07T10:00:00Z',
    finished_at: status === 'running' ? null : '2026-10-07T10:05:00Z',
  };
}

export function draft(id: string, overrides: Partial<Draft> = {}): Draft {
  return {
    at: '2026-10-07T10:01:00Z',
    id,
    lane: 'lane-a',
    model: 'model-x',
    kind: 'finding',
    path: 'src/a.rs',
    line: 4,
    target: '',
    body: 'The loop never ends.',
    decision: null,
    ...overrides,
  };
}

export function runDetail(status = 'finished'): RunDetail {
  return {
    run: { ...runSummary('r-1', status), requester: 'github:1234' },
    summary: 'Not bad. <b>1</b> finding.',
    error: null,
    check_id: '4711',
    heartbeat_at: null,
    lanes: [
      { name: 'lane-a', model: 'model-x', status: 'finished', turns: 3, input_tokens: 100, output_tokens: 20, error: null, last_call_turn: null },
      { name: 'lane-b', model: 'model-y', status: 'dropped', turns: 1, input_tokens: 10, output_tokens: 2, error: 'gave up', last_call_turn: null },
    ],
    findings: [{ at: '2026-10-07T10:02:00Z', lane: 'lane-a', path: 'src/a.rs', line: 4, comment_id: 'c-77', action: 'posted' }],
    drafts: [
      draft('d1', {
        decision: { at: '', verdict: 'confirmed', checker: 'opus', reason: 'src/a.rs:4 loops.', same_as: '', comment_id: 'c-77' },
      }),
      draft('d2', {
        lane: 'lane-b',
        decision: { at: '', verdict: 'same_as', checker: 'opus', reason: 'The same.', same_as: 'd1', comment_id: 'c-77' },
      }),
      draft('d3', { kind: 'withdrawal', target: 'c-12', body: 'It was wrong.' }),
    ],
    transcripts: [{ at: '', session: 'lane-a', model: 'model-x', stop: 'EndTurn', turns: 3, bytes: 100 }],
    tool_usage: [{ session: 'lane-a', tool: 'read_file', calls: 2, errors: 0, refusals: 1, other: 0, total_ms: 9 }],
    events: [{ at: '2026-10-07T10:03:00Z', level: 'info', message: '<script>alert(1)</script>' }],
    requests: [{ id: 'e-1', received_at: '', source: 'github_webhook', kind: 'pull_request', repo: 'docspec/app', target: 7, requester: null }],
  };
}
