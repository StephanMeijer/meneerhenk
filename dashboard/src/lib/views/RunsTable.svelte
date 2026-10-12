<script lang="ts">
  import type { RunSummary } from '$lib/api/types';
  import { about, kindText, runDuration } from '$lib/format';
  import { shownInRounds } from '$lib/roles';
  import { href, link, runPath } from '$lib/router';
  import { lookOf } from '$lib/status';
  import Commit from '$lib/ui/Commit.svelte';
  import Empty from '$lib/ui/Empty.svelte';
  import Elapsed from '$lib/ui/Elapsed.svelte';
  import Icon from '$lib/ui/Icon.svelte';
  import Status from '$lib/ui/Status.svelte';
  import Time from '$lib/ui/Time.svelte';

  /** `sessions` adds a column with one status icon per session (#225). */
  let { runs, why = '', sessions = false }: { runs: RunSummary[]; why?: string; sessions?: boolean } = $props();
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
          {#if sessions}<th>Sessions</th>{/if}
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
            <td class="num">{#if run.finished_at === null}<Elapsed since={run.started_at} />{:else}{runDuration(run)}{/if}</td>
            {#if sessions}
              <td class="session-icons">
                {#each run.sessions.filter(shownInRounds) as session (session.name)}
                  {@const look = lookOf(session.status)}
                  <span class="tone-{look.tone} session-icon" title="{session.name}: {look.label}">
                    <Icon name={look.icon} size={13} /><span class="visually-hidden">{session.name}: {look.label}</span>
                  </span>
                {/each}
              </td>
            {/if}
          </tr>
        {/each}
      </tbody>
    </table>
  </div>
{/if}

<style>
  .session-icons { white-space: nowrap; }
  .session-icon { display: inline-grid; place-items: center; width: 20px; height: 20px; border-radius: 50%; margin-right: 2px; }
</style>
