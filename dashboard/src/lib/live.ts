// Applying stream messages to what a page shows (#202). Pure: each takes
// the state and a message and returns the new state.
import type { RunDetail, RunMessage, RunningMessage, RunSummary, ToolCall } from './api/types';

const REFUSED = new Set(['refused_scope', 'refused_repeat']);

function addCall(detail: RunDetail, call: ToolCall): RunDetail {
  const tally = { errors: 0, refusals: 0, other: 0 };
  if (call.outcome === 'error') tally.errors = 1;
  else if (REFUSED.has(call.outcome)) tally.refusals = 1;
  else if (call.outcome !== 'ok') tally.other = 1;
  const found = detail.tool_usage.some((row) => row.session === call.session && row.tool === call.tool);
  const tool_usage = found
    ? detail.tool_usage.map((row) =>
        row.session === call.session && row.tool === call.tool
          ? {
              ...row,
              calls: row.calls + 1,
              errors: row.errors + tally.errors,
              refusals: row.refusals + tally.refusals,
              other: row.other + tally.other,
              total_ms: row.total_ms + call.elapsed_ms,
            }
          : row,
      )
    : [...detail.tool_usage, { session: call.session, tool: call.tool, calls: 1, ...tally, total_ms: call.elapsed_ms }];
  const lanes = detail.lanes.map((lane) =>
    lane.name === call.session ? { ...lane, last_call_turn: Math.max(lane.last_call_turn ?? 0, call.turn) } : lane,
  );
  return { ...detail, tool_usage, lanes };
}

/** A run's page after one message of its stream. */
export function applyRun(detail: RunDetail, message: RunMessage): RunDetail {
  switch (message.kind) {
    case 'snapshot':
      return message.data;
    case 'run': {
      const { run, summary, error, check_id } = message.data;
      return { ...detail, run, summary, error, check_id };
    }
    case 'lanes':
      return {
        ...detail,
        lanes: message.data.map((lane) => ({
          ...lane,
          last_call_turn:
            lane.last_call_turn ?? detail.lanes.find((old) => old.name === lane.name)?.last_call_turn ?? null,
        })),
      };
    case 'tool_call':
      return addCall(detail, message.data);
    case 'draft': {
      const draft = message.data;
      const known = detail.drafts.some((d) => d.id === draft.id);
      return {
        ...detail,
        drafts: known ? detail.drafts.map((d) => (d.id === draft.id ? draft : d)) : [...detail.drafts, draft],
      };
    }
    case 'finding':
      return { ...detail, findings: [...detail.findings, message.data] };
    case 'event':
      return { ...detail, events: [...detail.events, message.data] };
    case 'transcript':
      return detail.transcripts.some((t) => t.session === message.data.session)
        ? detail
        : { ...detail, transcripts: [...detail.transcripts, message.data] };
    case 'end':
      return detail;
  }
}

/** What runs now: the newest running runs and how many run in all. */
export type Running = { runs: RunSummary[]; count: number };

/** "Running now" after one message of `/runs/stream`. */
export function applyRunning(state: Running, message: RunningMessage): Running {
  if (message.kind === 'snapshot') {
    return { runs: message.data.runs, count: message.data.count };
  }
  const run = message.data;
  const known = state.runs.some((r) => r.id === run.id);
  if (run.status === 'running') {
    return known
      ? { ...state, runs: state.runs.map((r) => (r.id === run.id ? run : r)) }
      : { runs: [run, ...state.runs], count: state.count + 1 };
  }
  return known
    ? { runs: state.runs.filter((r) => r.id !== run.id), count: Math.max(0, state.count - 1) }
    : state;
}

/** A running run's stream as the page shows it (#224): `connecting` until
 * it first opens, `live` while open, `reconnecting` when it dropped, and
 * `ended` once the run ended or the page opened on a finished run. */
export type Connection = 'connecting' | 'live' | 'reconnecting' | 'ended';

/** The connection after the stream opened (`true`) or dropped (`false`). */
export function connectionAfter(connection: Connection, open: boolean): Connection {
  if (connection === 'ended') return 'ended';
  if (open) return 'live';
  return connection === 'connecting' ? 'connecting' : 'reconnecting';
}
