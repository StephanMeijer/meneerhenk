import { afterEach, describe, expect, it, vi } from 'vitest';
import type { LaneOutcome, LaneRow, LaneStats } from '$lib/api/types';
import { cleanup, render, settle } from '$lib/testing/render';
import LaneReliability from './LaneReliability.svelte';

afterEach(cleanup);

const reasons = { time_limit: 0, rate_limit: 0, provider_error: 0, cancelled: 0, declined: 0, stuck: 0 };

const outcome = (i: number, status: string, model = 'mistral-medium-3-5'): LaneOutcome => ({
  run_id: `r-${i}`,
  model,
  status,
  reason: status === 'did_not_finish' ? 'rate_limit' : status === 'timed_out' ? 'time_limit' : null,
});

/** 30 reviews; lane-a did not finish in every third, timed out in the
 * last; lane-b ran only in the last two, on a new model in the last. */
function fixture(): LaneStats {
  const reviews = Array.from({ length: 30 }, (_, i) => ({ run_id: `r-${i}`, started_at: `2026-10-07T${String(i % 24).padStart(2, '0')}:00:00Z` }));
  const a = reviews.map((_, i) => outcome(i, i === 29 ? 'timed_out' : i % 3 === 0 ? 'did_not_finish' : 'finished'));
  const laneA: LaneRow = {
    name: 'lane-a',
    kind: 'lane',
    models: ['mistral-medium-3-5'],
    outcomes: a,
    ran: 30,
    finished: 19,
    timed_out: 1,
    did_not_finish: 10,
    reasons: { ...reasons, rate_limit: 10, time_limit: 1 },
  };
  const laneB: LaneRow = {
    name: 'lane-b',
    kind: 'lane',
    models: ['deepseek-v4', 'qwen-3'],
    outcomes: [...Array<null>(28).fill(null), outcome(28, 'finished', 'qwen-3'), outcome(29, 'finished', 'deepseek-v4')],
    ran: 2,
    finished: 2,
    timed_out: 0,
    did_not_finish: 0,
    reasons,
  };
  return { reviews, lanes: [laneA, laneB] };
}

describe('LaneReliability', () => {
  it('shows a square per review by how the lane ended, linking to its run, with a text equivalent', async () => {
    const load = vi.fn(() => Promise.resolve(fixture()));
    render(LaneReliability, { load });
    await settle();
    expect(load).toHaveBeenCalledWith('last=30');

    const [a, b] = [...document.querySelectorAll('ul.lanes > li')];
    const squares = [...(a?.querySelectorAll('.squares .square') ?? [])];
    expect(squares).toHaveLength(30);
    expect(squares[0]?.className).toContain('sq-drop');
    expect(squares[1]?.className).toContain('sq-ok');
    expect(squares[29]?.className).toContain('sq-warn');
    expect(squares[0]?.getAttribute('href')).toBe('/dashboard/runs/r-0');
    expect(squares[0]?.getAttribute('title')).toBe('2026-10-07 mistral-medium-3-5: did not finish, rate limit');
    expect(a?.querySelector('.share')?.textContent?.replace(/\s+/g, ' ').trim()).toBe('33% 10 of 30 reviews');
    expect(a?.querySelector('.squares')?.getAttribute('aria-hidden')).toBe('true');
    expect(a?.querySelector('.visually-hidden')?.textContent).toBe(
      'lane-a: 10 of 30 did not finish, 1 timed out; last 5, newest last: finished, finished, did not finish, finished, timed out.',
    );

    const gaps = b?.querySelectorAll('.squares .gap') ?? [];
    expect(gaps).toHaveLength(28);
    expect(b?.querySelectorAll('.squares a')).toHaveLength(2);
    expect(b?.querySelector('.model')?.textContent).toBe('deepseek-v4 and 1 more');
    expect(b?.querySelector('.share strong')?.textContent).toBe('0%');

    const compare = document.querySelector<HTMLAnchorElement>('.panel-head a');
    expect(compare?.getAttribute('href')).toBe('/dashboard/lanes');
  });

  it('says so when no review has ended', async () => {
    render(LaneReliability, { load: () => Promise.resolve({ reviews: [], lanes: [] }) });
    await settle();
    expect(document.body.textContent).toContain('No review has ended yet.');
  });
});
