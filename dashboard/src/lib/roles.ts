// The roles a session runs in (#326): planner, reviewer and executor, and
// the history roles older runs keep: a review lane, a fact-check, and an
// address run's session (#324).

/** Every role, as the run page and the sessions page order them. */
export const ROLE_ORDER = ['planner', 'reviewer', 'executor', 'lane', 'check', 'address'] as const;

/** The roles of runs from before the review loop, #309 and #327. */
const HISTORY = new Set(['lane', 'check', 'address']);

const LABELS: Record<string, string> = {
  planner: 'Planner',
  reviewer: 'Reviewer',
  executor: 'Executor',
  lane: 'Review lane',
  check: 'Fact-check',
  address: 'Address',
};

/** A role as people say it; an unknown role as sent. */
export function roleLabel(role: string): string {
  return LABELS[role] ?? role;
}

/** Whether only older runs have sessions in `role`. */
export function isHistory(role: string): boolean {
  return HISTORY.has(role);
}

/** Where `role` comes in `ROLE_ORDER`; an unknown role last. */
export function roleRank(role: string): number {
  const at = (ROLE_ORDER as readonly string[]).indexOf(role);
  return at === -1 ? ROLE_ORDER.length : at;
}

/** Whether a session shows among a review's rounds: a fact-check of an
 * older review ran after them, so it does not. */
export function shownInRounds(session: { role: string }): boolean {
  return session.role !== 'check';
}
