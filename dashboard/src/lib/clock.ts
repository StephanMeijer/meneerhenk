// The time the page shows times against. Two clocks: `now` ticks every 30
// seconds, for relative times ("6 min ago"); `everySecond` ticks every
// second, for durations of what is still going (#252). Each runs only while
// something reads it, and `everySecond` rests while the tab is hidden.
// Both read the server's time when the browser's clock is off by seconds.
import { readable } from 'svelte/store';

const TICK = 30_000;
const SECOND = 1000;
/** An offset smaller than this is the `Date` header's rounding, not drift. */
const DRIFT = 2000;

/** How far the server's clock is ahead of the browser's, in ms. */
let offset = 0;

/** The time now, as the server counts it. */
export function serverNow(): Date {
  return new Date(Date.now() + offset);
}

/** Takes the server's clock from a response's `Date` header. */
export function noteServerDate(header: string | null): void {
  if (header === null) return;
  const server = Date.parse(header);
  if (Number.isNaN(server)) return;
  const ahead = server - Date.now();
  offset = Math.abs(ahead) < DRIFT ? 0 : ahead;
}

export const now = readable(serverNow(), (set) => {
  set(serverNow());
  const timer = setInterval(() => set(serverNow()), TICK);
  return () => clearInterval(timer);
});

export const everySecond = readable(serverNow(), (set) => {
  let timer: ReturnType<typeof setInterval> | null = null;
  const run = (): void => {
    if (timer !== null) return;
    set(serverNow());
    timer = setInterval(() => set(serverNow()), SECOND);
  };
  const rest = (): void => {
    if (timer !== null) clearInterval(timer);
    timer = null;
  };
  const follow = (): void => (document.visibilityState === 'hidden' ? rest() : run());
  follow();
  document.addEventListener('visibilitychange', follow);
  return () => {
    document.removeEventListener('visibilitychange', follow);
    rest();
  };
});
