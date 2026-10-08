<script lang="ts">
  import type { Part, ToolCall } from '$lib/api/types';
  import { FOLD_LINES, count, lineCount } from '$lib/format';
  import Status from './Status.svelte';

  /** A message's parts, shown as text, never as markup (§8.3): what a
   * model or a tool said is data. `calls`, when given, are the recorded
   * calls of the message's turn in the order they ran; each call part
   * then shows how its call went, or that it still runs (#238). */
  let { parts, calls = null }: { parts: Part[]; calls?: ToolCall[] | null } = $props();

  /** The record of the `index`th call part. */
  function callAt(index: number): ToolCall | undefined {
    const which = parts.slice(0, index).filter((p) => p.type === 'call').length;
    return calls?.[which];
  }
</script>

<div class="parts">
  {#each parts as part, at (at)}
    {#if part.type === 'text'}
      <pre>{part.text}</pre>
    {:else if part.type === 'call'}
      {@const record = callAt(at)}
      <p class="call">
        Call <code>{part.name}</code>
        {#if calls !== null}
          {#if record}
            <Status word={record.outcome} vocabulary="outcome" kind="outcome" />
            <span class="muted">{count(record.elapsed_ms)} ms, {count(record.result_chars)} chars</span>
          {:else}
            <Status word="running" />
          {/if}
        {/if}
      </p>
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

<style>
  .parts { display: flex; flex-direction: column; gap: var(--space-2); }
  .parts p { margin: 0; }
  .call { display: flex; flex-wrap: wrap; align-items: center; gap: var(--space-2); }
  .fail { color: var(--fail); }
</style>
