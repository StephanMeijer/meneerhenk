import { afterEach, describe, expect, it, vi } from 'vitest';
import type { DraftItem, Page, QualityRow } from '$lib/api/types';
import { draft } from '$lib/testing/fixtures';
import { cleanup, render, rows, settle } from '$lib/testing/render';
import Quality from './Quality.svelte';

afterEach(cleanup);

const row = (key: string, rejected: number, confirmed: number, waiting = 0): QualityRow => ({
  key,
  repo: null,
  target: null,
  target_url: null,
  drafts: rejected + confirmed + waiting,
  confirmed,
  rejected,
  same_as: 0,
  unchecked: 0,
  not_checked: 0,
  cancelled: 0,
  failed: 0,
  waiting,
  judged: rejected + confirmed,
  rejection_rate: rejected + confirmed === 0 ? null : rejected / (rejected + confirmed),
});

const rejectedItem: DraftItem = {
  run_id: 'r-70',
  repo: 'StephanMeijer/meneerhenk',
  target: 70,
  target_url: 'https://github.com/StephanMeijer/meneerhenk/pull/70',
  draft: draft('d3', {
    model: 'mistral-medium-3-5',
    lane: 'lane-b',
    body: 'The <code>target</code> can panic.',
    decision: {
      at: '',
      verdict: 'rejected',
      checker: 'claude-opus-5-5',
      reason: 'It is a plain u64 (types.rs:195); <b>nothing</b> panics.',
      same_as: '',
      comment_id: '',
    },
  }),
};

const NOW = new Date('2026-10-07T12:00:00Z');

describe('Quality', () => {
  it('shows the rates per model with a meter, and what was rejected as text', async () => {
    const loadRates = vi.fn(() =>
      Promise.resolve([row('mistral-medium-3-5', 72, 7), row('deepseek-v4-flash-0731', 2, 1), row('new-model', 0, 0, 3)]),
    );
    const loadDrafts = vi.fn((): Promise<Page<DraftItem>> => Promise.resolve({ items: [rejectedItem], next: 'NEXT' }));
    render(Quality, { query: new URLSearchParams(), loadRates, loadDrafts, now: () => NOW });
    await settle();

    expect(loadRates).toHaveBeenCalledWith('group=model&since=2026-09-07T12%3A00%3A00.000Z');
    expect(loadDrafts).toHaveBeenCalledWith('verdict=rejected&since=2026-09-07T12%3A00%3A00.000Z');
    const table = rows('table.rates tbody tr');
    expect(table[0]).toEqual(['mistral-medium-3-5', '79', '7', '72', '0', '0', '0', '91% of 79']);
    expect(table[2]?.[7]).toBe('- of 0');
    const meters = [...document.querySelectorAll('meter')].map((m) => m.getAttribute('value'));
    expect(meters[0]).toBe(String(72 / 79));
    expect(document.querySelector('meter')?.getAttribute('title')).toBe('72 of 79 judged');

    const modelLink = document.querySelector<HTMLAnchorElement>('table.rates a');
    expect(modelLink?.getAttribute('href')).toBe('/dashboard/quality?model=mistral-medium-3-5&verdict=rejected');

    const item = document.querySelector('ul.drafts li');
    expect(item?.querySelector('.text')?.textContent).toBe('The <code>target</code> can panic.');
    expect(item?.querySelector('.reason')?.textContent).toBe('claude-opus-5-5: It is a plain u64 (types.rs:195); <b>nothing</b> panics.');
    expect(item?.querySelector('b, code:not(.meta code)')).toBeNull();
    expect(item?.querySelector('a[href="/dashboard/runs/r-70"]')).not.toBeNull();
    expect(document.querySelector('.pager a')?.getAttribute('href')).toBe('/dashboard/quality?cursor=NEXT');
  });

  it('asks for the group, period, repo and the list filters the query names', async () => {
    const loadRates = vi.fn(() => Promise.resolve([]));
    const loadDrafts = vi.fn((): Promise<Page<DraftItem>> => Promise.resolve({ items: [], next: null }));
    render(Quality, {
      query: new URLSearchParams('group=lane&period=all&repo=o/r&model=mistral&verdict=all&cursor=C'),
      loadRates,
      loadDrafts,
      now: () => NOW,
    });
    await settle();
    expect(loadRates).toHaveBeenCalledWith('group=lane&repo=o%2Fr');
    expect(loadDrafts).toHaveBeenCalledWith('model=mistral&repo=o%2Fr&cursor=C');
    expect(document.body.textContent).toContain('No drafts in this period.');
    expect(document.querySelector('h2')?.textContent?.replace(/\s+/g, ' ').trim()).toBe('Every draft by mistral');
    const chosen = document.querySelector('.chips a.chosen');
    expect(chosen?.textContent).toBe('all');
    expect(document.querySelector<HTMLSelectElement>('select[name=group]')?.value).toBe('lane');
  });

  it('falls back to the defaults for values it does not know', async () => {
    const loadRates = vi.fn(() => Promise.resolve([]));
    const loadDrafts = vi.fn((): Promise<Page<DraftItem>> => Promise.resolve({ items: [], next: null }));
    render(Quality, { query: new URLSearchParams('group=colour&period=forever'), loadRates, loadDrafts, now: () => NOW });
    await settle();
    expect(loadRates).toHaveBeenCalledWith('group=model&since=2026-09-07T12%3A00%3A00.000Z');
  });
});
