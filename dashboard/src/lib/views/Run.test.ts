import { afterEach, describe, expect, it, vi } from 'vitest';
import { ApiError } from '$lib/api/client';
import { cleanup, render, rows, settle } from '$lib/testing/render';
import { runDetail } from '$lib/testing/fixtures';
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
    const load = vi.fn(() => Promise.resolve(runDetail('running')));
    render(Run, { id: 'r-1', load, cancel });
    await settle();
    const button = document.querySelector('button');
    expect(button?.textContent).toBe('Cancel this run');
    button?.click();
    await settle();
    expect(cancel).toHaveBeenCalledWith('r-1');
    expect(load).toHaveBeenCalledTimes(2);
    expect(document.querySelector('[role=status]')?.textContent).toContain('Cancel sent');
  });

  it('says why a cancel was refused', async () => {
    const cancel = vi.fn(() =>
      Promise.reject(new ApiError(409, 'conflict', 'That run is not running here.')),
    );
    render(Run, { id: 'r-1', load: () => Promise.resolve(runDetail('running')), cancel });
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
