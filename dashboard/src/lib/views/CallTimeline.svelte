<script lang="ts">
  import type { ToolCall } from '$lib/api/types';
  import { count, outcomeClass } from '$lib/format';
  import { href, link, transcriptPath } from '$lib/router';
  import Status from '$lib/ui/Status.svelte';

  /** Every call of one run, in order; `kept` names the sessions whose
   * conversation the run kept, which a call can link to. */
  let { runId, calls, kept }: { runId: string; calls: ToolCall[]; kept: string[] } = $props();

  let only = $state('all');
  let shown = $derived(
    calls.filter(
      (call) =>
        only === 'all' ||
        (only === 'problems' ? call.outcome !== 'ok' : outcomeClass(call.outcome) === only),
    ),
  );
  const FILTERS = ['all', 'problems', 'error', 'refused', 'other'];
</script>

<p class="chips">
  {#each FILTERS as option (option)}
    <button type="button" class="chip" class:chosen={option === only} onclick={() => (only = option)}>{option}</button>
  {/each}
  <span class="muted">{shown.length} of {calls.length}</span>
</p>
<div class="scroll-x">
<table class="timeline">
  <thead>
    <tr><th>Session</th><th class="num">Turn</th><th>Tool</th><th>Outcome</th><th>Arguments</th><th class="num">Back</th><th class="num">Time</th></tr>
  </thead>
  <tbody>
    {#each shown as call, index (index)}
      <tr>
        <td class="mono">{call.session}</td>
        <td class="num">
          {#if kept.includes(call.session)}
            <a href={href(transcriptPath(runId, call.session, call.turn))} use:link title="That turn of the conversation">{call.turn}</a>
          {:else}
            {call.turn}
          {/if}
        </td>
        <td><code>{call.tool}</code></td>
        <td><Status word={call.outcome} vocabulary="outcome" kind="outcome" /></td>
        <td class="args"><code>{call.arguments}</code></td>
        <td class="num">{count(call.result_chars)}</td>
        <td class="num">{call.elapsed_ms} ms</td>
      </tr>
    {/each}
  </tbody>
</table>
</div>

<style>
  .chips { margin-bottom: var(--space-3); }
  .scroll-x { overflow-x: auto; }
  .args code { white-space: pre-wrap; overflow-wrap: anywhere; display: block; max-width: 36rem; max-height: 6rem; overflow: auto; }
  td:nth-child(3), td:nth-child(4) { white-space: nowrap; }
</style>
