import { afterEach, describe, expect, it } from 'vitest';
import type { Transcript as Conversation } from '$lib/api/types';
import { cleanup, render, settle } from '$lib/testing/render';
import Transcript from './Transcript.svelte';

afterEach(cleanup);

describe('Transcript', () => {
  it('folds a long result and shows a short one', async () => {
    const long = Array.from({ length: 13 }, (_, i) => `line ${i}`).join('\n');
    const conversation: Conversation = {
      session: 'lane-a',
      model: 'model-x',
      stop: 'EndTurn',
      turns: 1,
      prompt_tokens: 15,
      output_tokens: 3,
      system: 'You review.',
      messages: [
        { role: 'user', turn: 0, parts: [{ type: 'text', text: 'Review <this>.' }] },
        { role: 'assistant', turn: 1, parts: [{ type: 'call', name: 'read_file', arguments: '{}' }, { type: 'opaque' }] },
        {
          role: 'user',
          turn: 1,
          parts: [
            { type: 'result', error: false, content: long },
            { type: 'result', error: true, content: 'no such file' },
          ],
        },
      ],
    };
    render(Transcript, { id: 'r-1', session: 'lane-a', load: () => Promise.resolve(conversation) });
    await settle();
    expect(document.querySelector('details summary')?.textContent).toBe('Result, 13 lines');
    expect(document.querySelectorAll('details')).toHaveLength(1);
    expect(document.body.textContent).toContain('Result: error');
    expect(document.body.textContent).toContain('Provider content, not shown.');
    expect([...document.querySelectorAll('pre')].map((p) => p.textContent)).toContain('Review <this>.');
  });
});
