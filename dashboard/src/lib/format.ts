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
