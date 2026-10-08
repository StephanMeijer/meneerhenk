<script lang="ts">
  import { health as loadHealth } from '$lib/api/client';
  import type { Health } from '$lib/api/types';
  import { href, link } from '$lib/router';
  import Problem from '$lib/ui/Problem.svelte';
  import Status from '$lib/ui/Status.svelte';

  /** The health checks at a glance (#225); the health page has the rest. */
  let { load = loadHealth }: { load?: () => Promise<Health> } = $props();
  let checks = $derived(load());
</script>

<section class="tiles-section" aria-labelledby="health-title">
  <div class="list-head">
    <h2 id="health-title">Health</h2>
    <a href={href('/health')} use:link>All checks</a>
  </div>
  {#await checks then result}
    <ul class="tiles">
      {#each result.checks as check (check.name)}
        <li class="tile">
          <div class="tile-head"><strong>{check.name}</strong><Status word={check.state} kind="state" /></div>
          <p class="muted">{check.detail}</p>
        </li>
      {/each}
    </ul>
  {:catch error}
    <Problem {error} />
  {/await}
</section>

<style>
  .tiles-section { display: flex; flex-direction: column; gap: var(--space-2); }
  .list-head { display: flex; justify-content: space-between; align-items: baseline; }
  .tiles {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(10.5rem, 1fr));
    gap: var(--space-2);
  }
  .tile {
    background: var(--surface);
    border: 1px solid var(--line-soft);
    border-radius: var(--radius-lg);
    padding: var(--space-2) var(--space-3);
    min-width: 0;
  }
  .tile-head { display: flex; justify-content: space-between; align-items: center; gap: var(--space-2); }
  .tile p { margin: 4px 0 0; font-size: 13px; overflow-wrap: anywhere; }
</style>
