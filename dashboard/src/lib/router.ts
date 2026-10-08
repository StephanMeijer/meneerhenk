// The dashboard's routes, on the History API under BASE. A route is a name
// with its parameters; App.svelte has a view per name. The paths are the
// ones the server-rendered pages had, so links posted before still work.
import { writable } from 'svelte/store';

export const BASE = '/dashboard';

export type Route =
  | { name: 'overview'; query: URLSearchParams }
  | { name: 'run'; id: string }
  | { name: 'transcript'; id: string; session: string }
  | { name: 'events'; query: URLSearchParams }
  | { name: 'event'; id: string; query: URLSearchParams }
  | { name: 'health' }
  | { name: 'quality'; query: URLSearchParams }
  | { name: 'tools'; query: URLSearchParams }
  | { name: 'not_found'; path: string };

/** Paths under BASE that are the server's, not the app's. */
const SERVER = ['/api', '/login', '/auth', '/logout'];

function decode(part: string): string | null {
  try {
    const text = decodeURIComponent(part);
    return text === '' ? null : text;
  } catch {
    return null;
  }
}

/** The route a location path and query name. */
export function routeOf(pathname: string, search = ''): Route {
  if (pathname !== BASE && !pathname.startsWith(`${BASE}/`)) {
    return { name: 'not_found', path: pathname };
  }
  const path = pathname.slice(BASE.length).replace(/\/+$/, '') || '/';
  const query = new URLSearchParams(search);
  const parts = path.split('/').slice(1).map(decode);
  const [first, second, third, fourth] = parts;
  if (path === '/') return { name: 'overview', query };
  if (path === '/health') return { name: 'health' };
  if (path === '/events') return { name: 'events', query };
  if (path === '/quality') return { name: 'quality', query };
  if (path === '/tools') return { name: 'tools', query };
  if (parts.length === 2 && first === 'events' && second) return { name: 'event', id: second, query };
  if (parts.length === 2 && first === 'runs' && second) return { name: 'run', id: second };
  if (parts.length === 4 && first === 'runs' && second && third === 'transcripts' && fourth) {
    return { name: 'transcript', id: second, session: fourth };
  }
  return { name: 'not_found', path };
}

/** A path inside the app (`/runs/r-1?x=1`) as a location path. */
export function href(path: string): string {
  const inside = path.startsWith('/') ? path : `/${path}`;
  return inside === '/' ? `${BASE}/` : BASE + inside;
}

export const runPath = (id: string): string => `/runs/${encodeURIComponent(id)}`;
export const eventPath = (id: string): string => `/events/${encodeURIComponent(id)}`;
export const transcriptPath = (id: string, session: string, turn?: number): string =>
  `${runPath(id)}/transcripts/${encodeURIComponent(session)}${turn === undefined ? '' : `#turn-${turn}`}`;

/** A path with its query, leaving out empty values. */
export function withQuery(path: string, query: Record<string, string | null | undefined>): string {
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(query)) {
    if (value !== null && value !== undefined && value.trim() !== '') params.set(key, value.trim());
  }
  const text = params.toString();
  return text === '' ? path : `${path}?${text}`;
}

const here = (): Route => routeOf(window.location.pathname, window.location.search);

export const route = writable<Route>({ name: 'overview', query: new URLSearchParams() });

/** Follows the address bar, now and on back and forward. */
export function start(): () => void {
  const update = (): void => route.set(here());
  update();
  window.addEventListener('popstate', update);
  return () => window.removeEventListener('popstate', update);
}

/** Goes to a path inside the app without a page load. `keepScroll` stays
 * where the page is, for choosing something on it (#227). */
export function navigate(path: string, { keepScroll = false }: { keepScroll?: boolean } = {}): void {
  window.history.pushState({}, '', href(path));
  route.set(here());
  if (!keepScroll) window.scrollTo?.(0, 0);
}

/** Whether a location path is the app's, not the server's. */
export function isApp(pathname: string): boolean {
  if (pathname !== BASE && !pathname.startsWith(`${BASE}/`)) return false;
  const rest = pathname.slice(BASE.length);
  return !SERVER.some((server) => rest === server || rest.startsWith(`${server}/`));
}

/** A Svelte action for links inside the app: no page load on a plain
 * click; a new tab or a download stays the browser's. `keepScroll` stays
 * where the page is, as `navigate` does (#227). */
export function link(
  node: HTMLAnchorElement,
  options: { keepScroll?: boolean } = {},
): { update: (next?: { keepScroll?: boolean }) => void; destroy: () => void } {
  let keepScroll = options.keepScroll ?? false;
  const click = (event: MouseEvent): void => {
    const url = new URL(node.href, window.location.href);
    const plain =
      event.button === 0 && !event.metaKey && !event.ctrlKey && !event.shiftKey && !event.altKey;
    if (!plain || url.origin !== window.location.origin || !isApp(url.pathname)) return;
    event.preventDefault();
    window.history.pushState({}, '', url.pathname + url.search + url.hash);
    route.set(here());
    if (!keepScroll) window.scrollTo?.(0, 0);
  };
  node.addEventListener('click', click);
  return {
    update: (next = {}) => {
      keepScroll = next.keepScroll ?? false;
    },
    destroy: () => node.removeEventListener('click', click),
  };
}
