// Shapes the API returns, for the view tests.
import type { Draft, Me, Round, RunDetail, RunSummary, Session, Stage, WaitingReview } from '$lib/api/types';

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
    superseded_by: null,
    loop_stop: null,
    loop_rounds: null,
    sessions: [],
    stages: [],
  };
}

export function draft(id: string, overrides: Partial<Draft> = {}): Draft {
  return {
    at: '2026-10-07T10:01:00Z',
    id,
    session: 'lane-a',
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
    sessions: [
      { name: 'lane-a', role: 'lane', model: 'model-x', status: 'finished', turns: 3, input_tokens: 100, output_tokens: 20, cache_read_tokens: 80, cache_write_tokens: 15, reasoning_tokens: 0, error: null, last_call_turn: null, started_at: '2026-10-07T10:00:05Z', finished_at: '2026-10-07T10:04:00Z' },
      { name: 'lane-b', role: 'lane', model: 'model-y', status: 'did_not_finish', turns: 1, input_tokens: 10, output_tokens: 2, cache_read_tokens: 0, cache_write_tokens: 0, reasoning_tokens: 0, error: 'gave up', last_call_turn: null, started_at: '2026-10-07T10:00:05Z', finished_at: '2026-10-07T10:01:00Z' },
    ],
    rounds: [],
    findings: [{ at: '2026-10-07T10:02:00Z', session: 'lane-a', path: 'src/a.rs', line: 4, comment_id: 'c-77', action: 'posted' }],
    drafts: [
      draft('d1', {
        decision: { at: '', verdict: 'confirmed', checker: 'opus', reason: 'src/a.rs:4 loops.', same_as: '', comment_id: 'c-77' },
      }),
      draft('d2', {
        session: 'lane-b',
        decision: { at: '', verdict: 'same_as', checker: 'opus', reason: 'The same.', same_as: 'd1', comment_id: 'c-77' },
      }),
      draft('d3', { kind: 'withdrawal', target: 'c-12', body: 'It was wrong.' }),
    ],
    transcripts: [{ at: '', session: 'lane-a', model: 'model-x', stop: 'EndTurn', turns: 3, bytes: 100 }],
    tool_usage: [{ session: 'lane-a', tool: 'read_file', calls: 2, errors: 0, refusals: 1, other: 0, total_ms: 9 }],
    events: [{ at: '2026-10-07T10:03:00Z', level: 'info', message: '<script>alert(1)</script>' }],
    stages: [],
    requests: [{ id: 'e-1', received_at: '', source: 'github_webhook', kind: 'pull_request', repo: 'docspec/app', target: 7, requester: null }],
  };
}

/** A session of a run, as the API sends it. */
function session(name: string, role: string, overrides: Partial<Session> = {}): Session {
  return {
    name,
    role,
    model: `model-${name}`,
    status: 'finished',
    turns: 4,
    input_tokens: 1000,
    output_tokens: 100,
    cache_read_tokens: 0,
    cache_write_tokens: 0,
    reasoning_tokens: 0,
    error: null,
    last_call_turn: null,
    started_at: '2026-10-07T10:01:00Z',
    finished_at: '2026-10-07T10:04:00Z',
    ...overrides,
  };
}

/** A round of a pairing (#374), from minute `from` to `to`. */
function round(
  name: string,
  n: number,
  firstTurn: number,
  handover: string,
  from: number,
  to: number,
  turns: number,
  cut = false,
): Round {
  return {
    session: name,
    role: name,
    round: n,
    started_at: `2026-10-07T10:0${from}:00Z`,
    ended_at: `2026-10-07T10:0${to}:00Z`,
    first_turn: firstTurn,
    turns,
    handover,
    handover_cut: cut,
  };
}

