import { afterEach, describe, expect, it, vi } from 'vitest';
import type { EventDetail } from '$lib/api/types';
import { cleanup, render, settle } from '$lib/testing/render';
import Event from './Event.svelte';

afterEach(cleanup);

const event = (outcomes: EventDetail['outcomes']): EventDetail => ({
  event: {
    id: 'e-1',
    received_at: '2026-10-07T10:00:00Z',
    source: 'dashboard',
    kind: 'review_requested',
    repo: 'docspec/app',
    target: 7,
    requester: 'github:1234',
  },
  payload: '{"title": "<img src=x onerror=alert(1)>"}',
  outcomes,
});

describe('Event', () => {
  it('reads again until a listener has answered', async () => {
    const answered = event([
      { listener: 'review', outcome: 'started', detail: 'a review', run_id: 'r-1', at: '' },
    ]);
    const load = vi
      .fn<(id: string) => Promise<EventDetail>>()
      .mockResolvedValueOnce(event([]))
      .mockResolvedValue(answered);
    render(Event, { id: 'e-1', load, every: 5, patience: 1000 });
    await settle(1);
    expect(document.body.textContent).toContain('No listener has answered yet.');
    await new Promise((resolve) => setTimeout(resolve, 20));
    await settle();
    expect(load.mock.calls.length).toBeGreaterThanOrEqual(2);
    expect(document.body.textContent).toContain('review: started');
    expect(document.querySelector<HTMLAnchorElement>('a[href="/dashboard/runs/r-1"]')).not.toBeNull();
    expect(document.querySelector('pre')?.textContent).toBe('{"title": "<img src=x onerror=alert(1)>"}');
    expect(document.querySelector('img')).toBeNull();
  });
});
