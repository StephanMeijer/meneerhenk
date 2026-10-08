import { afterEach, describe, expect, it, vi } from 'vitest';
import { ApiError } from '$lib/api/client';
import { cleanup, render, settle } from '$lib/testing/render';
import StartForm from './StartForm.svelte';

afterEach(cleanup);

function fill(name: string, value: string): void {
  const field = document.querySelector<HTMLInputElement | HTMLTextAreaElement>(`[name=${name}]:not([type=radio])`);
  if (field === null) throw new Error(`no field ${name}`);
  field.value = value;
  field.dispatchEvent(new Event('input', { bubbles: true }));
}

function choose(kind: string): void {
  const radio = document.querySelector<HTMLInputElement>(`input[type=radio][value=${kind}]`);
  if (radio === null) throw new Error(`no choice ${kind}`);
  radio.click();
}

const button = (): HTMLButtonElement | null => document.querySelector('button.primary');
const check = (): string => (document.getElementById('start-url-check')?.textContent ?? '').trim();
const label = (): string =>
  (document.querySelector('[name=url]')?.closest('label')?.querySelector('span')?.textContent ?? '').trim();

describe('StartForm', () => {
  it('offers only what this Henk can start, each with what it does', async () => {
    render(StartForm, { startable: ['review', 'plan'], start: vi.fn(), go: vi.fn() });
    await settle();
    const kinds = [...document.querySelectorAll<HTMLInputElement>('input[type=radio]')].map((r) => r.value);
    expect(kinds).toEqual(['review', 'plan']);
    expect(document.body.textContent).toContain('Lanes review a commit; findings are posted as comments.');
    expect(document.querySelector<HTMLInputElement>('input[value=review]')?.checked).toBe(true);
    expect(label()).toBe('Pull request URL');
    expect(button()?.textContent).toBe('Start review');
  });

  it('says what the URL names, and why it does not fit, as you type', async () => {
    render(StartForm, { startable: ['review', 'plan', 'address'], start: vi.fn(), go: vi.fn() });
    await settle();
    fill('url', 'https://github.com/docspec/app/pull/7');
    await settle(1);
    expect(check()).toBe('pull request docspec/app #7');
    expect(button()?.disabled).toBe(false);

    choose('plan');
    await settle(1);
    expect(label()).toBe('Issue URL');
    expect(check()).toBe('This is a pull request. A plan needs an issue URL.');
    expect(button()?.disabled).toBe(true);
    expect(button()?.textContent).toBe('Start plan');
    expect(document.querySelector('[name=commit]')).toBeNull();
    expect(document.querySelector('[name=note]')).not.toBeNull();
  });

  it('posts the request as JSON and opens the event it became', async () => {
    const start = vi.fn(() => Promise.resolve({ event_id: 'e-9' }));
    const go = vi.fn();
    render(StartForm, { startable: ['review', 'plan', 'address'], start, go });
    await settle();
    choose('plan');
    await settle(1);
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

  it('sends a review its commit and no note', async () => {
    const start = vi.fn(() => Promise.resolve({ event_id: 'e-1' }));
    render(StartForm, { startable: ['review'], start, go: vi.fn() });
    await settle();
    fill('url', 'https://gitlab.com/g/p/-/merge_requests/5');
    fill('commit', ' abc1234 ');
    await settle(1);
    expect(check()).toBe('merge request g/p #5');
    document.querySelector('form')?.requestSubmit();
    await settle();
    expect(start).toHaveBeenCalledWith({
      kind: 'review',
      url: 'https://gitlab.com/g/p/-/merge_requests/5',
      commit: 'abc1234',
      note: null,
    });
  });

  it("keeps the dialog open with the API's reason when it refuses", async () => {
    const start = vi.fn(() =>
      Promise.reject(new ApiError(400, 'bad_request', 'docspec/secret is not on the allowlist.')),
    );
    const go = vi.fn();
    render(StartForm, { startable: ['review'], start, go });
    await settle();
    fill('url', 'https://github.com/docspec/secret/pull/1');
    await settle(1);
    document.querySelector('form')?.requestSubmit();
    await settle();
    expect(document.querySelector('[role=alert]')?.textContent?.trim()).toBe(
      'Not started. docspec/secret is not on the allowlist.',
    );
    expect(go).not.toHaveBeenCalled();
    expect(button()?.disabled).toBe(false);
    expect([...document.querySelectorAll('button')].some((b) => b.textContent === 'Reload')).toBe(false);
  });

  it('offers a reload when the session token is stale', async () => {
    const start = vi.fn(() =>
      Promise.reject(new ApiError(403, 'csrf', 'The CSRF token is missing or not from this session. Reload and try again.')),
    );
    const reload = vi.fn();
    render(StartForm, { startable: ['review'], start, go: vi.fn(), reload });
    await settle();
    fill('url', 'https://github.com/docspec/app/pull/7');
    await settle(1);
    document.querySelector('form')?.requestSubmit();
    await settle();
    const again = [...document.querySelectorAll('button')].find((b) => b.textContent === 'Reload');
    expect(again).toBeDefined();
    again?.click();
    expect(reload).toHaveBeenCalledTimes(1);
  });
});
