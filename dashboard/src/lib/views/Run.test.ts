import { afterEach, describe, expect, it, vi } from 'vitest';
import { ApiError } from '$lib/api/client';
import { cleanup, render, rows, settle } from '$lib/testing/render';
import { draft, runDetail } from '$lib/testing/fixtures';
import { fakeConnect, firstOf } from '$lib/testing/source';
import Run from './Run.svelte';

afterEach(cleanup);

const section = (title: string): string[][] => {
  const heading = [...document.querySelectorAll('h2')].find((h) => h.textContent === title);
  const table = heading?.nextElementSibling;
  if (!(table instanceof HTMLTableElement)) throw new Error(`no table under ${title}`);
  return [...table.querySelectorAll('tbody tr')].map((row) =>
    [...row.querySelectorAll('td')].map((td) => (td.textContent ?? '').trim()),
  );
};

describe('Run', () => {
  it('shows every part of the run record, text as text', async () => {
    render(Run, { id: 'r-1', load: () => Promise.resolve(runDetail()), cancel: vi.fn() });
    await settle();
    expect(document.querySelector('h1')?.textContent).toBe('review r-1');
    const facts = [...document.querySelectorAll('.facts dd')].map((dd) => dd.textContent?.trim());
    expect(facts).toContain('opened (asked by github:1234)');
    expect(document.body.textContent).toContain('Not bad. <b>1</b> finding.');
    expect(document.querySelector('b')).toBeNull();
    expect(document.querySelector('main script, body script')).toBeNull();
    expect(section('Drafts')).toEqual([
      ['d1', 'lane-a', 'src/a.rs:4', 'The loop never ends.', 'confirmed', 'opus', 'c-77', 'src/a.rs:4 loops.'],
      ['d2', 'lane-b', 'src/a.rs:4', 'The loop never ends.', 'same as d1', 'opus', 'c-77', 'The same.'],
      ['d3', 'lane-a', 'src/a.rs:4', 'withdrawal of c-12: It was wrong.', 'waiting', '', '', ''],
    ]);
    expect(section('Timeline')[0]).toEqual(['2026-10-07T10:03:00Z', 'info', '<script>alert(1)</script>']);
    expect(section('Tool calls')[0]).toEqual(['lane-a', 'read_file', '2', '0', '1', '0', '9']);
    expect(section('Findings')[0]).toEqual(['2026-10-07T10:02:00Z', 'lane-a', 'src/a.rs:4', 'c-77', 'posted']);
    const lanes = section('Lanes');
    expect(lanes.map((row) => row[0])).toEqual(['lane-a', 'lane-b']);
    const transcriptLink = document.querySelector<HTMLAnchorElement>('a[title="The whole conversation"]');
    expect(transcriptLink?.getAttribute('href')).toBe('/dashboard/runs/r-1/transcripts/lane-a');
    expect(document.querySelectorAll('a[title="The whole conversation"]')).toHaveLength(1);
    expect(section('Events')[0]?.[0]).toBe('e-1');
    expect(document.querySelector('button')).toBeNull();
  });

  it('offers a cancel only while the run is running, and posts it', async () => {
    const cancel = vi.fn(() => Promise.resolve({ run_id: 'r-1' }));
    const { connect } = fakeConnect();
    render(Run, { id: 'r-1', load: () => Promise.resolve(runDetail('running')), cancel, connect });
    await settle();
    const button = document.querySelector('button');
    expect(button?.textContent).toBe('Cancel this run');
    button?.click();
    await settle();
    expect(cancel).toHaveBeenCalledWith('r-1');
    expect(document.querySelector('[role=status]')?.textContent).toContain('Cancel sent');
  });

  it('follows a running run as it happens, and stops when it ends', async () => {
    const { connect, sources } = fakeConnect();
    render(Run, { id: 'r-1', load: () => Promise.resolve(runDetail('running')), cancel: vi.fn(), connect });
    await settle();
    expect(sources.map((s) => s.url)).toEqual(['/dashboard/api/v1/runs/r-1/stream']);
    const source = firstOf(sources);
    source.open();
    await settle(1);
    expect(document.querySelector('.badge')?.textContent).toBe('live');

    source.push('tool_call', {
      at: '', session: 'lane-a', model: 'model-x', turn: 9, tool: 'read_file', origin: 'henk',
      outcome: 'ok', arguments: '{}', arguments_len: 2, result_chars: 1, elapsed_ms: 1,
    });
    const lanes = runDetail('running').lanes.map((l) => (l.name === 'lane-a' ? { ...l, status: 'running' } : l));
    source.push('lanes', lanes);
    source.push('draft', draft('d4', { body: 'A new one.' }));
    await settle(1);
    expect(section('Lanes')[0]?.[3]).toBe('turn 9');
    expect(section('Drafts').at(-1)?.slice(0, 5)).toEqual(['d4', 'lane-a', 'src/a.rs:4', 'A new one.', 'waiting']);

    source.push('run', { run: { ...runDetail().run }, summary: 'Not bad.', error: null, check_id: '4711' });
    source.push('end');
    await settle(1);
    expect(source.closed).toBe(true);
    expect(document.querySelector('.status')?.textContent).toBe('finished');
    expect(document.querySelector('.badge')).toBeNull();
    expect(document.querySelector('main button, button')).toBeNull();
    expect(sources).toHaveLength(1);
  });

  it('does not follow a run that has ended', async () => {
    const { connect, sources } = fakeConnect();
    render(Run, { id: 'r-1', load: () => Promise.resolve(runDetail('finished')), cancel: vi.fn(), connect });
    await settle();
    expect(sources).toHaveLength(0);
  });

  it('says why a cancel was refused', async () => {
    const cancel = vi.fn(() =>
      Promise.reject(new ApiError(409, 'conflict', 'That run is not running here.')),
    );
    render(Run, { id: 'r-1', load: () => Promise.resolve(runDetail('running')), cancel, connect: fakeConnect().connect });
    await settle();
    document.querySelector('button')?.click();
    await settle();
    expect(document.querySelector('[role=status]')?.textContent).toBe('That run is not running here.');
  });

  it('shows a missing run as a problem', async () => {
    render(Run, {
      id: 'r-x',
      load: () => Promise.reject(new ApiError(404, 'not_found', 'No such run.')),
      cancel: vi.fn(),
    });
    await settle();
    expect(document.querySelector('[role=alert]')?.textContent).toContain('No such run.');
    expect(rows()).toEqual([]);
  });
});
