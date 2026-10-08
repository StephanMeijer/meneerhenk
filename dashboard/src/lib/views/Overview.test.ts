import { afterEach, describe, expect, it, vi } from 'vitest';
import type { DayStats, Health, OverviewStats, Page, RunSummary } from '$lib/api/types';
import { cleanup, render, rows, settle } from '$lib/testing/render';
import { me, runSummary } from '$lib/testing/fixtures';
import { fakeConnect, firstOf } from '$lib/testing/source';
import Overview from './Overview.svelte';

afterEach(cleanup);

const NOW = new Date('2026-10-07T12:00:00Z');

const day = (date: string, runs: number, finished: number, failed: number, posted: number, drafts: number): DayStats => ({
  day: date,
  runs,
  finished,
  failed,
  findings_posted: posted,
  drafts,
});

const stats: OverviewStats = {
  from: '2026-10-05',
  to: '2026-10-07',
  days: [day('2026-10-05', 10, 8, 2, 3, 9), day('2026-10-06', 41, 38, 1, 5, 20), day('2026-10-07', 3, 1, 0, 0, 2)],
};

const health: Health = {
  checks: [
    { name: 'database', state: 'ok', detail: 'SQLite at /var/lib/henk/henk.db' },
    { name: 'secrets', state: 'warn', detail: '6 of 7 set.' },
  ],
};

function loaders(items: RunSummary[] = [], next: string | null = null) {
  return {
    loadRuns: vi.fn((): Promise<Page<RunSummary>> => Promise.resolve({ items, next })),
    loadCount: vi.fn(() => Promise.resolve({ count: 1284 })),
    loadHealth: vi.fn(() => Promise.resolve(health)),
    loadStats: vi.fn(() => Promise.resolve(stats)),
    now: () => NOW,
  };
}

/** The ids of the running runs, as the "Running now" list shows them. */
const runningIds = (): string[] =>
  [...document.querySelectorAll('ul.running > li a.mono')].map((a) => a.textContent ?? '');

describe('Overview', () => {
  it('lists the runs its query asks for, how many match, and how many run beyond those shown', async () => {
    const load = loaders([runSummary('r-2'), runSummary('r-1')], 'CUR');
    const { connect, sources } = fakeConnect();
    render(Overview, {
      me,
      query: new URLSearchParams('kind=review&repo=docspec/app&junk=x&cursor=OLD'),
      connect,
      ...load,
    });
    await settle();
    expect(load.loadRuns).toHaveBeenCalledWith('kind=review&repo=docspec%2Fapp&cursor=OLD');
    expect(load.loadCount).toHaveBeenCalledWith('kind=review&repo=docspec%2Fapp');
    expect(sources.map((s) => s.url)).toEqual(['/dashboard/api/v1/runs/stream']);
    firstOf(sources).push('snapshot', {
      runs: [runSummary('r-9', 'running')],
      count: 3,
      slots: { limit: 2, in_use: 2, waiting: [{ repo: 'docspec/app', target: 9, since: '2026-10-07T11:59:22Z' }] },
    });
    await settle(1);
    expect(document.body.textContent).toContain('Running now (3)');
    expect(document.body.textContent).toContain('And 2 more not shown.');
    expect(runningIds()).toEqual(['r-9']);
    expect(rows('table.runs tbody tr').map((row) => row[1])).toEqual(['r-2', 'r-1']);
    expect(document.querySelector('.panel-foot span')?.textContent?.replace(/\s+/g, ' ').trim()).toBe(
      'Newest first. 1,284 runs match.',
    );
    const pager = [...document.querySelectorAll('.pager a')].map((a) => [a.textContent, a.getAttribute('href')]);
    expect(pager).toEqual([
      ['Newest', '/dashboard/?kind=review&repo=docspec%2Fapp&junk=x'],
      ['Older', '/dashboard/?kind=review&repo=docspec%2Fapp&junk=x&cursor=CUR'],
    ]);
    expect(document.querySelector<HTMLSelectElement>('select[name=kind]')?.value).toBe('review');
  });

  it('asks for the runs since the start of the period chosen', async () => {
    const load = loaders();
    render(Overview, { me, query: new URLSearchParams('period=7d'), connect: fakeConnect().connect, ...load });
    await settle();
    expect(load.loadRuns).toHaveBeenCalledWith('since=2026-09-30T12%3A00%3A00.000Z');
    expect(document.querySelector<HTMLSelectElement>('select[name=period]')?.value).toBe('7d');
  });

  it('shows health and the four tiles, each chart with its numbers as text', async () => {
    const { connect, sources } = fakeConnect();
    render(Overview, { me, query: new URLSearchParams(), connect, ...loaders() });
    await settle();
    firstOf(sources).push('snapshot', {
      runs: [],
      count: 0,
      slots: { limit: 2, in_use: 2, waiting: [{ repo: 'docspec/app', target: 9, since: '2026-10-07T11:59:22Z' }] },
    });
    await settle(1);
    const tiles = [...document.querySelectorAll('.tiles .tile')].map((t) => [
      t.querySelector('strong')?.textContent,
      t.querySelector('.state')?.textContent,
      t.querySelector('p')?.textContent,
    ]);
    expect(tiles).toEqual([
      ['database', 'ok', 'SQLite at /var/lib/henk/henk.db'],
      ['secrets', 'warn', '6 of 7 set.'],
    ]);
    const figures = [...document.querySelectorAll('.stats .figure')].map((f) => f.textContent?.replace(/\s+/g, ' ').trim());
    expect(figures).toEqual([
      '54 41 yesterday',
      '6% 3 of 50 runs',
      '8 from 31 drafts',
      '2 of 2 in use, 1 waiting',
    ]);
    expect(document.querySelector('.stats .waiting')?.textContent?.replace(/\s+/g, ' ').trim()).toBe(
      'docspec/app #9 0m 38s, no free slot',
    );
    const runsTable = [...document.querySelectorAll('.stats figure')][0]?.querySelectorAll('tr');
    expect([...(runsTable ?? [])].map((r) => r.textContent)).toEqual(['2026-10-0510', '2026-10-0641', '2026-10-073']);
  });
});

