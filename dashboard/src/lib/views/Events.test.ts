import { afterEach, describe, expect, it, vi } from 'vitest';
import type { EventDetail, EventItem, Page } from '$lib/api/types';
import { cleanup, render, settle } from '$lib/testing/render';
import Events from './Events.svelte';

afterEach(cleanup);

const item = (id: string, outcomes: EventItem['outcomes'], repo: string | null = 'docspec/app'): EventItem => ({
  event: {
    id,
    received_at: '2026-10-07T10:00:00Z',
    source: 'github_webhook',
    kind: repo === null ? 'unmodelled' : 'pull_request',
    repo,
    target: repo === null ? null : 7,
    requester: null,
  },
  outcomes,
});

const items = [
  item('e-2', [{ listener: 'review', outcome: 'superseded', detail: 'New commits.', run_id: 'r-9', at: '' }]),
  item('e-1', [], null),
];

function loaders(page: Page<EventItem> = { items, next: null }) {
  const one = (id: string): Promise<EventDetail> => {
    const found = page.items.find((i) => i.event.id === id) ?? item(id, []);
    return Promise.resolve({ ...found, payload: '{"action":"synchronize"}' });
  };
  return {
    load: vi.fn(() => Promise.resolve(page)),
    loadFacets: vi.fn(() => Promise.resolve({ sources: ['api', 'github_webhook'], kinds: ['pull_request'] })),
    loadOne: vi.fn(one),
  };
}

describe('Events', () => {
  it('lists events with what the listeners did, and shows the newest beside them', async () => {
    const load = loaders();
    render(Events, { query: new URLSearchParams('source=github_webhook&junk=x'), ...load });
    await settle();
    expect(load.load).toHaveBeenCalledWith('source=github_webhook');
    const sources = [...document.querySelectorAll<HTMLOptionElement>('select[name=source] option')].map((o) => o.value);
    expect(sources).toEqual(['', 'api', 'github_webhook']);
    expect(document.querySelector<HTMLSelectElement>('select[name=source]')?.value).toBe('github_webhook');
    const rows = [...document.querySelectorAll('table.events tbody tr')];
    expect(rows[0]?.querySelector('.state, .listener-outcome')?.textContent).toBe('superseded');
    expect(rows[1]?.textContent).toContain('no listener');
    expect(rows[1]?.textContent).toContain('no repository');
    expect(rows[0]?.classList.contains('chosen')).toBe(true);
    expect(load.loadOne).toHaveBeenCalledWith('e-2');
    expect(document.querySelector('.inspector pre')?.textContent).toBe('{\n  "action": "synchronize"\n}');
  });

  it('shows the event the path names, even one not on this page, and links keep the filters', async () => {
    const load = loaders();
    render(Events, { query: new URLSearchParams('source=api'), selected: 'e-77', ...load });
    await settle();
    expect(load.loadOne).toHaveBeenCalledWith('e-77');
    expect(document.querySelector('.inspector h2')?.textContent).toContain('e-77');
    expect(document.querySelector('table.events tr.chosen')).toBeNull();
    const link = document.querySelector('table.events a.mono');
    expect(link?.getAttribute('href')).toBe('/dashboard/events/e-2?source=api');
  });

  it('chooses an event by its row, keeping the filters and the place on the page', async () => {
    const scroll = vi.fn();
    window.scrollTo = scroll;
    render(Events, { query: new URLSearchParams('source=github_webhook'), ...loaders() });
    await settle();
    document.querySelectorAll<HTMLTableRowElement>('table.events tbody tr')[1]?.querySelector('td:nth-child(2)')?.dispatchEvent(
      new MouseEvent('click', { bubbles: true }),
    );
    await settle(1);
    expect(window.location.pathname + window.location.search).toBe('/dashboard/events/e-1?source=github_webhook');
    expect(scroll).not.toHaveBeenCalled();
  });

  it('chooses an event by its id link, keeping the place on the page', async () => {
    const scroll = vi.fn();
    window.scrollTo = scroll;
    window.history.replaceState({}, '', '/dashboard/events');
    render(Events, { query: new URLSearchParams('kind=pull_request'), ...loaders() });
    await settle();
    const anchor = document.querySelectorAll<HTMLAnchorElement>('table.events tbody td:first-child a')[1];
    const click = new MouseEvent('click', { bubbles: true, cancelable: true, button: 0 });
    anchor?.dispatchEvent(click);
    await settle(1);
    expect(click.defaultPrevented).toBe(true);
    expect(window.location.pathname + window.location.search).toBe('/dashboard/events/e-1?kind=pull_request');
    expect(scroll).not.toHaveBeenCalled();
  });
});
