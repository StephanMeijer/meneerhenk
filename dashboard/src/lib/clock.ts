// The time the page shows relative times against. It ticks every 30
// seconds while something reads it, so "6 min ago" does not go stale.
import { readable } from 'svelte/store';

const TICK = 30_000;

export const now = readable(new Date(), (set) => {
  set(new Date());
  const timer = setInterval(() => set(new Date()), TICK);
  return () => clearInterval(timer);
});
