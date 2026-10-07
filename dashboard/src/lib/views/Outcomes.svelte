<script lang="ts">
  import type { ListenerOutcome } from '$lib/api/types';
  import { href, link, runPath } from '$lib/router';

  let { outcomes }: { outcomes: ListenerOutcome[] } = $props();
</script>

{#each outcomes as outcome, index (index)}
  <div>
    <strong>{outcome.listener}</strong>: {outcome.outcome}
    <span class="muted">{outcome.detail}</span>
    {#if outcome.run_id}
      <a href={href(runPath(outcome.run_id))} use:link>{outcome.run_id}</a>
    {/if}
  </div>
{/each}
