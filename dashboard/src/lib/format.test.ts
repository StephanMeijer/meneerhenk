import { describe, expect, it } from 'vitest';
import { about, draftWhat, kindText, lineCount, periodSince, ratePercent, verdictText } from './format';
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
