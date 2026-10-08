<script lang="ts">
  import { onMount } from 'svelte';
  import { health as loadHealth, laneStats, overviewStats, runCount, runs as listRuns } from '$lib/api/client';
  import { connectEventSource, follow, type Connect } from '$lib/api/stream';
  import { serverNow } from '$lib/clock';
  import type { Health, LaneStats, Me, OverviewStats, Page, RunCount, RunningMessage, RunSummary } from '$lib/api/types';
  import { PERIODS, about, count, kindText, periodSince, runDuration } from '$lib/format';
  import { applyRunning, connectionAfter, type Connection, type Running } from '$lib/live';
  import { href, link, navigate, runPath, withQuery } from '$lib/router';
  import { lookOf } from '$lib/status';
  import Commit from '$lib/ui/Commit.svelte';
  import Elapsed from '$lib/ui/Elapsed.svelte';
  import Empty from '$lib/ui/Empty.svelte';
  import Live from '$lib/ui/Live.svelte';
  import Loading from '$lib/ui/Loading.svelte';
  import Pager from '$lib/ui/Pager.svelte';
  import Problem from '$lib/ui/Problem.svelte';
  import Status from '$lib/ui/Status.svelte';
  import HealthTiles from './HealthTiles.svelte';
  import LaneReliability from './LaneReliability.svelte';
  import Queued from './Queued.svelte';
  import RunsTable from './RunsTable.svelte';
  import StageStepper from './StageStepper.svelte';
  import StatTiles from './StatTiles.svelte';

  /** The loaders, the stream and the clock are replaced in the tests. The
   * clock is the server's, like the durations' (#252), so the queued waits
   * agree with them. */
  let {
    me,
    query,
    loadRuns = listRuns,
    loadCount = runCount,
    loadHealth: health = loadHealth,
    loadStats = overviewStats,
    loadLanes = laneStats,
    connect = connectEventSource,
    now = serverNow,
  }: {
    me: Me;
    query: URLSearchParams;
    loadRuns?: (query: string) => Promise<Page<RunSummary>>;
    loadCount?: (query: string) => Promise<RunCount>;
    loadHealth?: () => Promise<Health>;
    loadStats?: () => Promise<OverviewStats>;
    loadLanes?: (query: string) => Promise<LaneStats>;
    connect?: Connect;
    now?: () => Date;
  } = $props();

  const KINDS = ['review', 'plan', 'address', 'discord_turn', 'mail_reply'];
  const STATUSES = ['running', 'finished', 'failed', 'cancelled', 'superseded'];
  const PLATFORMS = ['github', 'gitlab'];
  const FILTERS = ['kind', 'status', 'platform', 'repo', 'cursor'];

  let period = $derived(PERIODS[query.get('period') ?? ''] === undefined ? 'all' : (query.get('period') as string));

  /** The `GET /runs` query this page shows: only the keys it knows, and
   * the period as `since`. */
  let listQuery = $derived.by(() => {
    const params = new URLSearchParams(
      [...query.entries()].filter(([key, value]) => FILTERS.includes(key) && value !== ''),
    );
    const since = periodSince(period, now());
    if (since !== null) params.set('since', since);
    return params.toString();
  });
  /** The same without the cursor: what all pages together match. */
  let countQuery = $derived.by(() => {
    const params = new URLSearchParams(listQuery);
    params.delete('cursor');
    return params.toString();
  });
  let page = $derived(loadRuns(listQuery));
  let matching = $derived(loadCount(countQuery));

  // What runs now, as /runs/stream says it: no polling (#202).
  let running: Running = $state({ runs: [], count: 0, slots: null });
  let connection: Connection = $state('connecting');

  onMount(() => {
    const following = follow<RunningMessage>(
      '/runs/stream',
      ['snapshot', 'run', 'progress', 'slots'],
      (message) => (running = applyRunning(running, message)),
      (open) => (connection = connectionAfter(connection, open)),
      connect,
    );
    return () => following.close();
  });

  /** The runs "Running now" lists, so a review that just took its slot is
   * not also shown as queued. */
  let shownIds = $derived(running.runs.map((run) => run.id));
  let queuedCount = $derived(
    (running.slots?.waiting ?? []).filter((entry) => !shownIds.includes(entry.run_id)).length,
  );

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
        period: value('period') === 'all' ? '' : value('period'),
      }),
    );
  }

  /** Where a running run's lanes are, in a line: `lanes: 1 running, 1 did
   * not finish`, or `planner: running`. */
  function lanesLine(run: RunSummary): string {
    const sessions = run.lanes.filter((lane) => !lane.name.startsWith('check-'));
    if (sessions.length === 0) return '';
    if (run.kind !== 'review') {
      return sessions.map((lane) => `${lane.name}: ${lookOf(lane.status).label}`).join(', ');
    }
    const counts: Record<string, number> = {};
    for (const lane of sessions) counts[lane.status] = (counts[lane.status] ?? 0) + 1;
    return `lanes: ${Object.entries(counts).map(([status, n]) => `${n} ${lookOf(status).label}`).join(', ')}`;
  }
