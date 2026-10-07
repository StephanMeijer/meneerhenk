<script lang="ts">
  import type { ToolCall } from '$lib/api/types';
  import { outcomeClass } from '$lib/format';
  import { href, link, transcriptPath } from '$lib/router';

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

<p class="filters chips">
  {#each FILTERS as option (option)}
    <button type="button" class="chip" class:chosen={option === only} onclick={() => (only = option)}>{option}</button>
  {/each}
  <span class="muted">{shown.length} of {calls.length}</span>
</p>
<table class="timeline">
  <thead>
    <tr><th>Session</th><th>Turn</th><th>Tool</th><th>Outcome</th><th>Arguments</th><th>Back</th><th>Time</th></tr>
  </thead>
  <tbody>
    {#each shown as call, index (index)}
      <tr>
        <td>{call.session}</td>
        <td>
          {#if kept.includes(call.session)}
            <a href={href(transcriptPath(runId, call.session, call.turn))} use:link title="That turn of the conversation">{call.turn}</a>
          {:else}
            {call.turn}
          {/if}
        </td>
        <td><code>{call.tool}</code></td>
        <td><span class="outcome {outcomeClass(call.outcome)}">{call.outcome.replaceAll('_', ' ')}</span></td>
        <td class="args"><code>{call.arguments}</code></td>
        <td>{call.result_chars}</td>
        <td>{call.elapsed_ms} ms</td>
      </tr>
    {/each}
  </tbody>
</table>
