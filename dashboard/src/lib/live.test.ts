import { describe, expect, it } from 'vitest';
import type { LiveMessage, ToolCall } from './api/types';
import { NO_LOG, applyLive, applyRun, applyRunning, callsOf, connectionAfter } from './live';
import { draft, reviewStages, runDetail, runSummary } from './testing/fixtures';

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

describe('running runs moving on', () => {
  it('takes a run\'s lanes and stages from progress, and keeps them through a run message', () => {
    const run = { ...runSummary('r-1', 'running'), lanes: [{ name: 'lane-a', status: 'running' }] };
    let state = applyRunning({ runs: [], count: 0, slots: null }, {
      kind: 'snapshot',
      data: { runs: [run], count: 1, slots: { limit: 2, in_use: 1, waiting: [] } },
    });
    expect(state.slots?.limit).toBe(2);
    state = applyRunning(state, {
      kind: 'progress',
      data: { run_id: 'r-1', lanes: null, stages: [{ name: 'diff', state: 'done' }] },
    });
    expect(state.runs[0]?.stages).toEqual([{ name: 'diff', state: 'done' }]);
    expect(state.runs[0]?.lanes).toEqual([{ name: 'lane-a', status: 'running' }]);
    state = applyRunning(state, { kind: 'run', data: { ...runSummary('r-1', 'running'), trigger: 'again' } });
    expect(state.runs[0]?.trigger).toBe('again');
    expect(state.runs[0]?.stages).toHaveLength(1);
    expect(applyRunning(state, { kind: 'progress', data: { run_id: 'r-x', lanes: [], stages: null } })).toEqual(state);
  });
});

describe('stages and heartbeats', () => {
  it('replaces the stages and moves the heartbeat on', () => {
    const before = runDetail('running');
    const stages = reviewStages().slice(0, 3);
    const after = applyRun(before, { kind: 'stages', data: stages });
    expect(after.stages).toEqual(stages);
    const later = applyRun(after, { kind: 'heartbeat', data: { at: '2026-10-07T10:09:00Z' } });
    expect(later.heartbeat_at).toBe('2026-10-07T10:09:00Z');
    expect(later.stages).toEqual(stages);
  });
});

describe('applyRunning', () => {
  it('follows runs starting and ending, and keeps the count', () => {
    let state = applyRunning({ runs: [], count: 0, slots: null }, {
      kind: 'snapshot',
      data: { runs: [runSummary('r-1', 'running')], count: 5, slots: { limit: 3, in_use: 1, waiting: [] } },
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

describe('the live connection', () => {
  it('connects, goes live, reconnects and stays ended', () => {
    expect(connectionAfter('connecting', false)).toBe('connecting');
    expect(connectionAfter('connecting', true)).toBe('live');
    expect(connectionAfter('live', false)).toBe('reconnecting');
    expect(connectionAfter('reconnecting', true)).toBe('live');
    expect(connectionAfter('ended', true)).toBe('ended');
  });
});

const said = (seq: number, text: string): LiveMessage => ({
  seq,
  at: '2026-10-08T12:00:00Z',
  message: { role: 'assistant', turn: seq, parts: [{ type: 'text', text }] },
});

describe('applyLive', () => {
  it('starts from a snapshot, adds what is new once, and ends', () => {
    let log = applyLive(NO_LOG, { kind: 'snapshot', data: { messages: [said(1, 'a'), said(2, 'b')], cut: true } });
    expect(log.cut).toBe(true);
    log = applyLive(log, { kind: 'message', data: said(2, 'b') });
    log = applyLive(log, { kind: 'message', data: said(3, 'c') });
    expect(log.messages.map((m) => m.seq)).toEqual([1, 2, 3]);
    log = applyLive(log, { kind: 'end', data: { elsewhere: false } });
    expect([log.ended, log.elsewhere]).toEqual([true, false]);
  });
});

describe('callsOf', () => {
  it('gives the calls of one session and turn in order, each once', () => {
    const call = (session: string, turn: number, tool: string, at: string): ToolCall => ({
      at, session, model: 'm', turn, tool, origin: 'henk', outcome: 'ok', arguments: '{}', arguments_len: 2, result_chars: 1, elapsed_ms: 1,
    });
    const first = call('lane-a', 2, 'read_file', 't1');
    const calls = [first, call('lane-b', 2, 'search', 't1'), call('lane-a', 1, 'search', 't0'), call('lane-a', 2, 'search', 't2'), first];
    expect(callsOf(calls, 'lane-a', 2).map((c) => c.tool)).toEqual(['read_file', 'search']);
  });
});
