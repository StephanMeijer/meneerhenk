import { afterEach, describe, expect, it, vi } from 'vitest';
import { ApiError } from '$lib/api/client';
import { cleanup, render, settle } from '$lib/testing/render';
import { waitingReview } from '$lib/testing/fixtures';
import Queued from './Queued.svelte';

afterEach(cleanup);

const NOW = new Date('2026-10-07T12:00:00Z');

const slots = {
  limit: 2,
  in_use: 2,
  waiting: [
    waitingReview('r-a', 8, 1, { requester: 'github:1234', trigger: 'review command' }),
    waitingReview('r-b', 9, 2, { since: '2026-10-07T11:58:30Z', target_url: null }),
  ],
};

const rowText = (row: Element | undefined): string[] =>
  [...(row?.children ?? [])].map((cell) => cell.textContent?.replace(/\s+/g, ' ').trim() ?? '');

describe('Queued', () => {
  it('lists what waits in the order it will start, with who asked and why it waits', async () => {
    render(Queued, { slots, now: () => NOW });
    await settle();
    const rows = [...document.querySelectorAll('ol.queued > li')];
    expect(rows.map((row) => row.querySelector('.position')?.textContent)).toEqual(['1', '2']);
    const first = rowText(rows[0]);
    expect(first.slice(0, 3)).toEqual(['1', 'r-a review', 'docspec/app #8 9f31c0d']);
    expect(rows[0]?.querySelector('.trigger')?.firstChild?.textContent).toBe('review command');
    expect(rows[0]?.querySelector('.trigger .who')?.textContent).toBe('github:1234');
    expect(rows[0]?.querySelector('.when strong')?.textContent).toBe('10m 00s');
    expect(rows[0]?.querySelector('.why')?.textContent).toBe('no free slot (2 of 2 in use)');
    expect(rows[0]?.querySelector('.about a')?.getAttribute('href')).toBe('https://github.com/docspec/app/pull/8');
    expect(rows[1]?.querySelector('.about a'), 'no link to make').toBeNull();
    expect(rows[1]?.querySelector('.when strong')?.textContent).toBe('1m 30s');
    expect(document.querySelector('a[href^="/dashboard/runs/"]'), 'no run page yet').toBeNull();
  });

  it('leaves out what Running now already shows, and says when nothing waits', async () => {
    render(Queued, { slots, shown: ['r-a'], now: () => NOW });
    await settle();
    expect([...document.querySelectorAll('ol.queued .who-what .mono')].map((n) => n.textContent)).toEqual(['r-b']);
    cleanup();
    render(Queued, { slots: { limit: 2, in_use: 0, waiting: [] }, now: () => NOW });
    await settle();
    expect(document.body.textContent).toContain('Nothing is waiting.');
  });

  it('asks before it cancels, cancels once, and says what happened', async () => {
    const cancel = vi.fn(() => Promise.resolve({ run_id: 'r-b' }));
    render(Queued, { slots, cancel, now: () => NOW });
    await settle();
    const button = (name: string) =>
      [...document.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === name);
    [...document.querySelectorAll<HTMLButtonElement>('ol.queued button.danger')][1]?.click();
    await settle(1);
    expect(document.querySelector('dialog')?.hasAttribute('open')).toBe(true);
    expect(document.querySelector('dialog')?.textContent?.replace(/\s+/g, ' ')).toContain('r-b on docspec/app #9 has waited 1m 30s');
    button('Keep waiting')?.click();
    await settle(1);
    expect(cancel).not.toHaveBeenCalled();

    [...document.querySelectorAll<HTMLButtonElement>('ol.queued button.danger')][1]?.click();
    await settle(1);
    button('Cancel review')?.click();
    await settle();
    expect(cancel).toHaveBeenCalledTimes(1);
    expect(cancel).toHaveBeenCalledWith('r-b');
    expect(document.querySelector('[role=status]')?.textContent).toBe('Cancel sent. r-b ends as cancelled while queued.');
  });

  it('says why a cancel did not go through', async () => {
    const cancel = vi.fn(() => Promise.reject(new ApiError(409, 'conflict', 'That run is not running here.')));
    render(Queued, { slots, cancel, now: () => NOW });
    await settle();
    document.querySelector<HTMLButtonElement>('ol.queued button.danger')?.click();
    await settle(1);
    [...document.querySelectorAll<HTMLButtonElement>('button')].find((b) => b.textContent?.trim() === 'Cancel review')?.click();
    await settle();
    expect(document.querySelector('[role=status]')?.textContent).toBe('That run is not running here.');
  });
});
