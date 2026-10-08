import { afterEach, describe, expect, it, vi } from 'vitest';
import { ApiError } from '$lib/api/client';
import { cleanup, render, rows, settle } from '$lib/testing/render';
import { draft, reviewStages, runDetail } from '$lib/testing/fixtures';
import type { ToolCall } from '$lib/api/types';
import { fakeConnect, firstOf } from '$lib/testing/source';
import Run from './Run.svelte';

afterEach(cleanup);

const buttonNamed = (text: string): HTMLButtonElement | undefined =>
  [...document.querySelectorAll('button')].find((b) => (b.textContent ?? '').trim() === text);
const cancelButton = (): HTMLButtonElement | undefined => buttonNamed('Cancel run');

/** Opens the confirmation and confirms the cancel. */
async function confirmCancel(): Promise<void> {
  cancelButton()?.click();
  await settle(1);
  buttonNamed('Cancel review')?.click();
  await settle();
}

const section = (title: string): string[][] => {
  const heading = [...document.querySelectorAll('h2')].find((h) => h.textContent === title);
  const table = heading?.closest('section')?.querySelector('table');
  if (!(table instanceof HTMLTableElement)) throw new Error(`no table under ${title}`);
  return [...table.querySelectorAll('tbody tr')].map((row) =>
    [...row.querySelectorAll('td')].map((td) => (td.textContent ?? '').trim()),
  );
};

