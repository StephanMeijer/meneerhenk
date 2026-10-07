<script lang="ts">
  import { transcript as loadTranscript } from '$lib/api/client';
  import type { Transcript } from '$lib/api/types';
  import { FOLD_LINES, count, lineCount } from '$lib/format';
  import { href, link, runPath } from '$lib/router';
  import Loading from '$lib/ui/Loading.svelte';
  import Problem from '$lib/ui/Problem.svelte';

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
  <Loading />
{:then t}
  <div class="page-head">
    <span class="crumbs">
      <a href={href('/')} use:link>Runs</a> / <a class="mono" href={href(runPath(id))} use:link>{id}</a> / <code>{t.session}</code>
    </span>
    <h1>Transcript of <span class="mono">{t.session}</span></h1>
    <p class="intro">
      Model <span class="mono">{t.model}</span>, stopped {t.stop}, {count(t.turns)} turns, tokens in
      {count(t.prompt_tokens)} out {count(t.output_tokens)}. What the model saw and said, shown as data.
    </p>
  </div>
  <section class="panel message">
    <div class="panel-head"><h2>System prompt</h2></div>
    <div class="panel-body"><pre>{t.system}</pre></div>
  </section>
  {#each t.messages as message, index (index)}
    <section class="panel message {message.role}">
      <div class="panel-head">
        <h2 id={opensTurn(t, index) ? `turn-${message.turn}` : undefined}>
          {message.role} <span class="muted">turn {message.turn}</span>
        </h2>
      </div>
      <div class="panel-body parts">
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
              <p class:fail={part.error}>{part.error ? 'Result: error' : 'Result'}</p>
              <pre>{part.content}</pre>
            {/if}
          {:else}
            <p class="muted">Provider content, not shown.</p>
          {/if}
        {/each}
      </div>
    </section>
  {/each}
{:catch error}
  <Problem {error} />
{/await}

<style>
  .parts { display: flex; flex-direction: column; gap: var(--space-2); }
  .parts p { margin: 0; }
  .message.assistant { border-left: 3px solid var(--accent); }
  .fail { color: var(--fail); }
  h2[id] { scroll-margin-top: var(--space-4); }
</style>
