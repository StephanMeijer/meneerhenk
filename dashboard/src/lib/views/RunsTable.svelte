<script lang="ts">
  import type { RunSummary } from '$lib/api/types';
  import { now } from '$lib/clock';
  import { about, kindText, runDuration } from '$lib/format';
  import { href, link, runPath } from '$lib/router';
  import Commit from '$lib/ui/Commit.svelte';
  import Empty from '$lib/ui/Empty.svelte';
  import Status from '$lib/ui/Status.svelte';
  import Time from '$lib/ui/Time.svelte';

  let { runs, why = '' }: { runs: RunSummary[]; why?: string } = $props();
</script>

{#if runs.length === 0}
  <Empty {why} />
{:else}
  <div class="scroll">
    <table class="runs">
      <thead>
        <tr>
          <th>Status</th><th>Run</th><th>Kind</th><th>About</th><th>Commit</th><th>Trigger</th><th>Started</th>
          <th class="num">Duration</th>
        </tr>
      </thead>
      <tbody>
        {#each runs as run (run.id)}
          <tr>
            <td><Status word={run.status} /></td>
            <td><a class="mono" href={href(runPath(run.id))} use:link>{run.id}</a></td>
            <td class="nowrap">{kindText(run.kind)}</td>
            <td>
              {#if run.repo === ''}
                <span class="muted">no pull request</span>
              {:else if run.target_url}
                <a href={run.target_url} rel="noreferrer">{about(run.repo, run.target)}</a>
              {:else}
                {about(run.repo, run.target)}
              {/if}
            </td>
            <td>{#if run.commit}<Commit sha={run.commit} />{/if}</td>
            <td>{run.trigger} {#if run.requester}<span class="who">{run.requester}</span>{/if}</td>
            <td class="nowrap"><Time iso={run.started_at} /></td>
            <td class="num">{runDuration(run, $now)}</td>
          </tr>
        {/each}
      </tbody>
    </table>
  </div>
{/if}
