import { describe, expect, it } from 'vitest';
import { BASE, eventPath, href, isApp, routeOf, runPath, transcriptPath, withQuery } from './router';

describe('routeOf', () => {
  it('finds every page, with or without a trailing slash', () => {
    expect(routeOf(BASE)).toMatchObject({ name: 'overview' });
    expect(routeOf(`${BASE}/`)).toMatchObject({ name: 'overview' });
    expect(routeOf(`${BASE}/health/`)).toEqual({ name: 'health' });
    expect(routeOf(`${BASE}/events`)).toMatchObject({ name: 'events' });
    expect(routeOf(`${BASE}/events/e-1`)).toEqual({ name: 'event', id: 'e-1' });
    expect(routeOf(`${BASE}/quality`, '?group=lane')).toMatchObject({ name: 'quality' });
    expect(routeOf(`${BASE}/runs/r-1`)).toEqual({ name: 'run', id: 'r-1' });
    expect(routeOf(`${BASE}/runs/r-1/transcripts/lane.a`)).toEqual({
      name: 'transcript',
      id: 'r-1',
      session: 'lane.a',
    });
  });

  it('decodes parameters and keeps the query', () => {
    expect(routeOf(`${BASE}/runs/r%2D1/transcripts/check%201`)).toEqual({
      name: 'transcript',
      id: 'r-1',
      session: 'check 1',
    });
    const overview = routeOf(`${BASE}/`, '?kind=plan&cursor=abc');
    expect(overview.name === 'overview' && overview.query.get('kind')).toBe('plan');
  });

  it('names what it does not know', () => {
    expect(routeOf(`${BASE}/nope`)).toEqual({ name: 'not_found', path: '/nope' });
    expect(routeOf(`${BASE}/runs`)).toEqual({ name: 'not_found', path: '/runs' });
    expect(routeOf(`${BASE}/runs/%E0%A4%A`)).toMatchObject({ name: 'not_found' });
    expect(routeOf('/dashboards')).toMatchObject({ name: 'not_found' });
  });
});

describe('paths', () => {
  it('put app paths under the base, encoded', () => {
    expect(href('/')).toBe('/dashboard/');
    expect(href('/events')).toBe('/dashboard/events');
    expect(runPath('r 1')).toBe('/runs/r%201');
    expect(eventPath('e-1')).toBe('/events/e-1');
    expect(transcriptPath('r-1', 'lane/a')).toBe('/runs/r-1/transcripts/lane%2Fa');
  });

  it('leave empty query values out', () => {
    expect(withQuery('/', { kind: 'plan', repo: ' ', status: null })).toBe('/?kind=plan');
    expect(withQuery('/events', {})).toBe('/events');
  });

  it('know the server routes are not the app', () => {
    expect(isApp('/dashboard/runs/r-1')).toBe(true);
    expect(isApp('/dashboard')).toBe(true);
    for (const path of ['/dashboard/api/v1/me', '/dashboard/login', '/dashboard/auth/callback', '/dashboard/logout', '/runs/r-1']) {
      expect(isApp(path)).toBe(false);
    }
  });
});
