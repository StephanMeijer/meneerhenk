import { afterEach, describe, expect, it, vi } from 'vitest';
import { noteServerDate } from '$lib/clock';
import type { SessionStats } from '$lib/api/types';
import { cleanup, render, rows, settle } from '$lib/testing/render';
import Sessions from './Sessions.svelte';

afterEach(cleanup);

const NOW = new Date('2026-10-07T12:00:00Z');

const stats: SessionStats = {
  reviews: [{ run_id: 'r-1', started_at: '2026-10-06T12:00:00Z' }],
  sessions: [
    {
      name: 'reviewer',
      role: 'reviewer',
      models: ['m-r'],
      outcomes: [],
      ran: 4,
      finished: 4,
      timed_out: 0,
      did_not_finish: 0,
      reasons: { time_limit: 0, rate_limit: 0, provider_error: 0, cancelled: 0, declined: 0, stuck: 0 },
    },
    {
      name: 'lane-a',
      role: 'lane',
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

describe('Sessions', () => {
  it('compares the sessions over the period by role, with the reasons they did not finish', async () => {
    const load = vi.fn(() => Promise.resolve({ ...stats, sessions: [...stats.sessions].reverse() }));
    render(Sessions, { query: new URLSearchParams(), load, now: () => NOW });
    await settle();
    expect(load).toHaveBeenCalledWith('since=2026-09-07T12%3A00%3A00.000Z');
    const groups = [...document.querySelectorAll('table.compare tbody tr.group th')].map((th) =>
      th.textContent?.replace(/\s+/g, ' ').trim(),
    );
    expect(groups).toEqual(['Reviewer', 'Review lane · before the loop']);
    expect(rows('table.compare tbody tr:not(.group)')).toEqual([
      ['reviewer', 'm-r', '4', '4', '0', '0% 0', '-'],
      ['lane-a', 'm-new, m-old', '20', '14', '1', '25% 5', 'rate limit 3, time limit 1, provider error 1, cancelled 1'],
    ]);
    expect(document.querySelector('.segmented a.chosen')?.textContent).toBe('last 30 days');
    expect(document.querySelector('.segmented a')?.getAttribute('href')).toContain('/dashboard/sessions?');
    expect(document.body.textContent).toContain('1 review.');
  });

  it('asks for all time from the start, and falls back to 30 days for a period it does not know', async () => {
    const load = vi.fn(() => Promise.resolve(stats));
    render(Sessions, { query: new URLSearchParams('period=all'), load, now: () => NOW });
    await settle();
    expect(load).toHaveBeenCalledWith('since=1970-01-01T00%3A00%3A00Z');
    cleanup();
    render(Sessions, { query: new URLSearchParams('period=forever'), load, now: () => NOW });
    await settle();
    expect(load).toHaveBeenLastCalledWith('since=2026-09-07T12%3A00%3A00.000Z');
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

describe('Sessions on the server clock', () => {
  afterEach(clocksAgree);

  it('asks for the period by the server clock, not the browser one', async () => {
    serverAhead();
    const load = vi.fn(() => Promise.resolve(stats));
    render(Sessions, { query: new URLSearchParams(), load });
    await settle();
    expect(load).toHaveBeenCalledWith('since=2026-09-07T13%3A00%3A00.000Z');
  });
});