</script>

<div class="page-head">
  <div class="row">
    <h1>Overview</h1>
    <Live {connection} />
  </div>
  <p class="intro">
    Signed in as {me.login} <span class="who">github:{me.github_id}</span>. Times and days in UTC, hover
    for the exact time.
  </p>
</div>

<HealthTiles load={health} />

<StatTiles slots={running.slots} load={loadStats} />

<section class="panel" aria-labelledby="running-title">
  <div class="panel-head">
    <h2 id="running-title">Running now <span class="count-badge">({running.count})</span></h2>
    <span class="note">{running.count} running, {queuedCount} queued · Updates as runs start and end</span>
  </div>
  {#if running.runs.length === 0}
    <Empty why="Nothing runs right now." />
  {:else}
    <ul class="running">
      {#each running.runs as run (run.id)}
        <li>
          <Status word={run.status} />
          <span class="who-what">
            <a class="mono" href={href(runPath(run.id))} use:link>{run.id}</a>
            <span class="muted">{kindText(run.kind)}</span>
          </span>
          <span class="about">
            {#if run.target_url}
              <a href={run.target_url} rel="noreferrer">{about(run.repo, run.target)}</a>
            {:else}
              {about(run.repo, run.target)}
            {/if}
            {#if run.commit}<Commit sha={run.commit} />{/if}
          </span>
          <span class="trigger">{run.trigger}{#if run.requester}<span class="sub who">{run.requester}</span>{/if}</span>
          <span class="progress">
            <StageStepper stages={run.stages} lanes={run.lanes} />
            <span class="muted">{lanesLine(run)}</span>
          </span>
          <span class="num duration">{#if run.finished_at === null}<Elapsed since={run.started_at} />{:else}{runDuration(run)}{/if}</span>
        </li>
      {/each}
    </ul>
  {/if}
  {#if running.count > running.runs.length}
    <div class="panel-foot">And {running.count - running.runs.length} more not shown.</div>
  {/if}
</section>

<Queued slots={running.slots} shown={shownIds} {now} />

<LaneReliability load={loadLanes} />

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
      <label>
        period
        <select name="period" value={period}>
          {#each Object.entries(PERIODS) as [value, { label }] (value)}
            <option {value}>{label}</option>
          {/each}
        </select>
      </label>
      <button>Show</button>
    </form>
  </div>
  {#await page}
    <Loading />
  {:then result}
    <RunsTable runs={result.items} why="No runs match these filters." lanes />
    <div class="panel-foot">
      <span>
        Newest first.
        {#await matching then matched}{count(matched.count)} {matched.count === 1 ? 'run matches' : 'runs match'}.{/await}
      </span>
      <Pager path="/" {query} next={result.next} />
    </div>
  {:catch error}
    <Problem {error} />
  {/await}
</section>

<style>
  .running { list-style: none; margin: 0; padding: 0; border-top: 1px solid var(--line-soft); }
  .running > li {
    display: grid;
    grid-template-columns: 7rem minmax(10rem, 1.2fr) minmax(10rem, 1.2fr) minmax(8rem, 1fr) minmax(12rem, 1.6fr) 5rem;
    gap: var(--space-2) var(--space-3);
    align-items: center;
    padding: 10px var(--space-4);
    border-bottom: 1px solid var(--line-soft);
  }
  .running > li:last-child { border-bottom: 0; }
  .running > li > :global(.pill) { justify-self: start; }
  .duration { justify-self: end; }
  .who-what, .about, .trigger { display: flex; flex-direction: column; min-width: 0; overflow-wrap: anywhere; }
  .progress { display: flex; flex-wrap: wrap; align-items: center; gap: 4px var(--space-2); font-size: 13px; }
  .duration { font-weight: 600; }

  @media (max-width: 960px) {
    .running > li { grid-template-columns: 1fr 1fr; }
    .progress { grid-column: 1 / -1; }
  }
</style>
