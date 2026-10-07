import { afterEach, describe, expect, it, vi } from 'vitest';
import type { Page, RunSummary } from '$lib/api/types';
import { cleanup, render, rows, settle } from '$lib/testing/render';
import { me, runSummary } from '$lib/testing/fixtures';
import Overview from './Overview.svelte';

afterEach(cleanup);

describe('Overview', () => {
  it('lists the runs its query asks for, and how many run now beyond those shown', async () => {
    const loadRuns = vi.fn((query: string): Promise<Page<RunSummary>> => {
      if (query.startsWith('status=running')) {
        return Promise.resolve({ items: [runSummary('r-9', 'running')], next: null });
      }
      return Promise.resolve({ items: [runSummary('r-2'), runSummary('r-1')], next: 'CUR' });
    });
    const countRuns = vi.fn(() => Promise.resolve({ count: 3 }));
    render(Overview, {
      me,
      query: new URLSearchParams('kind=review&repo=docspec/app&junk=x&cursor=OLD'),
      loadRuns,
      countRuns,
      every: 60_000,
    });
    await settle();
    expect(loadRuns).toHaveBeenCalledWith('kind=review&repo=docspec%2Fapp&cursor=OLD');
    expect(countRuns).toHaveBeenCalledWith('status=running');
    expect(document.body.textContent).toContain('Running now (3)');
    expect(document.body.textContent).toContain('And 2 more not shown.');
    expect(rows().map((row) => row[0])).toEqual(['r-9', 'r-2', 'r-1']);
    const pager = [...document.querySelectorAll('.pager a')].map((a) => [a.textContent, a.getAttribute('href')]);
    expect(pager).toEqual([
      ['Newest', '/dashboard/?kind=review&repo=docspec%2Fapp&junk=x'],
      ['Older', '/dashboard/?kind=review&repo=docspec%2Fapp&junk=x&cursor=CUR'],
    ]);
    expect(document.querySelector<HTMLSelectElement>('select[name=kind]')?.value).toBe('review');
  });
});
