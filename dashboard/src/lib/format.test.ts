import { describe, expect, it } from 'vitest';
import {
  about,
  ago,
  average,
  clockTime,
  count,
  draftWhat,
  duration,
  kindText,
  lineCount,
  outcomeClass,
  periodSince,
  ratePercent,
  runDuration,
  share,
  shortCommit,
  utc,
  verdictText,
} from './format';
import { draft } from './testing/fixtures';

describe('format', () => {
  it('says what a run is about', () => {
    expect(about('o/r', 7)).toBe('o/r #7');
    expect(about('o/r', null)).toBe('o/r');
    expect(about(null, null)).toBe('');
  });

  it('puts a verdict in words', () => {
    expect(verdictText(draft('d1'))).toBe('waiting');
    expect(
      verdictText(draft('d2', { decision: { at: '', verdict: 'same_as', checker: 'm', reason: '', same_as: 'd1', comment_id: '' } })),
    ).toBe('same as d1');
    expect(
      verdictText(draft('d3', { decision: { at: '', verdict: 'not_checked', checker: '', reason: '', same_as: '', comment_id: '' } })),
    ).toBe('not checked');
  });

  it('names the comment a rewrite or withdrawal is about', () => {
    expect(draftWhat(draft('d1'))).toBe('The loop never ends.');
    expect(draftWhat(draft('d2', { kind: 'rewrite', target: 'c-9', body: 'Better.' }))).toBe('rewrite of c-9: Better.');
  });

  it('counts lines and says kinds', () => {
    expect(lineCount('')).toBe(0);
    expect(lineCount('a\nb\nc')).toBe(3);
    expect(kindText('discord_turn')).toBe('discord turn');
  });
});

describe('quality helpers', () => {
  it('put a rate as a whole percentage', () => {
    expect(ratePercent(72 / 79)).toBe('91%');
    expect(ratePercent(0)).toBe('0%');
    expect(ratePercent(null)).toBe('-');
  });

  it('turn a period into its start', () => {
    const now = new Date('2026-10-07T12:00:00Z');
    expect(periodSince('7d', now)).toBe('2026-09-30T12:00:00.000Z');
    expect(periodSince('all', now)).toBeNull();
    expect(periodSince('nonsense', now)).toBeNull();
  });
});

describe('tool helpers', () => {
  it('sort an outcome into its colour', () => {
    expect(['ok', 'error', 'refused_scope', 'refused_repeat', 'malformed_arguments', 'cancelled'].map(outcomeClass)).toEqual([
      'ok', 'error', 'refused', 'refused', 'other', 'other',
    ]);
  });

  it('give the time per call', () => {
    expect(average(100, 3)).toBe('33 ms');
    expect(average(0, 0)).toBe('-');
  });

  it('says how long ago, and the exact time', () => {
    const now = new Date('2026-10-07T10:08:11Z');
    expect(ago('2026-10-07T10:08:00Z', now)).toBe('just now');
    expect(ago('2026-10-07T10:09:00Z', now)).toBe('just now');
    expect(ago('2026-10-07T10:02:11Z', now)).toBe('6 min ago');
    expect(ago('2026-10-07T08:00:00Z', now)).toBe('2 h ago');
    expect(ago('2026-10-04T10:08:11Z', now)).toBe('3 d ago');
    expect(ago('not a time', now)).toBe('not a time');
    expect(utc('2026-10-07T12:02:11.345+02:00')).toBe('2026-10-07T10:02:11Z');
    expect(clockTime('2026-10-07T10:02:11Z')).toBe('10:02:11 UTC');
  });

  it('says how long something took', () => {
    expect(duration(0)).toBe('0m 00s');
    expect(duration(41_000)).toBe('0m 41s');
    expect(duration(192_000)).toBe('3m 12s');
    expect(duration(3_600_000)).toBe('1h 00m');
    expect(duration(3_840_000)).toBe('1h 04m');
    expect(duration(-5)).toBe('0m 00s');
    const run = { started_at: '2026-10-07T10:00:00Z', finished_at: null };
    expect(runDuration(run, new Date('2026-10-07T10:06:12Z'))).toBe('6m 12s');
    expect(runDuration({ ...run, finished_at: '2026-10-07T10:00:18Z' })).toBe('0m 18s');
  });

  it('shortens commits and counts with separators', () => {
    expect(shortCommit('abc1234def5678')).toBe('abc1234');
    expect(shortCommit('abc')).toBe('abc');
    expect(count(1284)).toBe('1,284');
    expect(share(72, 75)).toBe('96% of 75');
    expect(share(0, 0)).toBe('-');
  });
});
