<script lang="ts">
  import { onMount } from 'svelte';
  import { runs as listRuns } from '$lib/api/client';
  import { connectEventSource, follow, type Connect } from '$lib/api/stream';
  import type { Me, Page, RunningMessage, RunSummary } from '$lib/api/types';
  import { applyRunning, type Running } from '$lib/live';
  import { navigate, route, withQuery } from '$lib/router';
  import Loading from '$lib/ui/Loading.svelte';
  import Pager from '$lib/ui/Pager.svelte';
  import Problem from '$lib/ui/Problem.svelte';
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
  // "Start a run" in the header leads here (#230 makes it a dialog).
  $effect(() => {
    void $route;
    if (window.location.hash !== '#start') return;
    requestAnimationFrame(() => {
      document.getElementById('start')?.scrollIntoView?.();
      document.querySelector<HTMLInputElement>('#start input[name=url]')?.focus();
    });
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

<div class="page-head">
  <div class="row">
    <h1>Overview</h1>
  </div>
  <p class="intro">
    Signed in as {me.login} <span class="who">github:{me.github_id}</span>. Times in UTC, hover for the exact time.
  </p>
</div>

<section class="panel" id="start" aria-labelledby="start-title">
  <div class="panel-head"><h2 id="start-title">Start a run</h2></div>
  <div class="panel-body"><StartForm startable={me.startable} /></div>
</section>

<section class="panel" aria-labelledby="running-title">
  <div class="panel-head">
    <h2 id="running-title">Running now <span class="count-badge">({running.count})</span></h2>
    <span class="note">Updates as runs start and end</span>
  </div>
  <RunsTable runs={running.runs} why="Nothing runs right now." />
  {#if running.count > running.runs.length}
    <div class="panel-foot">And {running.count - running.runs.length} more not shown.</div>
  {/if}
</section>

<section class="panel" aria-labelledby="runs-title">
  <div class="panel-head">
    <h2 id="runs-title">Recent runs</h2>
    <form class="filters" onsubmit={filter}>
      {#each [['kind', KINDS], ['status', STATUSES], ['platform', PLATFORMS]] as const as [name, options] (name)}
        <label>
          {name}
          <select {name} value={query.get(name) ?? ''}>
            <option value="">any</option>
            {#each options as option (option)}
              <option value={option}>{option.replaceAll('_', ' ')}</option>
            {/each}
          </select>
        </label>
      {/each}
      <label>repository <input name="repo" value={query.get('repo') ?? ''} placeholder="owner/name"></label>
      <button>Show</button>
    </form>
  </div>
  {#await page}
    <Loading />
  {:then result}
    <RunsTable runs={result.items} why="No runs match these filters." />
    <div class="panel-foot">
      <span>Newest first.</span>
      <Pager path="/" {query} next={result.next} />
    </div>
  {:catch error}
    <Problem {error} />
  {/await}
</section>
