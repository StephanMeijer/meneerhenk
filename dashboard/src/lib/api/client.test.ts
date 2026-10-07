import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { ApiError, browser, forget, getJson, health, loginUrl, postJson } from './client';

type Call = { url: string; init: RequestInit };

function answer(status: number, body: unknown): Response {
  return new Response(body === undefined ? '' : JSON.stringify(body), {
    status,
    headers: { 'content-type': 'application/json' },
  });
}

let calls: Call[];
let replies: Response[];

beforeEach(() => {
  calls = [];
  replies = [];
  forget();
  vi.stubGlobal('fetch', async (url: string, init: RequestInit) => {
    calls.push({ url, init });
    const reply = replies.shift();
    if (reply === undefined) throw new Error(`no reply for ${url}`);
    return reply;
  });
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe('reads', () => {
  it('ask the API with the session cookie', async () => {
    replies.push(answer(200, { checks: [] }));
    expect(await health()).toEqual({ checks: [] });
    expect(calls[0]?.url).toBe('/dashboard/api/v1/health');
    expect(calls[0]?.init.credentials).toBe('same-origin');
    expect(calls[0]?.init.method).toBeUndefined();
  });

  it('turn an error body into an ApiError with its code', async () => {
    replies.push(answer(404, { error: { code: 'not_found', message: 'No such run.' } }));
    const error = await getJson('/runs/r-x').catch((e: unknown) => e);
    expect(error).toBeInstanceOf(ApiError);
    expect(error).toMatchObject({ status: 404, code: 'not_found', message: 'No such run.' });
  });

  it('say what happened when the answer is not JSON', async () => {
    replies.push(new Response('<html>bad gateway</html>', { status: 502 }));
    await expect(getJson('/health')).rejects.toMatchObject({ status: 502, code: 'http' });
  });

  it('send the browser to sign in on a 401, and back here after', async () => {
    const go = vi.spyOn(browser, 'go').mockImplementation(() => {});
    vi.spyOn(browser, 'here').mockReturnValue('/dashboard/app/runs/r-1?x=1');
    replies.push(answer(401, { error: { code: 'unauthenticated', message: 'Sign in first.' } }));
    await expect(health()).rejects.toMatchObject({ status: 401 });
    expect(go).toHaveBeenCalledWith('/dashboard/login?next=%2Fdashboard%2Fapp%2Fruns%2Fr-1%3Fx%3D1');
  });
});

describe('actions', () => {
  it('carry the CSRF token from /me and a JSON body', async () => {
    replies.push(answer(200, { github_id: 1, login: 'alice', csrf: 'tok-1' }));
    replies.push(answer(202, { event_id: 'e-1' }));
    const started = await postJson('/runs', { kind: 'review', url: 'https://github.com/o/r/pull/7' });
    expect(started).toEqual({ event_id: 'e-1' });
    expect(calls.map((c) => c.url)).toEqual(['/dashboard/api/v1/me', '/dashboard/api/v1/runs']);
    const post = calls[1]?.init;
    expect(post?.method).toBe('POST');
    expect(post?.headers).toMatchObject({ 'content-type': 'application/json', 'x-csrf-token': 'tok-1' });
    expect(JSON.parse(String(post?.body))).toEqual({ kind: 'review', url: 'https://github.com/o/r/pull/7' });
  });
});

describe('loginUrl', () => {
  it('keeps the way back inside one query value', () => {
    expect(loginUrl('/dashboard/app/?a=1&b=2')).toBe('/dashboard/login?next=%2Fdashboard%2Fapp%2F%3Fa%3D1%26b%3D2');
  });
});
