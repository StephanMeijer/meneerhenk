<script lang="ts">
  import { onMount } from 'svelte';
  import { runs as listRuns } from '$lib/api/client';
  import { connectEventSource, follow, type Connect } from '$lib/api/stream';
  import type { Me, Page, RunningMessage, RunSummary } from '$lib/api/types';
  import { applyRunning, type Running } from '$lib/live';
  import { navigate, withQuery } from '$lib/router';
  import Pager from './Pager.svelte';
  import Problem from './Problem.svelte';
  import RunsTable from './RunsTable.svelte';
  import StartForm from './StartForm.svelte';

  let {
    me,
    query,
    loadRuns = listRuns,
    connect = connectEventSource,
  }: {
    me: Me;
    query: URLSearchParams;
    loadRuns?: (query: string) => Promise<Page<RunSummary>>;
    connect?: Connect;
  } = $props();

  const KINDS = ['review', 'plan', 'address', 'discord_turn', 'mail_reply'];
  const STATUSES = ['running', 'finished', 'failed', 'cancelled', 'superseded'];
  const PLATFORMS = ['github', 'gitlab'];
  const FILTERS = ['kind', 'status', 'platform', 'repo', 'cursor'];

  /** The `GET /runs` query this page shows: only the keys it knows. */
  let listQuery = $derived(
    new URLSearchParams([...query.entries()].filter(([key, value]) => FILTERS.includes(key) && value !== '')).toString(),
  );
  let page = $derived(loadRuns(listQuery));

  // What runs now, as /runs/stream says it: no polling (#202).
  let running: Running = $state({ runs: [], count: 0 });

  onMount(() => {
    const following = follow<RunningMessage>(
      '/runs/stream',
      ['snapshot', 'run'],
      (message) => (running = applyRunning(running, message)),
      () => {},
      connect,
    );
    return () => following.close();
  });
  function filter(event: SubmitEvent): void {
    event.preventDefault();
    const form = new FormData(event.currentTarget as HTMLFormElement);
    const value = (key: string): string => String(form.get(key) ?? '');
    navigate(
      withQuery('/', {
        kind: value('kind'),
        status: value('status'),
        platform: value('platform'),
        repo: value('repo'),
      }),
    );
  }
</script>

<h1>Meneer Henk</h1>
<p class="muted">Signed in as {me.login}.</p>

<h2>Start</h2>
<StartForm startable={me.startable} />

<h2>Running now ({running.count})</h2>
<RunsTable runs={running.runs} />
{#if running.count > running.runs.length}
  <p class="muted">And {running.count - running.runs.length} more not shown.</p>
{/if}

<h2>Runs</h2>
<form class="filters" onsubmit={filter}>
  {#each [['kind', KINDS], ['status', STATUSES], ['platform', PLATFORMS]] as const as [name, options] (name)}
    <label>
      {name}
      <select {name} value={query.get(name) ?? ''}>
        <option value="">any</option>
        {#each options as option (option)}
          <option value={option}>{option}</option>
        {/each}
      </select>
    </label>
  {/each}
  <label>repo <input name="repo" value={query.get('repo') ?? ''} placeholder="owner/name"></label>
  <button>Show</button>
</form>
{#await page}
  <p class="muted" aria-busy="true">Loading.</p>
{:then result}
  <RunsTable runs={result.items} />
  <Pager path="/" {query} next={result.next} />
{:catch error}
  <Problem {error} />
{/await}
