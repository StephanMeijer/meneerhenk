<script lang="ts">
  import { health } from '$lib/api/client';
  import type { Health } from '$lib/api/types';
  import Loading from '$lib/ui/Loading.svelte';
  import Problem from '$lib/ui/Problem.svelte';
  import Status from '$lib/ui/Status.svelte';

  /** Loads the checks; replaced in the tests. */
  let { load = health }: { load?: () => Promise<Health> } = $props();

  let attempt = $state(0);
  let checks = $derived.by(() => {
    void attempt;
    return load();
  });
</script>

<div class="page-head">
  <h1>Health</h1>
  <p class="intro">
    From configuration and the database. <code>henk doctor --probe</code> also tries every model and
    MCP server.
  </p>
</div>
<section class="panel">
  {#await checks}
    <Loading />
  {:then result}
    <div class="scroll">
      <table>
        <thead><tr><th>Check</th><th>State</th><th>Detail</th></tr></thead>
        <tbody>
          {#each result.checks as check (check.name)}
            <tr>
              <td>{check.name}</td>
              <td><Status word={check.state} kind="state" /></td>
              <td class="text">{check.detail}</td>
            </tr>
          {/each}
        </tbody>
      </table>
    </div>
  {:catch error}
    <Problem {error} retry={() => (attempt += 1)} />
  {/await}
</section>
