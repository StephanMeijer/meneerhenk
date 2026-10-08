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

/** Why a lane did not finish (#229), as people say it. */
export const LANE_REASONS: Record<string, string> = {
  time_limit: 'time limit',
  rate_limit: 'rate limit',
  provider_error: 'provider error',
  cancelled: 'cancelled',
  declined: 'model declined',
  stuck: 'stuck in a loop',
};

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

const SECOND = 1000;
const MINUTE = 60 * SECOND;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

/** How long ago `iso` was, as people say it: `just now`, `6 min ago`,
 * `2 h ago`, `3 d ago`. A time in the future (a clock ahead) is `just now`. */
export function ago(iso: string, now: Date = new Date()): string {
  const then = Date.parse(iso);
  if (Number.isNaN(then)) return iso;
  const past = now.getTime() - then;
  if (past < MINUTE) return 'just now';
  if (past < HOUR) return `${Math.floor(past / MINUTE)} min ago`;
  if (past < DAY) return `${Math.floor(past / HOUR)} h ago`;
  return `${Math.floor(past / DAY)} d ago`;
}

/** The exact time in UTC, to the second: `2026-10-07T10:02:11Z`. */
export function utc(iso: string): string {
  const then = Date.parse(iso);
  if (Number.isNaN(then)) return iso;
  return new Date(then).toISOString().replace(/\.\d{3}Z$/, 'Z');
}

/** The clock time in UTC: `10:02:11 UTC`. */
export function clockTime(iso: string): string {
  const exact = utc(iso);
  const time = /T(\d{2}:\d{2}:\d{2})Z$/.exec(exact)?.[1];
  return time === undefined ? exact : `${time} UTC`;
}

/** A length of time: `0m 41s`, `3m 12s`, `1h 04m`. */
export function duration(ms: number): string {
  const total = Math.max(0, Math.floor(ms / SECOND));
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const seconds = total % 60;
  if (hours > 0) return `${hours}h ${String(minutes).padStart(2, '0')}m`;
  return `${minutes}m ${String(seconds).padStart(2, '0')}s`;
}

/** How long a run took, or has been going while it runs. */
export function runDuration(
  run: { started_at: string; finished_at: string | null },
  now: Date = new Date(),
): string {
  const start = Date.parse(run.started_at);
  const end = run.finished_at === null ? now.getTime() : Date.parse(run.finished_at);
  if (Number.isNaN(start) || Number.isNaN(end)) return '';
  return duration(end - start);
}

/** A commit as people show it: its first 7 characters. */
export function shortCommit(sha: string): string {
  return sha.slice(0, 7);
}

const NUMBER = new Intl.NumberFormat('en-US');

/** A count with thousands separators: `1,284`. */
export function count(n: number): string {
  return NUMBER.format(n);
}

/** A share with its base: `96% of 75`; a dash of nothing. */
export function share(part: number, whole: number): string {
  return whole === 0 ? '-' : `${Math.round((part / whole) * 100)}% of ${count(whole)}`;
}

/** A payload as received, laid out to read: JSON pretty-printed (only
 * re-spaced, never interpreted), anything else as it came (#227). */
export function prettyPayload(text: string): string {
  try {
    return JSON.stringify(JSON.parse(text), null, 2);
  } catch {
    return text;
  }
}
