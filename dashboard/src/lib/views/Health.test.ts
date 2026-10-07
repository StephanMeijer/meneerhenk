import { flushSync, mount, tick, unmount } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';
import { ApiError } from '$lib/api/client';
import type { Health as Checks } from '$lib/api/types';
import Health from './Health.svelte';

const settle = async (): Promise<void> => {
  await new Promise((resolve) => setTimeout(resolve, 0));
  await tick();
  flushSync();
};

let app: ReturnType<typeof mount> | null = null;

afterEach(() => {
  if (app !== null) unmount(app);
  app = null;
  document.body.replaceChildren();
});

describe('Health', () => {
  it('shows stored text as text, never as markup', async () => {
    const checks: Checks = {
      checks: [{ name: 'database', state: 'ok', detail: '<script>alert(1)</script><b>x</b>' }],
    };
    app = mount(Health, { target: document.body, props: { load: () => Promise.resolve(checks) } });
    await settle();
    const cells = [...document.querySelectorAll('td')].map((td) => td.textContent);
    expect(cells).toEqual(['database', 'ok', '<script>alert(1)</script><b>x</b>']);
    expect(document.querySelector('script')).toBeNull();
    expect(document.querySelector('b')).toBeNull();
  });

  it('says why when the API refused', async () => {
    app = mount(Health, {
      target: document.body,
      props: { load: () => Promise.reject(new ApiError(500, 'store', 'The run store failed; the log says why.')) },
    });
    await settle();
    expect(document.querySelector('[role=alert]')?.textContent).toContain('The run store failed');
  });
});
