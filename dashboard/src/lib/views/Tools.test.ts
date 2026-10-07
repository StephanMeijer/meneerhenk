import { afterEach, describe, expect, it, vi } from 'vitest';
import type { Page, ToolCallItem, ToolSummaryRow } from '$lib/api/types';
import { cleanup, render, rows, settle } from '$lib/testing/render';
import Tools from './Tools.svelte';

afterEach(cleanup);

const NOW = new Date('2026-10-07T12:00:00Z');

const summaryRow = (tool: string, calls: number, errors: number, refusals: number): ToolSummaryRow => ({
  tool,
  model: 'mistral-medium-3-5',
  session_kind: 'lane',
  calls,
  errors,
  refusals,
  other: 0,
  total_ms: calls * 40,
  error_rate: errors / calls,
  refusal_rate: refusals / calls,
});

const item: ToolCallItem = {
  run_id: 'r-70',
  repo: 'StephanMeijer/meneerhenk',
  target: 70,
  target_url: 'https://github.com/StephanMeijer/meneerhenk/pull/70',
  call: {
    at: '2026-10-06T12:40:00Z',
    session: 'lane-b',
    model: 'mistral-medium-3-5',
    turn: 12,
    tool: 'github__get_file_contents',
    origin: 'github',
    outcome: 'refused_scope',
    arguments: '{"owner":"<script>alert(1)</script>","path":"x"}',
    arguments_len: 48,
    result_chars: 90,
    elapsed_ms: 1,
  },
};

describe('Tools', () => {
  it('shows each tool with its error and refusal rates, and what went wrong as text', async () => {
    const loadSummary = vi.fn(() => Promise.resolve([summaryRow('read_file', 40, 4, 0), summaryRow('github__get_file_contents', 10, 0, 6)]));
    const loadCalls = vi.fn((): Promise<Page<ToolCallItem>> => Promise.resolve({ items: [item], next: null }));
    render(Tools, { query: new URLSearchParams(), loadSummary, loadCalls, now: () => NOW });
    await settle();

    expect(loadSummary).toHaveBeenCalledWith('since=2026-09-07T12%3A00%3A00.000Z');
    expect(loadCalls).toHaveBeenCalledWith('outcome=problems&since=2026-09-07T12%3A00%3A00.000Z');
    expect(rows('table.rates tbody tr')[0]).toEqual([
      'read_file', 'mistral-medium-3-5', 'lane', '40', '4', '0', '0', '40 ms', '10%', '0%',
    ]);
    expect(rows('table.rates tbody tr')[1]?.[9]).toBe('60%');
    expect(document.querySelectorAll('table.rates meter')[3]?.getAttribute('title')).toBe('6 of 10 calls');
    const toolLink = document.querySelector<HTMLAnchorElement>('table.rates a');
    expect(toolLink?.getAttribute('href')).toBe(
      '/dashboard/tools?tool=read_file&model=mistral-medium-3-5&session_kind=lane&outcome=all',
    );

    const call = document.querySelector('ul.calls li');
    expect(call?.querySelector('pre')?.textContent).toBe('{"owner":"<script>alert(1)</script>","path":"x"}');
    expect(document.querySelector('ul.calls script')).toBeNull();
    expect(call?.querySelector('.outcome')?.textContent).toBe('refused scope');
    expect(call?.querySelector('.outcome')?.classList.contains('refused')).toBe(true);
    expect(call?.querySelector('a[title="That turn of the conversation"]')?.getAttribute('href')).toBe(
      '/dashboard/runs/r-70/transcripts/lane-b#turn-12',
    );
  });

  it('asks for the period, kind, model, tool and outcome the query names', async () => {
    const loadSummary = vi.fn(() => Promise.resolve([]));
    const loadCalls = vi.fn((): Promise<Page<ToolCallItem>> => Promise.resolve({ items: [], next: null }));
    render(Tools, {
      query: new URLSearchParams('period=all&session_kind=check&model=opus&tool=bash&outcome=all&cursor=C'),
      loadSummary,
      loadCalls,
      now: () => NOW,
    });
    await settle();
    expect(loadSummary).toHaveBeenCalledWith('model=opus&session_kind=check');
    expect(loadCalls).toHaveBeenCalledWith('tool=bash&model=opus&session_kind=check&cursor=C');
    expect(document.body.textContent).toContain('No tool calls in this period.');
    expect(document.querySelector('h2')?.textContent?.replace(/\s+/g, ' ').trim()).toBe('Every call with bash by opus');
  });

  it('falls back to the defaults for values it does not know', async () => {
    const loadSummary = vi.fn(() => Promise.resolve([]));
    const loadCalls = vi.fn((): Promise<Page<ToolCallItem>> => Promise.resolve({ items: [], next: null }));
    render(Tools, { query: new URLSearchParams('period=forever&session_kind=robot'), loadSummary, loadCalls, now: () => NOW });
    await settle();
    expect(loadSummary).toHaveBeenCalledWith('since=2026-09-07T12%3A00%3A00.000Z');
  });
});
