import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { get } from 'svelte/store';
import { everySecond, noteServerDate, now, serverNow } from './clock';

const START = new Date('2026-10-08T12:00:00Z');

function hidden(is: boolean): void {
  Object.defineProperty(document, 'visibilityState', { configurable: true, get: () => (is ? 'hidden' : 'visible') });
  document.dispatchEvent(new Event('visibilitychange'));
}

beforeEach(() => {
  vi.useFakeTimers();
  vi.setSystemTime(START);
  noteServerDate(START.toUTCString());
});

afterEach(() => {
  hidden(false);
  vi.useRealTimers();
});

describe('everySecond', () => {
  it('ticks every second while read, and stops when nothing reads it', () => {
    const seen: number[] = [];
    const stop = everySecond.subscribe((at) => seen.push(at.getTime() - START.getTime()));
    vi.advanceTimersByTime(3000);
    expect(seen).toEqual([0, 1000, 2000, 3000]);
    stop();
    expect(vi.getTimerCount(), 'no timer left').toBe(0);
  });

  it('rests while the tab is hidden and catches up at once when it shows', () => {
    const seen: number[] = [];
    const stop = everySecond.subscribe((at) => seen.push(at.getTime() - START.getTime()));
    vi.advanceTimersByTime(1000);
    hidden(true);
    expect(vi.getTimerCount()).toBe(0);
    vi.advanceTimersByTime(60_000);
    expect(seen).toEqual([0, 1000]);
    hidden(false);
    expect(seen.at(-1)).toBe(61_000);
    vi.advanceTimersByTime(1000);
    expect(seen.at(-1)).toBe(62_000);
    stop();
  });
});

describe('the server clock', () => {
  it('follows a server ahead by seconds, and takes a second for rounding', () => {
    noteServerDate(new Date(START.getTime() + 10_000).toUTCString());
    expect(serverNow().getTime() - START.getTime()).toBe(10_000);
    const stop = now.subscribe(() => {});
    expect(get(now).getTime() - START.getTime()).toBe(10_000);
    stop();
    noteServerDate(new Date(START.getTime() + 1000).toUTCString());
    expect(serverNow().getTime()).toBe(START.getTime());
    noteServerDate('not a date');
    noteServerDate(null);
    expect(serverNow().getTime()).toBe(START.getTime());
  });
});
