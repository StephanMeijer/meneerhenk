<script lang="ts">
  import { transcript as loadTranscript } from '$lib/api/client';
  import type { Transcript } from '$lib/api/types';
  import { FOLD_LINES, lineCount } from '$lib/format';
  import { href, link, runPath } from '$lib/router';
  import Problem from './Problem.svelte';

  let {
    id,
    session,
    load = loadTranscript,
  }: {
    id: string;
    session: string;
    load?: (id: string, session: string) => Promise<Transcript>;
  } = $props();

  let conversation = $derived(load(id, session));

  /** Whether message `index` is the first of its turn: the turn's anchor. */
  function opensTurn(t: Transcript, index: number): boolean {
    return index === 0 || t.messages[index - 1]?.turn !== t.messages[index]?.turn;
  }

  // A link to a turn (`#turn-4`) lands on it once the conversation is in.
  $effect(() => {
    void conversation.then(() => {
      const hash = window.location.hash;
      if (hash.startsWith('#turn-')) {
        requestAnimationFrame(() => document.getElementById(hash.slice(1))?.scrollIntoView?.());
      }
    });
  });
</script>

{#await conversation}
  <p class="muted" aria-busy="true">Loading.</p>
{:then t}
  <h1>Transcript of {t.session}</h1>
  <p>
    Run <a href={href(runPath(id))} use:link>{id}</a><br>
    Model {t.model}, stopped {t.stop}, {t.turns} turns, tokens in {t.prompt_tokens} out {t.output_tokens}
  </p>
  <h2>System prompt</h2>
  <pre>{t.system}</pre>
  {#each t.messages as message, index (index)}
    <h2 id={opensTurn(t, index) ? `turn-${message.turn}` : undefined}>
      {message.role} <span class="muted">turn {message.turn}</span>
    </h2>
    {#each message.parts as part, at (at)}
      {#if part.type === 'text'}
        <pre>{part.text}</pre>
      {:else if part.type === 'call'}
        <p>Call <code>{part.name}</code></p>
        <pre>{part.arguments}</pre>
      {:else if part.type === 'result'}
        {#if lineCount(part.content) > FOLD_LINES}
          <details>
            <summary>{part.error ? 'Result: error' : 'Result'}, {lineCount(part.content)} lines</summary>
            <pre>{part.content}</pre>
          </details>
        {:else}
          <p>{part.error ? 'Result: error' : 'Result'}</p>
          <pre>{part.content}</pre>
        {/if}
      {:else}
        <p class="muted">Provider content, not shown.</p>
      {/if}
    {/each}
  {/each}
{:catch error}
  <Problem {error} />
{/await}
