import { afterEach, describe, expect, it, vi } from 'vitest';
import type { Page, RunSummary } from '$lib/api/types';
import { cleanup, render, rows, settle } from '$lib/testing/render';
import { me, runSummary } from '$lib/testing/fixtures';
import { fakeConnect, firstOf } from '$lib/testing/source';
import Overview from './Overview.svelte';

afterEach(cleanup);

describe('Overview', () => {
  it('lists the runs its query asks for, and how many run now beyond those shown', async () => {
    const loadRuns = vi.fn(
      (): Promise<Page<RunSummary>> =>
        Promise.resolve({ items: [runSummary('r-2'), runSummary('r-1')], next: 'CUR' }),
    );
    const { connect, sources } = fakeConnect();
    render(Overview, {
      me,
      query: new URLSearchParams('kind=review&repo=docspec/app&junk=x&cursor=OLD'),
      loadRuns,
      connect,
    });
    await settle();
    expect(loadRuns).toHaveBeenCalledTimes(1);
    expect(loadRuns).toHaveBeenCalledWith('kind=review&repo=docspec%2Fapp&cursor=OLD');
    expect(sources.map((s) => s.url)).toEqual(['/dashboard/api/v1/runs/stream']);
    firstOf(sources).push('snapshot', { runs: [runSummary('r-9', 'running')], count: 3 });
    await settle(1);
    expect(document.body.textContent).toContain('Running now (3)');
    expect(document.body.textContent).toContain('And 2 more not shown.');
    expect(rows().map((row) => row[1])).toEqual(['r-9', 'r-2', 'r-1']);
    const pager = [...document.querySelectorAll('.pager a')].map((a) => [a.textContent, a.getAttribute('href')]);
    expect(pager).toEqual([
      ['Newest', '/dashboard/?kind=review&repo=docspec%2Fapp&junk=x'],
      ['Older', '/dashboard/?kind=review&repo=docspec%2Fapp&junk=x&cursor=CUR'],
    ]);
    expect(document.querySelector<HTMLSelectElement>('select[name=kind]')?.value).toBe('review');
  });
});

describe('Running now', () => {
  it('follows runs starting and ending without polling', async () => {
    const loadRuns = vi.fn((): Promise<Page<RunSummary>> => Promise.resolve({ items: [], next: null }));
    const { connect, sources } = fakeConnect();
    render(Overview, { me, query: new URLSearchParams(), loadRuns, connect });
    await settle();
    const source = firstOf(sources);
    source.push('snapshot', { runs: [], count: 0 });
    source.push('run', runSummary('r-5', 'running'));
    await settle(1);
    expect(document.body.textContent).toContain('Running now (1)');
    expect(rows().map((row) => row[1])).toEqual(['r-5']);
    source.push('run', { ...runSummary('r-6', 'running'), trigger: 'requested', requester: 'github:1234' });
    await settle(1);
    expect(rows().find((row) => row[1] === 'r-6')?.[5]).toBe('requested github:1234');
    source.push('run', runSummary('r-5', 'finished'));
    await settle(1);
    expect(document.body.textContent).toContain('Running now (1)');
    expect(loadRuns).toHaveBeenCalledTimes(1);
  });
});
