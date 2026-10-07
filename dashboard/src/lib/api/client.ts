// The dashboard's only way to Henk: /dashboard/api/v1 (docs/API.md). Reads
// carry the session cookie; actions also the session's CSRF token, which
// /me gives. A 401 sends the browser to sign in and back.
import type { ErrorBody, Health, Me } from './types';

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

/** Forgets the CSRF token; for the tests. */
export function forget(): void {
  csrf = null;
}
