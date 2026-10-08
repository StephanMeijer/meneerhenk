// The dashboard's only way to Henk: /dashboard/api/v1 (docs/API.md). Reads
// carry the session cookie; actions also the session's CSRF token, which
// /me gives. A 401 sends the browser to sign in and back.
import { noteServerDate } from '../clock';
import type {
  Cancelled,
  DraftItem,
  ErrorBody,
  EventDetail,
  EventFacets,
  EventItem,
  Health,
  LaneStats,
  Me,
  OverviewStats,
  Page,
  QualityRow,
  QualitySeries,
  DraftCount,
  RunCount,
  RunDetail,
  RunSummary,
  StartRequest,
  Started,
  ToolCall,
  ToolCallItem,
  ToolSummaryRow,
  Transcript,
} from './types';

export const API = '/dashboard/api/v1';

/** An error the API answered with, or a failure to reach it. */
export class ApiError extends Error {
  readonly status: number;
  readonly code: string;

  constructor(status: number, code: string, message: string) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.code = code;
  }
}

/** Where the browser goes; a seam for the tests. */
export const browser = {
  here: (): string => window.location.pathname + window.location.search,
  go: (url: string): void => window.location.assign(url),
};

/** Sign-in, coming back to `path` after. */
export function loginUrl(path: string): string {
  return `/dashboard/login?next=${encodeURIComponent(path)}`;
}

let csrf: string | null = null;

async function send<T>(path: string, init: RequestInit = {}): Promise<T> {
  let response: Response;
  try {
    response = await fetch(API + path, {
      ...init,
      credentials: 'same-origin',
      headers: { accept: 'application/json', ...init.headers },
    });
  } catch {
    throw new ApiError(0, 'unreachable', 'Henk did not answer. Is it running?');
  }
  noteServerDate(response.headers.get('date'));
  if (response.status === 401) {
    browser.go(loginUrl(browser.here()));
    throw new ApiError(401, 'unauthenticated', 'Sign in first.');
  }
  const text = await response.text();
  let body: unknown = null;
  try {
    body = text === '' ? null : JSON.parse(text);
  } catch {
    // Not JSON: reported below with the status.
  }
  if (!response.ok) {
    const error = (body as Partial<ErrorBody> | null)?.error;
    throw new ApiError(
      response.status,
      error?.code ?? 'http',
      error?.message ?? `Henk answered ${response.status}.`,
    );
  }
  return body as T;
}

/** A read. */
export function getJson<T>(path: string): Promise<T> {
  return send<T>(path);
}

/** An action, with the session's CSRF token. */
export async function postJson<T>(path: string, body: unknown): Promise<T> {
  if (csrf === null) {
    await me();
  }
  return send<T>(path, {
    method: 'POST',
    headers: { 'content-type': 'application/json', 'x-csrf-token': csrf ?? '' },
    body: JSON.stringify(body),
  });
}

/** Who is signed in; keeps their CSRF token for actions. */
export async function me(): Promise<Me> {
  const who = await send<Me>('/me');
  csrf = who.csrf;
  return who;
}

/** What the service has. */
export function health(): Promise<Health> {
  return getJson<Health>('/health');
}

/** What happened per day over the last `days`, for the overview (#225). */
export function overviewStats(days = 14): Promise<OverviewStats> {
  return getJson<OverviewStats>(`/stats/overview?days=${days}`);
}

/** How the lanes of the recent reviews ended (#229); `query` is
 * `last=30` or `since=...`. */
export function laneStats(query = ''): Promise<LaneStats> {
  return getJson<LaneStats>(`/stats/lanes${query === '' ? '' : `?${query}`}`);
}

/** Runs, newest first; `query` is the `GET /runs` query. */
export function runs(query = ''): Promise<Page<RunSummary>> {
  return getJson<Page<RunSummary>>(`/runs${query === '' ? '' : `?${query}`}`);
}

/** How many runs `query` matches, over every page. */
export function runCount(query = ''): Promise<RunCount> {
  return getJson<RunCount>(`/runs/count${query === '' ? '' : `?${query}`}`);
}

/** One run in full. */
export function run(id: string): Promise<RunDetail> {
  return getJson<RunDetail>(`/runs/${encodeURIComponent(id)}`);
}

/** One session's conversation. */
export function transcript(id: string, session: string): Promise<Transcript> {
  return getJson<Transcript>(
    `/runs/${encodeURIComponent(id)}/transcripts/${encodeURIComponent(session)}`,
  );
}

/** Inbound events, newest first; `query` is the `GET /events` query. */
export function events(query = ''): Promise<Page<EventItem>> {
  return getJson<Page<EventItem>>(`/events${query === '' ? '' : `?${query}`}`);
}

/** The sources and kinds Henk has recorded, for the events filters. */
export function eventFacets(): Promise<EventFacets> {
  return getJson<EventFacets>('/events/facets');
}

/** One inbound event with its payload and outcomes. */
export function event(id: string): Promise<EventDetail> {
  return getJson<EventDetail>(`/events/${encodeURIComponent(id)}`);
}

/** Starts a review, plan or address run. */
export function startRun(request: StartRequest): Promise<Started> {
  return postJson<Started>('/runs', request);
}

/** Cancels a running run. */
export function cancelRun(id: string): Promise<Cancelled> {
  return postJson<Cancelled>(`/runs/${encodeURIComponent(id)}/cancel`, {});
}

/** What became of the drafts per group; `query` is the `GET /quality` query. */
export function quality(query = ''): Promise<QualityRow[]> {
  return getJson<QualityRow[]>(`/quality${query === '' ? '' : `?${query}`}`);
}

/** The rejection rate per day of the largest groups; the `GET /quality` query. */
export function qualityDaily(query = ''): Promise<QualitySeries[]> {
  return getJson<QualitySeries[]>(`/quality/daily${query === '' ? '' : `?${query}`}`);
}

/** How many drafts the `GET /drafts` query matches, over every page. */
export function draftCount(query = ''): Promise<DraftCount> {
  return getJson<DraftCount>(`/drafts/count${query === '' ? '' : `?${query}`}`);
}

/** Drafts across runs, newest first; `query` is the `GET /drafts` query. */
export function drafts(query = ''): Promise<Page<DraftItem>> {
  return getJson<Page<DraftItem>>(`/drafts${query === '' ? '' : `?${query}`}`);
}

/** Tool calls per tool, model and kind of session; `query` is the `GET /tool-calls/summary` query. */
export function toolSummary(query = ''): Promise<ToolSummaryRow[]> {
  return getJson<ToolSummaryRow[]>(`/tool-calls/summary${query === '' ? '' : `?${query}`}`);
}

/** Tool calls across runs, newest first; `query` is the `GET /tool-calls` query. */
export function toolCalls(query = ''): Promise<Page<ToolCallItem>> {
  return getJson<Page<ToolCallItem>>(`/tool-calls${query === '' ? '' : `?${query}`}`);
}

/** One run's tool calls, in order. */
export function runToolCalls(id: string): Promise<ToolCall[]> {
  return getJson<ToolCall[]>(`/runs/${encodeURIComponent(id)}/tool-calls`);
}

/** Forgets the CSRF token; for the tests. */
export function forget(): void {
  csrf = null;
}
