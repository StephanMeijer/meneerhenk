import { afterEach, describe, expect, it, vi } from 'vitest';
import { noteServerDate } from '$lib/clock';
import type { DraftItem, Page, QualityRow, QualitySeries } from '$lib/api/types';
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

/** The loaders the page needs besides the rates and the drafts. */
function more(series: QualitySeries[] = [], total = 0) {
  return {
    loadDaily: vi.fn(() => Promise.resolve(series)),
    loadCount: vi.fn(() => Promise.resolve({ count: total })),
  };
}

describe('Quality', () => {
  it('shows the rates per model with a meter, and what was rejected as text', async () => {
    const loadRates = vi.fn(() =>
      Promise.resolve([row('mistral-medium-3-5', 72, 7), row('deepseek-v4-flash-0731', 2, 1), row('new-model', 0, 0, 3)]),
    );
    const loadDrafts = vi.fn((): Promise<Page<DraftItem>> => Promise.resolve({ items: [rejectedItem], next: 'NEXT' }));
    const extra = more([], 72);
    render(Quality, { query: new URLSearchParams(), loadRates, loadDrafts, now: () => NOW, ...extra });
    await settle();

    expect(loadRates).toHaveBeenCalledWith('group=model&since=2026-09-07T12%3A00%3A00.000Z');
    expect(extra.loadDaily).toHaveBeenCalledWith('group=model&since=2026-09-07T12%3A00%3A00.000Z');
    expect(loadDrafts).toHaveBeenCalledWith('verdict=rejected&since=2026-09-07T12%3A00%3A00.000Z');
    expect(extra.loadCount).toHaveBeenCalledWith('verdict=rejected&since=2026-09-07T12%3A00%3A00.000Z');
    const table = rows('table.rates tbody tr');
    expect(table[0]).toEqual(['All models', '85', '8', '74', '0', '0', '3', '90% of 82']);
    expect(table[1]).toEqual(['mistral-medium-3-5', '79', '7', '72', '0', '0', '0', '91% of 79']);
    expect(table[3]?.[7]).toBe('- of 0');
    const meters = [...document.querySelectorAll('table.rates tbody tr:nth-child(2) meter')].map((m) => m.getAttribute('value'));
    expect(meters[0]).toBe(String(72 / 79));
    expect(document.querySelector('table.rates tbody tr:nth-child(2) meter')?.getAttribute('title')).toBe('72 of 79 judged');
    const flow = [...document.querySelectorAll('ul.flow li')].map((li) => li.textContent?.replace(/\s+/g, ' ').trim());
    expect(flow).toEqual([
      '8 confirmed and posted',
      '74 rejected by the check',
      '0 repeats, merged into another draft',
      '0 unchecked, went out unverified',
      '3 waiting for the check',
    ]);
    expect(document.querySelector('.showing')?.textContent?.trim()).toBe('Showing 1 of 72 rejected drafts');

    const modelLink = document.querySelector<HTMLAnchorElement>('table.rates a');
    expect(modelLink?.getAttribute('href')).toBe('/dashboard/quality?model=mistral-medium-3-5&verdict=rejected');

    const item = document.querySelector('ul.drafts li');
    expect(item?.querySelector('.text')?.textContent).toBe('The <code>target</code> can panic.');
    expect(item?.querySelector('.reason')?.textContent).toBe('claude-opus-5-5: It is a plain u64 (types.rs:195); <b>nothing</b> panics.');
    expect(item?.querySelector('b, code:not(.meta code)')).toBeNull();
    expect(item?.querySelector('a[href="/dashboard/runs/r-70"]')).not.toBeNull();
    expect(document.querySelector('.pager a')?.getAttribute('href')).toBe('/dashboard/quality?cursor=NEXT');
    const card = item?.querySelector('.meta strong')?.textContent;
    expect(card).toBe('d3');
  });

  it('asks for the group, period, repo and the list filters the query names', async () => {
    const loadRates = vi.fn(() => Promise.resolve([]));
    const loadDrafts = vi.fn((): Promise<Page<DraftItem>> => Promise.resolve({ items: [], next: null }));
    render(Quality, {
      query: new URLSearchParams('group=lane&period=all&repo=o/r&model=mistral&verdict=all&cursor=C'),
      loadRates,
      loadDrafts,
      now: () => NOW,
      ...more(),
    });
    await settle();
    expect(loadRates).toHaveBeenCalledWith('group=lane&repo=o%2Fr');
    expect(loadDrafts).toHaveBeenCalledWith('model=mistral&repo=o%2Fr&cursor=C');
    expect(document.body.textContent).toContain('No drafts in this period.');
    expect(document.querySelector('.list-head h2')?.textContent?.replace(/\s+/g, ' ').trim()).toBe('Every draft by mistral');
    const chosen = document.querySelector('.chips a.chosen');
    expect(chosen?.textContent).toBe('all');
    const current = [...document.querySelectorAll('.segmented a[aria-current]')].map((a) => a.textContent);
    expect(current).toEqual(['lane', 'all time']);
    const repository = [...document.querySelectorAll('.segmented a')].find((a) => a.textContent === 'repository');
    expect(repository?.getAttribute('href')).toBe('/dashboard/quality?group=repo&period=all&repo=o%2Fr&verdict=all');
    const selection = document.querySelector('.chips a.selection');
    expect(selection?.textContent?.replace(/\s+/g, ' ').trim()).toBe('model: mistral ×');
    expect(selection?.getAttribute('href')).toBe('/dashboard/quality?group=lane&period=all&repo=o%2Fr&verdict=all');
  });

  it('falls back to the defaults for values it does not know', async () => {
    const loadRates = vi.fn(() => Promise.resolve([]));
    const loadDrafts = vi.fn((): Promise<Page<DraftItem>> => Promise.resolve({ items: [], next: null }));
    render(Quality, { query: new URLSearchParams('group=colour&period=forever'), loadRates, loadDrafts, now: () => NOW, ...more() });
    await settle();
    expect(loadRates).toHaveBeenCalledWith('group=model&since=2026-09-07T12%3A00%3A00.000Z');
  });

  it('draws each group\'s rate per day, with a gap where nothing was judged', async () => {
    const series: QualitySeries[] = [
      {
        key: 'mistral',
        days: [
          { day: '2026-10-05', judged: 4, rejected: 3, rate: 0.75 },
          { day: '2026-10-06', judged: 0, rejected: 0, rate: null },
          { day: '2026-10-07', judged: 2, rejected: 1, rate: 0.5 },
        ],
      },
    ];
    const loadRates = vi.fn(() => Promise.resolve([row('mistral', 4, 2)]));
    const loadDrafts = vi.fn((): Promise<Page<DraftItem>> => Promise.resolve({ items: [], next: null }));
    render(Quality, { query: new URLSearchParams(), loadRates, loadDrafts, now: () => NOW, ...more(series) });
    await settle();
    const hidden = [...document.querySelectorAll('.line-chart table tr')].map((r) => r.textContent);
    expect(hidden).toEqual(['2026-10-0575%', '2026-10-06nothing judged', '2026-10-0750%']);
    expect(document.querySelectorAll('.line-chart circle')).toHaveLength(2);
    expect(document.querySelector('.legend')?.textContent?.trim()).toBe('mistral');
  });
});

