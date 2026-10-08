import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { noteServerDate } from '$lib/clock';
import { cleanup, render, settle } from '$lib/testing/render';
import { runDetail, runSummary } from '$lib/testing/fixtures';
import { fakeConnect, firstOf } from '$lib/testing/source';
import Run from '$lib/views/Run.svelte';
import RunsTable from '$lib/views/RunsTable.svelte';

// Five seconds after the fixtures' runs started.
const START = new Date('2026-10-07T10:00:05Z');

beforeEach(() => {
  // Only the clock's interval and Date are fake: settle() waits on real timeouts.
  vi.useFakeTimers({ toFake: ['setInterval', 'clearInterval', 'Date'] });
  vi.setSystemTime(START);
  noteServerDate(START.toUTCString());
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

const durations = (): (string | undefined)[] =>
  [...document.querySelectorAll('table.runs tbody tr')].map((row) => row.querySelectorAll('td')[7]?.textContent?.trim());

describe('durations of what still runs', () => {
  it('count up every second, and those that ended stay put', async () => {
    render(RunsTable, { runs: [runSummary('r-1', 'running'), runSummary('r-2', 'finished')] });
    await settle();
    expect(durations()).toEqual(['0m 05s', '5m 00s']);
    vi.advanceTimersByTime(1000);
    await settle(1);
    expect(durations()).toEqual(['0m 06s', '5m 00s']);
    vi.advanceTimersByTime(3600_000);
    await settle(1);
    expect(durations()[0]).toBe('1h 00m');
  });

  it('stop at the time the stream says a run ended, not at the browser clock', async () => {
    const { connect, sources } = fakeConnect();
    render(Run, { id: 'r-1', load: () => Promise.resolve(runDetail('running')), cancel: vi.fn(), connect });
    await settle();
    const shown = () =>
      [...document.querySelectorAll('dt')].find((dt) => dt.textContent === 'Duration')?.nextElementSibling?.textContent?.trim();
    expect(shown()).toBe('0m 05s');
    vi.advanceTimersByTime(2000);
    await settle(1);
    expect(shown()).toBe('0m 07s');

    const ended = { ...runDetail('running').run, status: 'finished', finished_at: '2026-10-07T10:00:06Z' };
    firstOf(sources).push('run', { run: ended, summary: null, error: null, check_id: null });
    vi.advanceTimersByTime(5000);
    await settle(1);
    expect(shown(), 'six seconds from start to end; the browser clock would say 12').toBe('0m 06s, ended just now');
  });
});