describe('Run', () => {
  it('shows every part of the run record, text as text', async () => {
    render(Run, { id: 'r-1', load: () => Promise.resolve(runDetail()), cancel: vi.fn() });
    await settle();
    expect(document.querySelector('h1')?.textContent).toBe('review r-1');
    const facts = [...document.querySelectorAll('.facts dd')].map((dd) => dd.textContent?.trim());
    expect(facts).toContain('opened (asked by github:1234)');
    expect(document.body.textContent).toContain('Not bad. <b>1</b> finding.');
    expect(document.querySelector('b')).toBeNull();
    expect(document.querySelector('main script, body script')).toBeNull();
    expect(section('Drafts')).toEqual([
      ['d1', 'lane-a', 'src/a.rs:4', 'The loop never ends.', 'confirmed', 'opus', 'c-77', 'src/a.rs:4 loops.'],
      ['d2', 'lane-b', 'src/a.rs:4', 'The loop never ends.', 'same as d1', 'opus', 'c-77', 'The same.'],
      ['d3', 'lane-a', 'src/a.rs:4', 'withdrawal of c-12: It was wrong.', 'waiting', '', '', ''],
    ]);
    expect(section('Timeline')[0]).toEqual(['10:03:00 UTC', 'info', '<script>alert(1)</script>']);
    expect(section('Tool calls')[0]).toEqual(['lane-a', 'read_file', '2', '0', '1', '0', '9']);
    const card = document.querySelector('ul.findings li');
    expect(card?.querySelector('.meta')?.textContent?.replace(/\s+/g, ' ').trim()).toBe(
      'c-77 posted lane-a src/a.rs:4 10:02:00 UTC',
    );
    expect(card?.querySelector('.text')?.textContent).toBe('The loop never ends.');
    expect(card?.querySelector('.reason')?.textContent).toBe('confirmed by opus: src/a.rs:4 loops.');
    const lanes = section('Lanes');
    expect(lanes.map((row) => row[0])).toEqual(['lane-a', 'lane-b']);
    const transcriptLink = document.querySelector<HTMLAnchorElement>('a[title="The whole conversation"]');
    expect(transcriptLink?.getAttribute('href')).toBe('/dashboard/runs/r-1/transcripts/lane-a');
    expect(document.querySelectorAll('a[title="The whole conversation"]')).toHaveLength(1);
    expect(section('Requests')[0]?.[0]).toBe('e-1');
    expect(document.querySelector('ol.pipeline')).toBeNull();
    expect(cancelButton()).toBeUndefined();
  });

  it('offers a cancel only while the run is running, asks first, and posts it once', async () => {
    const cancel = vi.fn(() => Promise.resolve({ run_id: 'r-1' }));
    const { connect } = fakeConnect();
    const detail = runDetail('running');
    detail.lanes = detail.lanes.map((lane) => (lane.name === 'lane-a' ? { ...lane, status: 'running' } : lane));
    render(Run, { id: 'r-1', who: 'github:5821', load: () => Promise.resolve(detail), cancel, connect });
    await settle();
    expect(cancelButton()).toBeDefined();

    cancelButton()?.click();
    await settle(1);
    const dialog = document.querySelector('dialog[open]');
    expect(dialog?.querySelector('h2')?.textContent).toBe('Cancel this review?');
    expect(dialog?.textContent).toContain('lane-a is still working.');
    const effects = [...(dialog?.querySelectorAll('li') ?? [])].map((li) => li.textContent);
    expect(effects).toEqual([
      'All lanes stop now. Drafts still waiting are marked cancelled.',
      'On the pull request, one comment and the check say it was cancelled and name you, github:5821.',
      'Nothing already posted is removed.',
    ]);
    expect(document.activeElement?.textContent).toBe('Keep running');
    buttonNamed('Keep running')?.click();
    await settle(1);
    expect(document.querySelector('dialog[open]')).toBeNull();
    expect(cancel).not.toHaveBeenCalled();

    await confirmCancel();
    expect(cancel).toHaveBeenCalledTimes(1);
    expect(cancel).toHaveBeenCalledWith('r-1');
    expect(document.querySelector('[role=status]')?.textContent).toContain('Cancel sent');
  });

  it('follows a running run as it happens, and stops when it ends', async () => {
    const { connect, sources } = fakeConnect();
    render(Run, { id: 'r-1', load: () => Promise.resolve(runDetail('running')), cancel: vi.fn(), connect });
    await settle();
    expect(sources.map((s) => s.url)).toEqual(['/dashboard/api/v1/runs/r-1/stream']);
    const source = firstOf(sources);
    source.open();
    await settle(1);
    expect(document.querySelector('.badge')?.textContent).toBe('live');

    source.push('tool_call', {
      at: '', session: 'lane-a', model: 'model-x', turn: 9, tool: 'read_file', origin: 'henk',
      outcome: 'ok', arguments: '{}', arguments_len: 2, result_chars: 1, elapsed_ms: 1,
    });
    const lanes = runDetail('running').lanes.map((l) => (l.name === 'lane-a' ? { ...l, status: 'running' } : l));
    source.push('lanes', lanes);
    source.push('draft', draft('d4', { body: 'A new one.' }));
    await settle(1);
    expect(section('Lanes')[0]?.[3]).toBe('turn 9');
    expect(section('Drafts').at(-1)?.slice(0, 5)).toEqual(['d4', 'lane-a', 'src/a.rs:4', 'A new one.', 'waiting']);

    source.push('run', { run: { ...runDetail().run }, summary: 'Not bad.', error: null, check_id: '4711' });
    source.push('end');
    await settle(1);
    expect(source.closed).toBe(true);
    expect(document.querySelector('.status')?.textContent).toBe('finished');
    expect(document.querySelector('.badge')).toBeNull();
    expect(cancelButton()).toBeUndefined();
    expect(sources).toHaveLength(1);
  });

  it('does not follow a run that has ended', async () => {
    const { connect, sources } = fakeConnect();
    render(Run, { id: 'r-1', load: () => Promise.resolve(runDetail('finished')), cancel: vi.fn(), connect });
    await settle();
    expect(sources).toHaveLength(0);
  });

  it('says why a cancel was refused', async () => {
    const cancel = vi.fn(() =>
      Promise.reject(new ApiError(409, 'conflict', 'That run is not running here.')),
    );
    render(Run, { id: 'r-1', load: () => Promise.resolve(runDetail('running')), cancel, connect: fakeConnect().connect });
    await settle();
    await confirmCancel();
    expect(document.querySelector('[role=status]')?.textContent).toBe('That run is not running here.');
  });

  it('offers to review a finished review again, and starts a new run of its commit', async () => {
    const start = vi.fn(() => Promise.resolve({ event_id: 'e-7' }));
    const go = vi.fn();
    render(Run, { id: 'r-1', load: () => Promise.resolve(runDetail()), cancel: vi.fn(), start, go });
    await settle();
    buttonNamed('Review again')?.click();
    await settle(1);
    const dialog = document.querySelector('dialog[open]');
    const text = (dialog?.textContent ?? '').replace(/\s+/g, ' ');
    expect(text).toContain('A new run, not a retry of this one.');
    expect(text).toContain('Henk starts a new review of abc1234 on docspec/app #7.');
    expect(text).toContain('If a review of that commit is running, the request joins it instead.');
    buttonNamed('Start a new review')?.click();
    await settle();
    expect(start).toHaveBeenCalledWith({
      kind: 'review',
      url: 'https://github.com/docspec/app/pull/7',
      commit: 'abc1234',
      note: null,
    });
    expect(go).toHaveBeenCalledWith('/events/e-7');
  });

  it('does not offer a review again while running, for a superseded run, or for a plan', async () => {
    const cases = [
      runDetail('running'),
      { ...runDetail(), run: { ...runDetail().run, status: 'superseded', superseded_by: 'r-2' } },
      { ...runDetail(), run: { ...runDetail().run, kind: 'plan' } },
    ];
    for (const detail of cases) {
      render(Run, { id: 'r-1', load: () => Promise.resolve(detail), cancel: vi.fn(), connect: fakeConnect().connect });
      await settle();
      expect(buttonNamed('Review again'), detail.run.status + detail.run.kind).toBeUndefined();
      cleanup();
    }
  });

  it('names the run that replaced a superseded one', async () => {
    const detail = { ...runDetail(), run: { ...runDetail().run, status: 'superseded', superseded_by: 'r-2' } };
    render(Run, { id: 'r-1', load: () => Promise.resolve(detail), cancel: vi.fn() });
    await settle();
    const facts = [...document.querySelectorAll('.facts div')];
    const replaced = facts.find((f) => f.querySelector('dt')?.textContent === 'Replaced by');
    expect(replaced?.querySelector('a')?.getAttribute('href')).toBe('/dashboard/runs/r-2');
  });

  it('shows the review as its stages, the lanes inside theirs', async () => {
    const detail = { ...runDetail(), stages: reviewStages() };
    render(Run, { id: 'r-1', load: () => Promise.resolve(detail), cancel: vi.fn() });
    await settle();
    const boxes = [...document.querySelectorAll('ol.pipeline > li')];
    expect(boxes.map((b) => b.querySelector('.name')?.textContent)).toEqual([
      'Requested', 'Queued', 'Started', 'Diff', 'Checkout', 'Lanes', 'Fact-check', 'Publish', 'Done',
    ]);
    const diff = boxes[3];
    expect(diff?.querySelector('.detail')?.textContent).toBe('12 files, +340 -25; 1 not reviewed');
    expect(diff?.querySelector('.time')?.textContent).toBe('10:01:00');
    expect(boxes[5]?.querySelector('.time')?.textContent).toBe('3m 00s');
    expect(boxes[4]?.classList.contains('skipped')).toBe(true);
    const lanes = [...(boxes[5]?.querySelectorAll('.lanes li') ?? [])].map((l) => l.textContent?.replace(/\s+/g, ' ').trim());
    expect(lanes).toEqual(['lane-a model-x finished 3m 55s', 'lane-b model-y did not finish 0m 55s']);
    const fold = (title: string) =>
      [...document.querySelectorAll('details')].find((x) => x.querySelector('h2')?.textContent === title);
    expect(fold('Lanes')?.open, 'with a pipeline the tables are folded').toBe(false);
  });

  it('opens the log of the box selected, and lets go when it is selected again', async () => {
    const detail = {
      ...runDetail(),
      stages: reviewStages(),
      events: [
        { at: '2026-10-07T10:01:00Z', level: 'info', message: 'diff: 12 files' },
        { at: '2026-10-07T10:03:00Z', level: 'warn', message: 'lane-b: rate limited' },
        { at: '2026-10-07T10:03:30Z', level: 'info', message: 'lane-a: EndTurn after 3 turns' },
      ],
    };
    render(Run, { id: 'r-1', load: () => Promise.resolve(detail), cancel: vi.fn() });
    await settle();
    const timeline = () => section('Timeline').map((row) => row[2]);
    const timelineOpen = () =>
      [...document.querySelectorAll('details')].find((x) => x.querySelector('h2')?.textContent === 'Timeline')?.open;
    expect(timelineOpen()).toBe(false);

    [...document.querySelectorAll<HTMLButtonElement>('ol.pipeline button.lane')].find((b) => b.textContent?.includes('lane-b'))?.click();
    await settle(1);
    expect(timeline()).toEqual(['lane-b: rate limited']);
    expect(timelineOpen()).toBe(true);

    [...document.querySelectorAll<HTMLButtonElement>('ol.pipeline button.box')].find((b) => b.textContent?.includes('Diff'))?.click();
    await settle(1);
    expect(timeline()).toEqual(['diff: 12 files']);
    [...document.querySelectorAll<HTMLButtonElement>('ol.pipeline button.box')].find((b) => b.textContent?.includes('Diff'))?.click();
    await settle(1);
    expect(timeline()).toHaveLength(3);
  });

  it('shows a missing run as a problem', async () => {
    render(Run, {
      id: 'r-x',
      load: () => Promise.reject(new ApiError(404, 'not_found', 'No such run.')),
      cancel: vi.fn(),
    });
    await settle();
    expect(document.querySelector('[role=alert]')?.textContent).toContain('No such run.');
    expect(rows()).toEqual([]);
  });
});