/** The browser at noon, the server an hour ahead of it (#257). */
const BROWSER = new Date('2026-10-07T12:00:00Z');
const SERVER = new Date('2026-10-07T13:00:00Z');

function serverAhead(): void {
  vi.useFakeTimers({ toFake: ['Date'] });
  vi.setSystemTime(BROWSER);
  noteServerDate(SERVER.toUTCString());
}

function clocksAgree(): void {
  vi.useRealTimers();
  noteServerDate(new Date().toUTCString());
}

describe('Quality on the server clock', () => {
  afterEach(clocksAgree);

  it('asks for the period and says how long ago by the server clock, not the browser one', async () => {
    serverAhead();
    const loadRates = vi.fn(() => Promise.resolve([]));
    const twoMinutes = { ...rejectedItem, draft: { ...rejectedItem.draft, at: '2026-10-07T12:58:00Z' } };
    const loadDrafts = vi.fn((): Promise<Page<DraftItem>> => Promise.resolve({ items: [twoMinutes], next: null }));
    render(Quality, { query: new URLSearchParams(), loadRates, loadDrafts, ...more() });
    await settle();
    expect(loadRates).toHaveBeenCalledWith('group=model&since=2026-09-07T13%3A00%3A00.000Z');
    expect(document.querySelector('ul.drafts time')?.textContent).toBe('2 min ago');
  });
});
