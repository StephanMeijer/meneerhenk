import { afterEach, describe, expect, it, vi } from 'vitest';
import { ApiError } from '$lib/api/client';
import { cleanup, render, settle } from '$lib/testing/render';
import Commit from './Commit.svelte';
import Live from './Live.svelte';
import Problem from './Problem.svelte';
import Status from './Status.svelte';
import Time from './Time.svelte';

afterEach(cleanup);

describe('Status', () => {
  it('shows an icon and the word, in its tone', async () => {
    render(Status, { word: 'did_not_finish' });
    await settle(1);
    const pill = document.querySelector('.pill');
    expect(pill?.textContent).toBe('did not finish');
    expect(pill?.classList.contains('status')).toBe(true);
    expect(pill?.classList.contains('tone-drop')).toBe(true);
    expect(pill?.querySelector('svg.icon-broken')?.getAttribute('aria-hidden')).toBe('true');
  });

  it('takes a vocabulary, a class and words of its own', async () => {
    render(Status, { word: 'same_as', vocabulary: 'verdict', kind: 'verdict', text: 'same as d1' });
    await settle(1);
    const pill = document.querySelector('.verdict');
    expect(pill?.textContent).toBe('same as d1');
    expect(pill?.querySelector('svg.icon-link')).not.toBeNull();
  });
});

describe('Time', () => {
  it('says how long ago, with the exact time on hover', async () => {
    render(Time, { iso: '2026-10-07T10:02:11Z', at: new Date('2026-10-07T10:08:30Z') });
    await settle(1);
    const time = document.querySelector('time');
    expect(time?.textContent).toBe('6 min ago');
    expect(time?.getAttribute('title')).toBe('2026-10-07T10:02:11Z');
    expect(time?.getAttribute('datetime')).toBe('2026-10-07T10:02:11Z');
  });
});

describe('Commit', () => {
  it('shows 7 characters, the full SHA on hover, and copies it', async () => {
    const copy = vi.fn(() => Promise.resolve());
    render(Commit, { sha: 'abc1234def5678', copy });
    await settle(1);
    expect(document.querySelector('code')?.textContent).toBe('abc1234');
    expect(document.querySelector('code')?.getAttribute('title')).toBe('abc1234def5678');
    document.querySelector('button')?.click();
    await settle();
    expect(copy).toHaveBeenCalledWith('abc1234def5678');
    expect(document.querySelector('[role=status]')?.textContent).toBe('Copied.');
  });
});

describe('Live', () => {
  it('names the connection while it matters, and nothing once ended', async () => {
    render(Live, { connection: 'reconnecting' });
    await settle(1);
    expect(document.querySelector('.badge')?.textContent).toBe('reconnecting');
    cleanup();
    render(Live, { connection: 'ended' });
    await settle(1);
    expect(document.querySelector('.badge')).toBeNull();
  });
});

describe('Problem', () => {
  it('offers to try again', async () => {
    const retry = vi.fn();
    render(Problem, { error: new ApiError(0, 'unreachable', 'Henk did not answer. Is it running?'), retry });
    await settle(1);
    expect(document.querySelector('[role=alert]')?.textContent).toContain('Not loaded. Henk did not answer.');
    document.querySelector('button')?.click();
    expect(retry).toHaveBeenCalledTimes(1);
  });

  it('shows a refusal as one, with nothing to try again', async () => {
    render(Problem, { error: new ApiError(403, 'forbidden', "This GitHub account may not use Henk's dashboard."), retry: vi.fn() });
    await settle(1);
    expect(document.querySelector('.problem.refused')).not.toBeNull();
    expect(document.querySelector('button')).toBeNull();
  });

  it('keeps what went wrong inside the dashboard vague', async () => {
    render(Problem, { error: new TypeError('x is undefined') });
    await settle(1);
    expect(document.querySelector('[role=alert]')?.textContent).toContain('Reload the page.');
    expect(document.body.textContent).not.toContain('x is undefined');
  });
});