describe('every call of a run', () => {
  const call = (session: string, turn: number, outcome: string): ToolCall => ({
    at: '', session, model: 'm', turn, tool: 'read_file', origin: 'henk', outcome,
    arguments: '{"path":"<b>a.rs</b>"}', arguments_len: 20, result_chars: 5, elapsed_ms: 3,
  });

  it('loads once on asking, filters by outcome, and links a call to its turn when the run kept that conversation', async () => {
    const loadCalls = vi.fn(() =>
      Promise.resolve([call('lane-a', 1, 'ok'), call('lane-a', 2, 'error'), call('lane-b', 1, 'refused_scope')]),
    );
    render(Run, { id: 'r-1', load: () => Promise.resolve(runDetail()), cancel: vi.fn(), loadCalls });
    await settle();
    expect(loadCalls).not.toHaveBeenCalled();
    const ask = [...document.querySelectorAll('button')].find((b) => b.textContent === 'Show every call');
    ask?.click();
    await settle();
    expect(loadCalls).toHaveBeenCalledTimes(1);
    const timeline = () => rows('table.timeline tbody tr');
    expect(timeline().map((r) => [r[0], r[1], r[3]])).toEqual([
      ['lane-a', '1', 'ok'],
      ['lane-a', '2', 'error'],
      ['lane-b', '1', 'refused scope'],
    ]);
    expect(timeline()[0]?.[4]).toBe('{"path":"<b>a.rs</b>"}');
    expect(document.querySelector('table.timeline b')).toBeNull();
    const turnLinks = [...document.querySelectorAll('table.timeline a')].map((a) => a.getAttribute('href'));
    expect(turnLinks).toEqual([
      '/dashboard/runs/r-1/transcripts/lane-a#turn-1',
      '/dashboard/runs/r-1/transcripts/lane-a#turn-2',
    ]);

    [...document.querySelectorAll<HTMLButtonElement>('button.chip')].find((b) => b.textContent === 'problems')?.click();
    await settle(1);
    expect(timeline().map((r) => r[3])).toEqual(['error', 'refused scope']);
    [...document.querySelectorAll<HTMLButtonElement>('button.chip')].find((b) => b.textContent === 'refused')?.click();
    await settle(1);
    expect(timeline().map((r) => r[3])).toEqual(['refused scope']);
    expect(document.querySelector('.chips .muted')?.textContent).toBe('1 of 3');
  });
});
