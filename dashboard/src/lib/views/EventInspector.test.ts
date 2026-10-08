import { afterEach, describe, expect, it, vi } from 'vitest';
import type { EventDetail } from '$lib/api/types';
import { cleanup, render, settle } from '$lib/testing/render';
import EventInspector from './EventInspector.svelte';

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

describe('EventInspector', () => {
  it('reads again until a listener has answered', async () => {
    const answered = event([
      { listener: 'review', outcome: 'started', detail: 'a review', run_id: 'r-1', at: '' },
    ]);
    const load = vi
      .fn<(id: string) => Promise<EventDetail>>()
      .mockResolvedValueOnce(event([]))
      .mockResolvedValue(answered);
    render(EventInspector, { id: 'e-1', load, every: 5, patience: 1000 });
    await settle(1);
    expect(document.body.textContent).toContain('No listener has answered yet.');
    await new Promise((resolve) => setTimeout(resolve, 20));
    await settle();
    expect(load.mock.calls.length).toBeGreaterThanOrEqual(2);
    expect(document.body.textContent).toContain('review: started');
    expect(document.querySelector<HTMLAnchorElement>('a[href="/dashboard/runs/r-1"]')).not.toBeNull();
    expect(document.querySelector('pre')?.textContent).toBe('{\n  "title": "<img src=x onerror=alert(1)>"\n}');
    expect(document.querySelector('img')).toBeNull();
  });

  it('shows a payload that is not JSON as it came, and copies what it shows', async () => {
    const copy = vi.fn(() => Promise.resolve());
    const odd = { ...event([{ listener: 'review', outcome: 'ignored', detail: 'not modelled', run_id: null, at: '' }]), payload: 'a=1&b=<b>2</b>' };
    render(EventInspector, { id: 'e-1', load: () => Promise.resolve(odd), copy });
    await settle();
    expect(document.querySelector('pre')?.textContent).toBe('a=1&b=<b>2</b>');
    expect(document.querySelector('pre b')).toBeNull();
    document.querySelector<HTMLButtonElement>('button[title="Copy the payload"]')?.click();
    await settle();
    expect(copy).toHaveBeenCalledWith('a=1&b=<b>2</b>');
    const facts = [...document.querySelectorAll('.facts dd')].map((dd) => dd.textContent?.trim());
    expect(facts.at(-1)).toBe('github:1234');
  });
});