describe('Running now', () => {
  it('follows runs starting and ending, and where each one is, without polling', async () => {
    const load = loaders();
    const { connect, sources } = fakeConnect();
    render(Overview, { me, query: new URLSearchParams(), connect, ...load });
    await settle();
    const source = firstOf(sources);
    source.push('snapshot', { runs: [], count: 0, slots: { limit: 2, in_use: 0, waiting: [] } });
    source.push('run', runSummary('r-5', 'running'));
    await settle(1);
    expect(document.body.textContent).toContain('Running now (1)');
    expect(runningIds()).toEqual(['r-5']);

    source.push('progress', {
      run_id: 'r-5',
      stages: [
        { name: 'diff', state: 'done' },
        { name: 'lanes', state: 'running' },
      ],
      lanes: [
        { name: 'lane-a', status: 'running' },
        { name: 'lane-b', status: 'did_not_finish' },
        { name: 'check-1', status: 'running' },
      ],
    });
    await settle(1);
    const row = document.querySelector('ul.running > li');
    const dots = [...(row?.querySelectorAll('.stepper .visually-hidden') ?? [])].map((d) => d.textContent);
    expect(dots).toEqual(['diff: done', 'lane-a: running', 'lane-b: did not finish']);
    expect(row?.querySelector('.progress .muted')?.textContent).toBe('lanes: 1 running, 1 did not finish');

    source.push('run', runSummary('r-5', 'finished'));
    await settle(1);
    expect(document.body.textContent).toContain('Running now (0)');
    expect(load.loadRuns).toHaveBeenCalledTimes(1);
  });
});

describe('Recent runs', () => {
  it('shows how each run\'s lanes ended, leaving the checks out', async () => {
    const run = {
      ...runSummary('r-1'),
      lanes: [
        { name: 'lane-a', status: 'finished' },
        { name: 'lane-b', status: 'timed_out' },
        { name: 'check-1', status: 'finished' },
      ],
    };
    render(Overview, { me, query: new URLSearchParams(), connect: fakeConnect().connect, ...loaders([run]) });
    await settle();
    expect(rows('table.runs tbody tr')[0]?.at(-1)).toBe('lane-a: finishedlane-b: timed out');
  });
});
