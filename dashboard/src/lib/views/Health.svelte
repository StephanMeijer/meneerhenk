<script lang="ts">
  import { health } from '$lib/api/client';
  import type { Health } from '$lib/api/types';
  import Problem from './Problem.svelte';

  /** Loads the checks; replaced in the tests. */
  let { load = health }: { load?: () => Promise<Health> } = $props();

  let checks = $derived(load());
</script>

<h1>Health</h1>
<p class="muted">
  From configuration and the database. <code>henk doctor --probe</code> also
  tries every model and MCP server.
</p>
{#await checks}
  <p class="muted" aria-busy="true">Loading.</p>
{:then result}
  <table>
    <thead><tr><th>Check</th><th>State</th><th>Detail</th></tr></thead>
    <tbody>
      {#each result.checks as check (check.name)}
        <tr>
          <td>{check.name}</td>
          <td><span class="state {check.state}">{check.state}</span></td>
          <td>{check.detail}</td>
        </tr>
      {/each}
    </tbody>
  </table>
{:catch error}
  <Problem {error} />
{/await}
