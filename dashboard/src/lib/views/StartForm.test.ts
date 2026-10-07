import { afterEach, describe, expect, it, vi } from 'vitest';
import { ApiError } from '$lib/api/client';
import { cleanup, render, settle } from '$lib/testing/render';
import StartForm from './StartForm.svelte';

afterEach(cleanup);

function fill(name: string, value: string): void {
  const field = document.querySelector<HTMLInputElement | HTMLSelectElement>(`[name=${name}]`);
  if (field === null) throw new Error(`no field ${name}`);
  field.value = value;
  field.dispatchEvent(new Event(field instanceof HTMLSelectElement ? 'change' : 'input', { bubbles: true }));
}

describe('StartForm', () => {
  it('offers only what this Henk can start', async () => {
    render(StartForm, { startable: ['review', 'plan'], start: vi.fn(), go: vi.fn() });
    await settle();
    const options = [...document.querySelectorAll('option')].map((o) => o.value);
    expect(options).toEqual(['review', 'plan']);
  });

  it('posts the request as JSON and opens the event it became', async () => {
    const start = vi.fn(() => Promise.resolve({ event_id: 'e-9' }));
    const go = vi.fn();
    render(StartForm, { startable: ['review', 'plan', 'address'], start, go });
    await settle();
    fill('kind', 'plan');
    fill('url', ' https://github.com/docspec/app/issues/3 ');
    fill('note', 'small steps');
    await settle(1);
    document.querySelector('form')?.requestSubmit();
    await settle();
    expect(start).toHaveBeenCalledWith({
      kind: 'plan',
      url: 'https://github.com/docspec/app/issues/3',
      commit: null,
      note: 'small steps',
    });
    expect(go).toHaveBeenCalledWith('/events/e-9');
  });

  it("shows the API's reason when it refuses", async () => {
    const start = vi.fn(() =>
      Promise.reject(new ApiError(400, 'bad_request', 'That is not a pull request URL.')),
    );
    const go = vi.fn();
    render(StartForm, { startable: ['review'], start, go });
    await settle();
    fill('url', 'nope');
    await settle(1);
    document.querySelector('form')?.requestSubmit();
    await settle();
    expect(document.querySelector('[role=alert]')?.textContent).toBe('Not started. That is not a pull request URL.');
    expect(go).not.toHaveBeenCalled();
    expect(document.querySelector('button')?.disabled).toBe(false);
  });
});
