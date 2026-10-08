import { afterEach, describe, expect, it, vi } from 'vitest';
import type { LaneStats } from '$lib/api/types';
import { cleanup, render, rows, settle } from '$lib/testing/render';
import Lanes from './Lanes.svelte';

afterEach(cleanup);

const NOW = new Date('2026-10-07T12:00:00Z');

const stats: LaneStats = {
  reviews: [{ run_id: 'r-1', started_at: '2026-10-06T12:00:00Z' }],
  lanes: [
    {
      name: 'lane-a',
      kind: 'lane',
      models: ['m-new', 'm-old'],
      outcomes: [],
      ran: 20,
      finished: 14,
      timed_out: 1,
      did_not_finish: 5,
      reasons: { time_limit: 1, rate_limit: 3, provider_error: 1, cancelled: 1, declined: 0, stuck: 0 },
    },
  ],
};

describe('Lanes', () => {
  it('compares the lanes over the period with the reasons they did not finish', async () => {
    const load = vi.fn(() => Promise.resolve(stats));
    render(Lanes, { query: new URLSearchParams(), load, now: () => NOW });
    await settle();
    expect(load).toHaveBeenCalledWith('since=2026-09-07T12%3A00%3A00.000Z');
    expect(rows('table.compare tbody tr')[0]).toEqual([
      'lane-a', 'm-new, m-old', '20', '14', '1', '25% 5', 'rate limit 3, time limit 1, provider error 1, cancelled 1',
    ]);
    expect(document.querySelector('.segmented a.chosen')?.textContent).toBe('last 30 days');
    expect(document.body.textContent).toContain('1 review.');
  });

  it('asks for all time from the start, and falls back to 30 days for a period it does not know', async () => {
    const load = vi.fn(() => Promise.resolve(stats));
    render(Lanes, { query: new URLSearchParams('period=all'), load, now: () => NOW });
    await settle();
    expect(load).toHaveBeenCalledWith('since=1970-01-01T00%3A00%3A00Z');
    cleanup();
    render(Lanes, { query: new URLSearchParams('period=forever'), load, now: () => NOW });
    await settle();
    expect(load).toHaveBeenLastCalledWith('since=2026-09-07T12%3A00%3A00.000Z');
  });
});
