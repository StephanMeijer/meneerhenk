import { afterEach, describe, expect, it, vi } from 'vitest';
import type { LiveMessage, ToolCall } from '$lib/api/types';
import { cleanup, render, settle } from '$lib/testing/render';
import { fakeConnect, firstOf } from '$lib/testing/source';
import LiveSession from './LiveSession.svelte';

afterEach(cleanup);

const at = '2026-10-08T12:00:00Z';

const opening: LiveMessage = {
  seq: 1,
  at,
  message: { role: 'user', turn: 0, parts: [{ type: 'text', text: '<b>Review</b> this.' }] },
};
const answer: LiveMessage = {
  seq: 2,
  at,
  message: {
    role: 'assistant',
    turn: 1,
    parts: [
      { type: 'text', text: 'Looking.' },
      { type: 'call', name: 'read_file', arguments: '{"path":"src/a.rs"}' },
      { type: 'call', name: 'search', arguments: '{"q":"x"}' },
    ],
  },
};
const results: LiveMessage = {
  seq: 3,
  at,
  message: { role: 'user', turn: 1, parts: [{ type: 'result', error: false, content: 'fn main() {}' }] },
};

const call = (session: string, turn: number, tool: string, outcome: string): ToolCall => ({
  at, session, model: 'm', turn, tool, origin: 'henk', outcome, arguments: '{}', arguments_len: 2, result_chars: 300, elapsed_ms: 12,
});

/** The log box with a layout jsdom does not have: 1000 px of content in
 * a 200 px box. */
function laidOut(): HTMLElement & { scrollTop: number } {
  const box = document.querySelector<HTMLElement>('.log');
  if (box === null) throw new Error('no log');
  let top = 800;
  Object.defineProperty(box, 'scrollHeight', { configurable: true, get: () => 1000 });
  Object.defineProperty(box, 'clientHeight', { configurable: true, get: () => 200 });
  Object.defineProperty(box, 'scrollTop', {
    configurable: true,
    get: () => top,
    set: (value: number) => (top = value),
  });
  return box as HTMLElement & { scrollTop: number };
}

describe('LiveSession', () => {
  it('shows what the session says as text, with how each call went', async () => {
    const { connect, sources } = fakeConnect();
    const loadCalls = vi.fn(() => Promise.resolve([call('lane-a', 1, 'read_file', 'refused_scope'), call('lane-b', 1, 'search', 'ok')]));
    render(LiveSession, { runId: 'r-1', session: 'lane-a', connect, loadCalls });
    await settle();
    expect(sources.map((s) => s.url)).toEqual(['/dashboard/api/v1/runs/r-1/sessions/lane-a/stream']);
    expect(loadCalls).toHaveBeenCalledWith('r-1');
    const source = firstOf(sources);
    source.push('snapshot', { messages: [opening, answer], cut: true });
    await settle(1);

    const log = document.querySelector('.log');
    expect(log?.textContent).toContain('<b>Review</b> this.');
    expect(log?.querySelector('b')).toBeNull();
    expect(document.body.textContent).toContain('Earlier turns are in the transcript once the lane ends.');
    const calls = [...document.querySelectorAll('.call')].map((c) => c.textContent?.replace(/\s+/g, ' ').trim());
    expect(calls).toEqual(['Call read_file refused scope 12 ms, 300 chars', 'Call search running']);

    source.push('message', results);
    source.push('message', results);
    await settle(1);
    expect(document.querySelectorAll('.log article')).toHaveLength(3);
  });

  it('keeps to the end while followed, lets go on a scroll up, and jumps back', async () => {
    const { connect, sources } = fakeConnect();
    render(LiveSession, { runId: 'r-1', session: 'lane-a', connect, loadCalls: () => Promise.resolve([]) });
    await settle();
    const source = firstOf(sources);
    source.push('snapshot', { messages: [opening], cut: false });
    await settle(1);
    const box = laidOut();

    source.push('message', answer);
    await settle(1);
    expect(box.scrollTop, 'followed to the end').toBe(1000);
    expect(document.querySelector('button.jump')).toBeNull();

    box.scrollTop = 300;
    box.dispatchEvent(new Event('scroll'));
    await settle(1);
    source.push('message', results);
    await settle(1);
    expect(box.scrollTop, 'left where the reader is').toBe(300);
    const jump = document.querySelector<HTMLButtonElement>('button.jump');
    expect(jump?.textContent).toBe('Jump to latest');

    jump?.click();
    await settle(1);
    expect(box.scrollTop).toBe(1000);
    expect(document.querySelector('button.jump')).toBeNull();
  });

  it('hands over to the transcript when the session ends, or says it cannot hear it', async () => {
    const { connect, sources } = fakeConnect();
    render(LiveSession, { runId: 'r-1', session: 'lane-a', connect, loadCalls: () => Promise.resolve([]) });
    await settle();
    const source = firstOf(sources);
    source.push('snapshot', { messages: [opening], cut: false });
    source.push('end', { elsewhere: false });
    await settle(1);
    expect(source.closed).toBe(true);
    expect(document.querySelector('.panel-foot a')?.getAttribute('href')).toBe('/dashboard/runs/r-1/transcripts/lane-a');

    cleanup();
    const other = fakeConnect();
    render(LiveSession, { runId: 'r-2', session: 'lane-b', connect: other.connect, loadCalls: () => Promise.resolve([]) });
    await settle();
    firstOf(other.sources).push('end', { elsewhere: true });
    await settle(1);
    expect(document.body.textContent).toContain('This lane runs in another Henk process');
  });
});
