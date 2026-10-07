// Small pure helpers the views share. Everything returns text; the views
// show it as text.
import type { Draft } from './api/types';

/** What a run or event is about: `owner/name #7`. */
export function about(repo: string | null, target: number | null): string {
  if (repo === null) return '';
  return target === null ? repo : `${repo} #${target}`;
}

/** A draft's verdict in words: `confirmed`, `same as d2`, `waiting`. */
export function verdictText(draft: Draft): string {
  const decision = draft.decision;
  if (decision === null) return 'waiting';
  const verdict = decision.verdict.replaceAll('_', ' ');
  return decision.same_as === '' ? verdict : `${verdict} ${decision.same_as}`;
}

/** What a draft says; a rewrite or withdrawal names the comment it is about. */
export function draftWhat(draft: Draft): string {
  return draft.kind === 'finding' ? draft.body : `${draft.kind} of ${draft.target}: ${draft.body}`;
}

/** A tool result longer than this many lines is folded away. */
export const FOLD_LINES = 12;

/** How many lines a text has. */
export function lineCount(text: string): number {
  return text === '' ? 0 : text.split('\n').length;
}

/** A run kind as people say it. */
export function kindText(kind: string): string {
  return kind.replaceAll('_', ' ');
}

/** A rate from 0 to 1 as a whole percentage; a dash when there is none. */
export function ratePercent(rate: number | null): string {
  return rate === null ? '-' : `${Math.round(rate * 100)}%`;
}

/** The periods the quality page offers, by query value. */
export const PERIODS: Record<string, { label: string; days: number | null }> = {
  '7d': { label: 'last 7 days', days: 7 },
  '30d': { label: 'last 30 days', days: 30 },
  all: { label: 'all time', days: null },
};

/** The start of a period ending `now`, as RFC 3339; none for all time. */
export function periodSince(period: string, now: Date = new Date()): string | null {
  const days = PERIODS[period]?.days ?? null;
  if (days === null) return null;
  return new Date(now.getTime() - days * 24 * 60 * 60 * 1000).toISOString();
}

/** How a tool call ended, for its colour: `ok`, `error`, `refused` or `other`. */
export function outcomeClass(outcome: string): 'ok' | 'error' | 'refused' | 'other' {
  if (outcome === 'ok' || outcome === 'error') return outcome;
  if (outcome === 'refused_scope' || outcome === 'refused_repeat') return 'refused';
  return 'other';
}

/** Milliseconds per call, rounded; a dash without calls. */
export function average(totalMs: number, calls: number): string {
  return calls === 0 ? '-' : `${Math.round(totalMs / calls)} ms`;
}
