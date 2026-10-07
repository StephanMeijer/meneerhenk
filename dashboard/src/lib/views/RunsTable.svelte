<script lang="ts">
  import type { RunSummary } from '$lib/api/types';
  import { about, kindText } from '$lib/format';
  import { href, link, runPath } from '$lib/router';

  let { runs }: { runs: RunSummary[] } = $props();
</script>

{#if runs.length === 0}
  <p class="muted">None.</p>
{:else}
  <table>
    <thead>
      <tr><th>Run</th><th>Kind</th><th>About</th><th>Status</th><th>Started</th><th>Trigger</th></tr>
    </thead>
    <tbody>
      {#each runs as run (run.id)}
        <tr>
          <td><a href={href(runPath(run.id))} use:link>{run.id}</a></td>
          <td>{kindText(run.kind)}</td>
          <td>
            {#if run.target_url}
              <a href={run.target_url} rel="noreferrer">{about(run.repo, run.target)}</a>
            {:else}
              {about(run.repo, run.target)}
            {/if}
          </td>
          <td><span class="status {run.status}">{run.status}</span></td>
          <td>{run.started_at}</td>
          <td>{run.trigger}</td>
        </tr>
      {/each}
    </tbody>
  </table>
{/if}
