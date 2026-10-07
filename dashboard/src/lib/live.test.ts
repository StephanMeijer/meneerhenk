import { describe, expect, it } from 'vitest';
import type { ToolCall } from './api/types';
import { applyRun, applyRunning } from './live';
import { draft, runDetail, runSummary } from './testing/fixtures';

const call = (turn: number, outcome = 'ok', tool = 'read_file'): ToolCall => ({
  at: '',
  session: 'lane-a',
  model: 'model-x',
  turn,
  tool,
  origin: 'henk',
  outcome,
  arguments: '{}',
  arguments_len: 2,
  result_chars: 1,
  elapsed_ms: 5,
});

describe('applyRun', () => {
  it('adds a tool call to its tally and moves the lane on', () => {
    let d = runDetail('running');
    d = applyRun(d, { kind: 'tool_call', data: call(4) });
    d = applyRun(d, { kind: 'tool_call', data: call(5, 'refused_repeat') });
    d = applyRun(d, { kind: 'tool_call', data: call(6, 'error', 'bash') });
    expect(d.tool_usage).toEqual([
      { session: 'lane-a', tool: 'read_file', calls: 4, errors: 0, refusals: 2, other: 0, total_ms: 19 },
      { session: 'lane-a', tool: 'bash', calls: 1, errors: 1, refusals: 0, other: 0, total_ms: 5 },
    ]);
    expect(d.lanes.find((l) => l.name === 'lane-a')?.last_call_turn).toBe(6);
  });

  it('keeps the live turn when the lanes come again without one', () => {
    let d = applyRun(runDetail('running'), { kind: 'tool_call', data: call(7) });
    d = applyRun(d, { kind: 'lanes', data: d.lanes.map((l) => ({ ...l, last_call_turn: null })) });
    expect(d.lanes.find((l) => l.name === 'lane-a')?.last_call_turn).toBe(7);
  });

  it('puts a decided draft in place of the waiting one, and a new one at the end', () => {
    let d = runDetail('running');
    const decided = draft('d3', {
      kind: 'withdrawal',
      target: 'c-12',
      body: 'It was wrong.',
      decision: { at: '', verdict: 'rejected', checker: 'opus', reason: 'It is right.', same_as: '', comment_id: '' },
    });
    d = applyRun(d, { kind: 'draft', data: decided });
    d = applyRun(d, { kind: 'draft', data: draft('d4') });
    expect(d.drafts.map((x) => [x.id, x.decision?.verdict ?? null])).toEqual([
      ['d1', 'confirmed'],
      ['d2', 'same_as'],
      ['d3', 'rejected'],
      ['d4', null],
    ]);
  });

  it('replaces the run, appends lines and findings, adds a transcript once, and takes a snapshot whole', () => {
    let d = runDetail('running');
    d = applyRun(d, {
      kind: 'run',
      data: { run: runSummary('r-1', 'finished'), summary: 'Done.', error: null, check_id: '1' },
    });
    expect([d.run.status, d.summary, d.check_id]).toEqual(['finished', 'Done.', '1']);
    d = applyRun(d, { kind: 'event', data: { at: '', level: 'warn', message: 'late' } });
    d = applyRun(d, {
      kind: 'finding',
      data: { at: '', lane: 'lane-a', path: 'a.rs', line: 1, comment_id: 'c-2', action: 'posted' },
    });
    const ref = { at: '', session: 'lane-b', model: 'm', stop: 'EndTurn', turns: 1, bytes: 1 };
    d = applyRun(d, { kind: 'transcript', data: ref });
    d = applyRun(d, { kind: 'transcript', data: ref });
    expect(d.events.at(-1)?.message).toBe('late');
    expect(d.findings).toHaveLength(2);
    expect(d.transcripts.map((t) => t.session)).toEqual(['lane-a', 'lane-b']);
    const fresh = runDetail('finished');
    expect(applyRun(d, { kind: 'snapshot', data: fresh })).toBe(fresh);
    expect(applyRun(d, { kind: 'end' })).toBe(d);
  });
});

describe('applyRunning', () => {
  it('follows runs starting and ending, and keeps the count', () => {
    let state = applyRunning({ runs: [], count: 0 }, {
      kind: 'snapshot',
      data: { runs: [runSummary('r-1', 'running')], count: 5 },
    });
    state = applyRunning(state, { kind: 'run', data: runSummary('r-2', 'running') });
    expect([state.runs.map((r) => r.id), state.count]).toEqual([['r-2', 'r-1'], 6]);
    state = applyRunning(state, { kind: 'run', data: runSummary('r-1', 'running') });
    expect(state.count).toBe(6);
    state = applyRunning(state, { kind: 'run', data: runSummary('r-1', 'finished') });
    expect([state.runs.map((r) => r.id), state.count]).toEqual([['r-2'], 5]);
    state = applyRunning(state, { kind: 'run', data: runSummary('r-9', 'failed') });
    expect(state.count).toBe(5);
  });
});
