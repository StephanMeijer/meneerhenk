import { describe, expect, it } from 'vitest';
import { BASE, href, routeOf } from './router';

describe('routeOf', () => {
  it('finds the app routes under the base, with or without a trailing slash', () => {
    expect(routeOf(BASE)).toEqual({ name: 'home' });
    expect(routeOf(`${BASE}/`)).toEqual({ name: 'home' });
    expect(routeOf(`${BASE}/health`)).toEqual({ name: 'health' });
    expect(routeOf(`${BASE}/health/`)).toEqual({ name: 'health' });
  });

  it('names what it does not know', () => {
    expect(routeOf(`${BASE}/nope`)).toEqual({ name: 'not_found', path: '/nope' });
    expect(routeOf('/dashboard/application')).toEqual({ name: 'not_found', path: '/dashboard/application' });
    expect(routeOf('/elsewhere')).toEqual({ name: 'not_found', path: '/elsewhere' });
  });
});

describe('href', () => {
  it('puts a path under the base', () => {
    expect(href('/health')).toBe('/dashboard/app/health');
    expect(href('health')).toBe('/dashboard/app/health');
  });
});
