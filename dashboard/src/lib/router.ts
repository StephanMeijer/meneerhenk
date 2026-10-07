// The app's routes, on the History API under BASE. Small on purpose: a
// route is a name and its parameters, and a view per name in App.svelte.
import { writable } from 'svelte/store';

export const BASE = '/dashboard/app';

export type Route =
  | { name: 'home' }
  | { name: 'health' }
  | { name: 'not_found'; path: string };

/** The route a location path names. */
export function routeOf(pathname: string): Route {
  if (pathname !== BASE && !pathname.startsWith(`${BASE}/`)) {
    return { name: 'not_found', path: pathname };
  }
  const path = pathname.slice(BASE.length).replace(/\/+$/, '') || '/';
  switch (path) {
    case '/':
      return { name: 'home' };
    case '/health':
      return { name: 'health' };
    default:
      return { name: 'not_found', path };
  }
}

/** A path inside the app as a location path. */
export function href(path: string): string {
  return BASE + (path.startsWith('/') ? path : `/${path}`);
}

export const route = writable<Route>({ name: 'home' });

/** Follows the address bar, now and on back and forward. */
export function start(): () => void {
  const update = (): void => route.set(routeOf(window.location.pathname));
  update();
  window.addEventListener('popstate', update);
  return () => window.removeEventListener('popstate', update);
}

/** Goes to a path inside the app without a page load. */
export function navigate(path: string): void {
  const to = href(path);
  window.history.pushState({}, '', to);
  route.set(routeOf(to));
}

/** A Svelte action for links inside the app: no page load, and a plain
 * click only; a new tab or a download stays the browser's. */
export function link(node: HTMLAnchorElement): { destroy: () => void } {
  const click = (event: MouseEvent): void => {
    const url = new URL(node.href, window.location.href);
    const plain =
      event.button === 0 && !event.metaKey && !event.ctrlKey && !event.shiftKey && !event.altKey;
    const inside = url.pathname === BASE || url.pathname.startsWith(`${BASE}/`);
    if (!plain || url.origin !== window.location.origin || !inside) {
      return;
    }
    event.preventDefault();
    window.history.pushState({}, '', url.pathname);
    route.set(routeOf(url.pathname));
  };
  node.addEventListener('click', click);
  return { destroy: () => node.removeEventListener('click', click) };
}
