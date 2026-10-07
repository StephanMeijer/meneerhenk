// One status, one colour, one icon, everywhere (#224). Every status word
// the API sends maps to a tone, which the stylesheet colours, and an icon;
// the word itself is always shown, so colour is never the only signal.

/** What a status colours as; `app.css` has a `.tone-*` rule for each. */
export type Tone = 'accent' | 'pending' | 'ok' | 'fail' | 'warn' | 'drop' | 'refused' | 'neutral';

/** The icons `Icon.svelte` draws. */
export type IconName =
  | 'spinner'
  | 'circle'
  | 'check'
  | 'cross'
  | 'slash'
  | 'minus'
  | 'broken'
  | 'hourglass'
  | 'triangle'
  | 'shield'
  | 'clock'
  | 'link'
  | 'pencil'
  | 'undo'
  | 'copy'
  | 'menu'
  | 'plus'
  | 'info';

export interface Look {
  tone: Tone;
  icon: IconName;
  /** The word as people say it: `did not finish`, not `did_not_finish`. */
  label: string;
}

const look = (tone: Tone, icon: IconName): Omit<Look, 'label'> => ({ tone, icon });

/** Runs, lanes, stages, health checks and timeline levels. */
const STATES: Record<string, Omit<Look, 'label'>> = {
  running: look('accent', 'spinner'),
  pending: look('pending', 'circle'),
  queued: look('pending', 'circle'),
  finished: look('ok', 'check'),
  done: look('ok', 'check'),
  ok: look('ok', 'check'),
  info: look('neutral', 'info'),
  failed: look('fail', 'cross'),
  fail: look('fail', 'cross'),
  error: look('fail', 'cross'),
  cancelled: look('neutral', 'slash'),
  superseded: look('neutral', 'minus'),
  did_not_finish: look('drop', 'broken'),
  dropped: look('drop', 'broken'),
  timed_out: look('warn', 'hourglass'),
  warn: look('warn', 'triangle'),
  refused: look('refused', 'shield'),
  waiting: look('neutral', 'clock'),
};

/** What the fact-check decided about a draft. */
const VERDICTS: Record<string, Omit<Look, 'label'>> = {
  confirmed: look('ok', 'check'),
  rejected: look('fail', 'cross'),
  same_as: look('neutral', 'link'),
  unchecked: look('warn', 'triangle'),
  not_checked: look('neutral', 'minus'),
  cancelled: look('neutral', 'slash'),
  failed: look('fail', 'cross'),
  waiting: look('neutral', 'clock'),
};

/** What happened to a finding on the pull request. */
const ACTIONS: Record<string, Omit<Look, 'label'>> = {
  posted: look('ok', 'check'),
  improved: look('ok', 'pencil'),
  unverified: look('warn', 'triangle'),
  refused: look('refused', 'shield'),
  rejected: look('fail', 'cross'),
  withdrawn: look('neutral', 'undo'),
  merged: look('neutral', 'link'),
};

/** How one tool call ended. */
const OUTCOMES: Record<string, Omit<Look, 'label'>> = {
  ok: look('ok', 'check'),
  error: look('fail', 'cross'),
  refused_scope: look('refused', 'shield'),
  refused_repeat: look('refused', 'shield'),
  malformed_arguments: look('warn', 'triangle'),
  unknown_tool: look('warn', 'triangle'),
  not_run: look('neutral', 'minus'),
  cancelled: look('neutral', 'slash'),
};

export const VOCABULARIES = {
  state: STATES,
  verdict: VERDICTS,
  action: ACTIONS,
  outcome: OUTCOMES,
} as const;

export type Vocabulary = keyof typeof VOCABULARIES;

/** A status word as people say it. */
export function statusLabel(word: string): string {
  return word.replaceAll('_', ' ');
}

/** How `word` looks in `vocabulary`; a word Henk does not know yet is
 * neutral and shown as it is. */
export function lookOf(word: string, vocabulary: Vocabulary = 'state'): Look {
  const table = VOCABULARIES[vocabulary];
  // Own keys only: a word such as `constructor` is not a status.
  const known = (Object.hasOwn(table, word) ? table[word] : undefined) ?? look('neutral', 'circle');
  return { ...known, label: statusLabel(word) };
}