/** A review the loop made (#283): the reviewer and the executor, in rounds. */
export function loopDetail(): RunDetail {
  const at = (s: number): string => `2026-10-07T10:0${s}:00Z`;
  const stage = (name: string, state: string, detail: string, from: number, to: number): Stage => ({
    name, state, detail, started_at: at(from), ended_at: at(to),
  });
  return {
    ...runDetail(),
    sessions: [session('executor', 'executor'), session('reviewer', 'reviewer')],
    rounds: [
      round('reviewer', 1, 1, 'Review pull request #7 at abc.', 1, 2, 5),
      round('executor', 1, 1, 'Round 1. The reviewer\'s findings, as data: f1.', 2, 3, 3),
      round('reviewer', 2, 6, 'Round 2. The executor pushed commit 9f3 to the branch.', 3, 4, 2, true),
    ],
    findings: [],
    drafts: [draft('f1', { session: 'reviewer', kind: 'loop_finding' })],
    transcripts: [],
    tool_usage: [],
    stages: [
      stage('requested', 'done', '', 0, 0),
      stage('started', 'done', 'check 4711 opened', 0, 0),
      stage('diff', 'done', '2 files', 0, 1),
      stage('checkout', 'done', '', 1, 1),
      stage('rounds', 'done', 'The review loop ran 2 rounds', 1, 4),
      stage('push', 'done', '1 commit', 4, 4),
      stage('publish', 'skipped', 'a review posts nothing', 4, 4),
      stage('done', 'done', '', 4, 4),
    ],
  };
}

/** A plan (#328): the planner and the reviewer, with no rounds stage. */
export function planDetail(): RunDetail {
  const detail = runDetail();
  const at = (s: number): string => `2026-10-07T10:0${s}:00Z`;
  return {
    ...detail,
    run: { ...detail.run, kind: 'plan', loop_stop: 'converged', loop_rounds: 2 },
    sessions: [session('planner', 'planner'), session('reviewer', 'reviewer')],
    rounds: [
      round('planner', 1, 1, 'Plan issue #7.', 0, 2, 4),
      round('reviewer', 1, 1, 'Review this plan.', 2, 3, 2),
    ],
    findings: [],
    drafts: [],
    transcripts: [],
    tool_usage: [],
    stages: [
      { name: 'requested', state: 'done', detail: '', started_at: at(0), ended_at: at(0) },
      { name: 'started', state: 'done', detail: '', started_at: at(0), ended_at: at(0) },
      { name: 'session', state: 'done', detail: 'planner, model-planner', started_at: at(0), ended_at: at(4) },
      { name: 'publish', state: 'done', detail: 'the plan on #7', started_at: at(4), ended_at: at(4) },
      { name: 'done', state: 'done', detail: '', started_at: at(4), ended_at: at(4) },
    ],
  };
}

/** A finished review's stages, as Henk records them (#226). */
export function reviewStages(): Stage[] {
  const at = (s: number): string => `2026-10-07T10:0${s}:00Z`;
  const stage = (name: string, state: string, detail: string, from: number, to: number | null): Stage => ({
    name, state, detail, started_at: at(from), ended_at: to === null ? null : at(to),
  });
  return [
    stage('requested', 'done', 'requested, github:1234', 0, 0),
    stage('queued', 'done', 'waited for a review slot', 0, 1),
    stage('started', 'done', 'check 4711 opened', 1, 1),
    stage('diff', 'done', '12 files, +340 -25; 1 not reviewed', 1, 1),
    stage('checkout', 'skipped', 'no workspaces: the lanes read through the platform', 1, 1),
    stage('lanes', 'done', '1 of 2 finished, 1 did not finish', 1, 4),
    stage('fact_check', 'done', '2 drafts: 1 confirmed, 0 rejected, 1 repeat', 4, 5),
    stage('publish', 'done', '1 line comment and the summary', 5, 5),
    stage('done', 'done', 'check 4711 closed', 5, 5),
  ];
}

/** A review waiting for a slot (#251); `overrides` make it a rebase or a
 * review waiting for one (#580). */
export function waitingReview(runId: string, target: number, position: number, overrides: Partial<WaitingReview> = {}): WaitingReview {
  return {
    run_id: runId,
    kind: 'review',
    platform: 'github',
    repo: 'docspec/app',
    target,
    target_url: `https://github.com/docspec/app/pull/${target}`,
    commit: '9f31c0d8e2a1b4c5d6e7f8091a2b3c4d5e6f7081',
    trigger: 'new commits',
    requester: null,
    since: '2026-10-07T11:50:00Z',
    position,
    reason: 'no_slot',
    priority: 'review',
    waits_for: null,
    ...overrides,
  };
}
