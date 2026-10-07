<script lang="ts">
  import type { ListenerOutcome } from '$lib/api/types';
  import { href, link, runPath } from '$lib/router';
  import Status from '$lib/ui/Status.svelte';

  let { outcomes }: { outcomes: ListenerOutcome[] } = $props();
</script>

<ul class="outcomes">
  {#each outcomes as outcome, index (index)}
    <li>
      <span class="mono listener">{outcome.listener}:</span>
      <Status word={outcome.outcome} kind="listener-outcome" />
      {#if outcome.run_id}
        <a class="mono" href={href(runPath(outcome.run_id))} use:link>{outcome.run_id}</a>
      {/if}
      {#if outcome.detail}<span class="muted detail">{outcome.detail}</span>{/if}
    </li>
  {/each}
</ul>

<style>
  .outcomes { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 6px; }
  li { display: flex; flex-wrap: wrap; align-items: center; gap: 2px var(--space-2); }
  .listener { font-weight: 600; }
  .detail { flex-basis: 100%; font-size: 13px; }
</style>
