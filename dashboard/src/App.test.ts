import { afterEach, describe, expect, it, vi } from 'vitest';
import type { EventItem } from '$lib/api/types';
import { href, navigate } from '$lib/router';
import { me } from '$lib/testing/fixtures';
import { cleanup, render, settle } from '$lib/testing/render';
import App from './App.svelte';

const item = (id: string): EventItem => ({
  event: {
    id,
    received_at: '2026-10-07T10:00:00Z',
    source: 'github_webhook',
    kind: 'pull_request',
    repo: 'docspec/app',
    target: 7,
    requester: null,
  },
  outcomes: [],
});

const api = vi.hoisted(() => ({
  events: vi.fn(),
  eventFacets: vi.fn(),
  event: vi.fn(),
}));

vi.mock('$lib/api/client', async (actual) => ({
  ...(await actual<typeof import('$lib/api/client')>()),
  me: () => Promise.resolve(me),
  events: api.events,
  eventFacets: api.eventFacets,
  event: api.event,
}));

afterEach(() => {
  cleanup();
  window.history.replaceState({}, '', '/');
});

describe('App', () => {
  it('keeps the events list when an event is chosen, without loading it again', async () => {
    api.events.mockResolvedValue({ items: [item('e-2'), item('e-1')], next: null });
    api.eventFacets.mockResolvedValue({ sources: ['github_webhook'], kinds: ['pull_request'] });
    api.event.mockImplementation((id: string) => Promise.resolve({ ...item(id), payload: '{}' }));
    window.history.replaceState({}, '', href('/events'));
    render(App, {});
    await settle();
    const table = document.querySelector('table.events');
    expect(table).not.toBeNull();
    expect(api.events).toHaveBeenCalledTimes(1);

    navigate('/events/e-1', { keepScroll: true });
    await settle();
    expect(document.querySelector('table.events')).toBe(table);
    expect(api.events).toHaveBeenCalledTimes(1);
    expect(api.eventFacets).toHaveBeenCalledTimes(1);
    expect(api.event).toHaveBeenLastCalledWith('e-1');
  });
});
