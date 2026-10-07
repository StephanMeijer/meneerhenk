// Renders a component into jsdom for a test, and waits for its promises.
import { flushSync, mount, tick, unmount, type Component } from 'svelte';

let mounted: ReturnType<typeof mount>[] = [];

export function render<Props extends Record<string, unknown>>(
  component: Component<Props>,
  props: Props,
): void {
  mounted.push(mount(component, { target: document.body, props }));
}

/** Lets pending promises and Svelte's updates run. */
export async function settle(times = 3): Promise<void> {
  for (let i = 0; i < times; i += 1) {
    await new Promise((resolve) => setTimeout(resolve, 0));
    await tick();
    flushSync();
  }
}

export function cleanup(): void {
  for (const app of mounted) unmount(app);
  mounted = [];
  document.body.replaceChildren();
}

/** The text of every cell of the table rows, row by row. */
export function rows(selector = 'tbody tr'): string[][] {
  return [...document.querySelectorAll(selector)].map((row) =>
    [...row.querySelectorAll('td')].map((td) => (td.textContent ?? '').trim()),
  );
}
