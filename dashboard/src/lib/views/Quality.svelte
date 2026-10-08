<script lang="ts">
  import {
    draftCount as loadDraftCount,
    drafts as listDrafts,
    quality as loadQuality,
    qualityDaily as loadQualityDaily,
  } from '$lib/api/client';
  import type { DraftCount, DraftItem, Page, QualityRow, QualitySeries } from '$lib/api/types';
  import { serverNow } from '$lib/clock';
  import { PERIODS, about, count, draftWhat, periodSince, ratePercent, verdictText } from '$lib/format';
  import { href, link, navigate, runPath, withQuery } from '$lib/router';
  import Empty from '$lib/ui/Empty.svelte';
  import LineChart from '$lib/ui/LineChart.svelte';
  import Loading from '$lib/ui/Loading.svelte';
  import Pager from '$lib/ui/Pager.svelte';
  import Problem from '$lib/ui/Problem.svelte';
  import Segmented from '$lib/ui/Segmented.svelte';
  import Status from '$lib/ui/Status.svelte';
  import Time from '$lib/ui/Time.svelte';

  /** The loaders and `now` are replaced in the tests. */
  let {
    query,
    loadRates = loadQuality,
    loadDaily = loadQualityDaily,
    loadDrafts = listDrafts,
    loadCount = loadDraftCount,
    now = serverNow,
  }: {
    query: URLSearchParams;
    loadRates?: (query: string) => Promise<QualityRow[]>;
    loadDaily?: (query: string) => Promise<QualitySeries[]>;
    loadDrafts?: (query: string) => Promise<Page<DraftItem>>;
    loadCount?: (query: string) => Promise<DraftCount>;
    now?: () => Date;
  } = $props();

  const GROUPS: Record<string, string> = {
    model: 'model',
    lane: 'lane',
    repo: 'repository',
    target: 'pull request',
  };
  const PLURALS: Record<string, string> = {
    model: 'models',
    lane: 'lanes',
    repo: 'repositories',
    target: 'pull requests',
  };
  const VERDICTS = ['rejected', 'confirmed', 'same_as', 'unchecked', 'not_checked', 'cancelled', 'failed', 'waiting'];

  let group = $derived(GROUPS[query.get('group') ?? ''] === undefined ? 'model' : (query.get('group') as string));
  let period = $derived(PERIODS[query.get('period') ?? ''] === undefined ? '30d' : (query.get('period') as string));
  let verdict = $derived(query.get('verdict') ?? 'rejected');
  let since = $derived(periodSince(period, now()));
  let selected = $derived(
    query.get('model') ? { key: 'model', value: query.get('model') as string }
    : query.get('lane') ? { key: 'lane', value: query.get('lane') as string }
    : null,
  );

  /** The query of the page itself, without the list's cursor. */
  let base = $derived(
    Object.fromEntries([...query.entries()].filter(([key]) => key !== 'cursor')),
  );

  const encode = (fields: Record<string, string | null | undefined>): string =>
    new URLSearchParams(
      Object.entries(fields).filter(
        (entry): entry is [string, string] => entry[1] !== null && entry[1] !== undefined && entry[1] !== '',
      ),
    ).toString();

  let ratesQuery = $derived(encode({ group, since, repo: query.get('repo') }));
  let listFields = $derived({
    verdict: verdict === 'all' ? null : verdict,
    model: query.get('model'),
    lane: query.get('lane'),
    repo: query.get('repo'),
    since,
  });
  let rates = $derived(loadRates(ratesQuery));
  let daily = $derived(loadDaily(ratesQuery));
  let list = $derived(loadDrafts(encode({ ...listFields, cursor: query.get('cursor') })));
  let listed = $derived(loadCount(encode(listFields)));

  /** The page with `changes` to its query, and the list back at its start. */
  function at(changes: Record<string, string | null>): string {
    return withQuery('/quality', { ...base, ...changes, cursor: null });
  }

  function chooseRepo(event: SubmitEvent): void {
    event.preventDefault();
    const form = new FormData(event.currentTarget as HTMLFormElement);
    navigate(at({ repo: String(form.get('repo') ?? '') }));
  }

  /** A row's key links the list to it, for a model or lane. */
  function filterOf(row: QualityRow): Record<string, string | null> | null {
    if (group === 'model') return { model: row.key, lane: null };
    if (group === 'lane') return { lane: row.key, model: null };
    return null;
  }

  /** Every group together: the counts add up. */
  function allOf(rows: QualityRow[]): QualityRow {
    const sum = (pick: (row: QualityRow) => number): number => rows.reduce((total, row) => total + pick(row), 0);
    const judged = sum((r) => r.judged);
    const rejected = sum((r) => r.rejected);
    return {
      key: `All ${PLURALS[group] ?? group}`,
      repo: null,
      target: null,
      target_url: null,
      drafts: sum((r) => r.drafts),
      confirmed: sum((r) => r.confirmed),
      rejected,
      same_as: sum((r) => r.same_as),
      unchecked: sum((r) => r.unchecked),
      not_checked: sum((r) => r.not_checked),
      cancelled: sum((r) => r.cancelled),
      failed: sum((r) => r.failed),
      waiting: sum((r) => r.waiting),
      judged,
      rejection_rate: judged === 0 ? null : rejected / judged,
    };
  }

  /** Where the drafts went, in the order of the bar. */
  function flowOf(all: QualityRow): { tone: string; n: number; words: string }[] {
    const parts = [
      { tone: 'ok', n: all.confirmed, words: 'confirmed and posted' },
      { tone: 'fail', n: all.rejected, words: 'rejected by the check' },
      { tone: 'neutral', n: all.same_as, words: 'repeats, merged into another draft' },
      { tone: 'warn', n: all.unchecked + all.not_checked, words: 'unchecked, went out unverified' },
      { tone: 'pending', n: all.waiting, words: 'waiting for the check' },
      { tone: 'refused', n: all.cancelled + all.failed, words: 'not written: the review ended first or the write failed' },
    ];
    return parts.filter((part, index) => index < 4 || part.n > 0);
  }

  /** `Showing 3 of 72 rejected drafts by mistral-medium-3-5`. */
  function showing(shown: number, total: number): string {
    const which = verdict === 'all' ? '' : `${verdict.replaceAll('_', ' ')} `;
    const whose = selected === null ? '' : ` ${selected.key === 'model' ? 'by' : 'on'} ${selected.value}`;
    return `Showing ${count(shown)} of ${count(total)} ${which}drafts${whose}`;
  }
</script>

<div class="page-head">
  <h1>Review quality</h1>
  <p class="intro">
    What the fact-check made of the lanes' drafts. The rejection rate is the share it
    rejected of the drafts it judged: confirmed, rejected and repeats. Drafts not checked or
    still waiting are counted, but not in the rate. Days are UTC days.
  </p>
</div>

<div class="controls">
  <Segmented
    label="Group by"
    options={Object.entries(GROUPS).map(([value, label]) => ({ value, label }))}
    current={group}
    to={(value) => at({ group: value, model: null, lane: null })}
  />
  <Segmented
    label="Period"
    options={Object.entries(PERIODS).map(([value, { label }]) => ({ value, label }))}
    current={period}
    to={(value) => at({ period: value })}
  />
  <form class="filters" onsubmit={chooseRepo}>
    <label>repository <input name="repo" value={query.get('repo') ?? ''} placeholder="all repositories"></label>
    <button>Show</button>
  </form>
</div>

{#await rates}
  <Loading />
{:then rows}
  {@const all = allOf(rows)}
  <div class="pair">
    <section class="panel" aria-labelledby="flow-title">
      <div class="panel-head">
        <h2 id="flow-title">From draft to pull request</h2>
        <span class="note">{all.key}, {PERIODS[period]?.label}: {count(all.drafts)} drafts</span>
      </div>
      <div class="panel-body">
        {#if all.drafts === 0}
          <Empty why="No drafts in this period." />
        {:else}
          {@const flow = flowOf(all)}
          <svg class="flow-bar" viewBox="0 0 1000 14" preserveAspectRatio="none" aria-hidden="true" focusable="false">
            {#each flow as part, index (part.words)}
              {@const before = flow.slice(0, index).reduce((total, p) => total + p.n, 0)}
              <rect class="fill-{part.tone}" x={(before / all.drafts) * 1000} y="0" width={(part.n / all.drafts) * 1000} height="14" />
            {/each}
          </svg>
          <ul class="flow">
            {#each flow as part (part.words)}
              <li><span class="key fill-{part.tone}" aria-hidden="true"></span><strong>{count(part.n)}</strong> {part.words}</li>
            {/each}
          </ul>
        {/if}
      </div>
    </section>

    <section class="panel" aria-labelledby="trend-title">
      <div class="panel-head">
        <h2 id="trend-title">Rejection rate by {GROUPS[group]}</h2>
        <span class="note">Higher means more drafts the check threw out</span>
      </div>
      <div class="panel-body">
        {#await daily}
          <Loading lines={2} />
        {:then series}
          {#if series.length === 0 || (series[0]?.days.length ?? 0) === 0}
            <Empty why="Nothing judged in this period." />
          {:else}
            <LineChart
              caption="Rejection rate per day"
              labels={series[0]?.days.map((d) => d.day) ?? []}
              series={series.map((s) => ({
                key: s.key,
                values: s.days.map((d) => (d.rate === null ? null : d.rate * 100)),
              }))}
            />
          {/if}
        {:catch error}
          <Problem {error} />
        {/await}
      </div>
    </section>
  </div>

  <section class="panel" aria-labelledby="rates-title">
    <div class="panel-head">
      <h2 id="rates-title">Rates by {GROUPS[group]}</h2>
      <span class="note">Largest group first. Select one to narrow the drafts below.</span>
    </div>
    {#if rows.length === 0}
      <Empty why="No drafts in this period." />
    {:else}
      <div class="scroll">
        <table class="rates">
          <thead>
            <tr>
              <th>{GROUPS[group]}</th><th class="num">Drafts</th><th class="num">Confirmed</th><th class="num">Rejected</th>
              <th class="num">Repeats</th><th class="num">Unchecked</th><th class="num">Waiting</th><th>Rejection rate</th>
            </tr>
          </thead>
          <tbody>
            {#each [all, ...rows] as row, index (index === 0 ? '' : row.key)}
              {@const only = index === 0 ? null : filterOf(row)}
              <tr class:all={index === 0} class:chosen={selected !== null && only !== null && only[selected.key] === selected.value}>
                <td class:mono={index > 0}>
                  {#if index === 0}
                    {row.key}
                  {:else if only !== null}
                    <a href={href(at({ ...only, verdict: 'rejected' }))} use:link title="Show what it got rejected">{row.key}</a>
                  {:else if row.target_url}
                    <a href={row.target_url} rel="noreferrer">{row.key}</a>
                  {:else}
                    {row.key}
                  {/if}
                </td>
                <td class="num">{count(row.drafts)}</td>
                <td class="num">{count(row.confirmed)}</td>
                <td class="num">{count(row.rejected)}</td>
                <td class="num">{count(row.same_as)}</td>
                <td class="num">{count(row.unchecked + row.not_checked)}</td>
                <td class="num">{count(row.waiting)}</td>
                <td class="rate"><span class="rate-cell">
                  <meter min="0" max="1" low="0.25" high="0.5" optimum="0" value={row.rejection_rate ?? 0}
                    title="{row.rejected} of {row.judged} judged"></meter>
                  <strong>{ratePercent(row.rejection_rate)}</strong>
                  <span class="muted">of {count(row.judged)}</span>
                </span></td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    {/if}
  </section>
{:catch error}
  <Problem {error} />
{/await}

<section class="list">
  <div class="list-head">
    <h2>
      {#if verdict === 'all'}Every draft{:else}Drafts {verdict.replaceAll('_', ' ')}{/if}
      {#if query.get('model')}by <span class="mono">{query.get('model')}</span>{/if}
      {#if query.get('lane')}on <span class="mono">{query.get('lane')}</span>{/if}
    </h2>
    <p class="chips">
      {#if selected !== null}
        <a class="selection" href={href(at({ model: null, lane: null }))} use:link title="Show every {selected.key}">
          {selected.key}: <span class="mono">{selected.value}</span> <span aria-hidden="true">&times;</span>
        </a>
      {/if}
      {#each [...VERDICTS, 'all'] as option (option)}
        <a href={href(at({ verdict: option }))} use:link class:chosen={option === verdict}>{option.replaceAll('_', ' ')}</a>
      {/each}
    </p>
  </div>
  {#await list}
    <Loading />
  {:then page}
    {#if page.items.length === 0}
      <Empty why="No drafts match." />
    {:else}
      <ul class="cards drafts">
        {#each page.items as item (`${item.run_id}/${item.draft.id}`)}
          <li>
            <div class="meta">
              <strong class="mono">{item.draft.id}</strong>
              <span class="mono">{item.draft.model}</span>
              <span class="muted mono">{item.draft.lane}</span>
              <code>{item.draft.path}:{item.draft.line}</code>
              <Status word={item.draft.decision?.verdict ?? 'waiting'} vocabulary="verdict" kind="verdict" text={verdictText(item.draft)} />
              <a class="mono" href={href(runPath(item.run_id))} use:link>{item.run_id}</a>
              {#if item.target_url}
                <a href={item.target_url} rel="noreferrer">{about(item.repo, item.target)}</a>
              {:else}
                <span>{about(item.repo, item.target)}</span>
              {/if}
              <span class="muted"><Time iso={item.draft.at} at={now()} /></span>
            </div>
            <p class="text text-block">{draftWhat(item.draft)}</p>
            {#if item.draft.decision && item.draft.decision.reason !== ''}
              <p class="reason text"><span class="muted">{item.draft.decision.checker}:</span> {item.draft.decision.reason}</p>
            {/if}
          </li>
        {/each}
      </ul>
    {/if}
    <div class="list-foot">
      <span class="muted showing">
        {#await listed then matched}
          {showing(page.items.length, matched.count)}
        {/await}
      </span>
      <Pager path="/quality" {query} next={page.next} />
    </div>
  {:catch error}
    <Problem {error} />
  {/await}
</section>

<style>
  .controls { display: flex; flex-wrap: wrap; gap: var(--space-3) var(--space-6); align-items: end; }
  .pair { display: grid; grid-template-columns: repeat(auto-fit, minmax(22rem, 1fr)); gap: var(--space-4); }
  .flow-bar { display: block; width: 100%; height: 14px; border-radius: var(--radius); overflow: hidden; }
  .flow { list-style: none; margin: var(--space-3) 0 0; padding: 0; display: flex; flex-wrap: wrap; gap: 6px var(--space-4); font-size: 13.5px; }
  .flow li { display: inline-flex; align-items: center; gap: 6px; }
  .key { width: 10px; height: 10px; border-radius: 2px; display: inline-block; }
  .fill-ok { fill: var(--ok); background: var(--ok); }
  .fill-fail { fill: var(--fail); background: var(--fail); }
  .fill-neutral { fill: var(--neutral); background: var(--neutral); }
  .fill-warn { fill: var(--warn); background: var(--warn); }
  .fill-pending { fill: var(--line); background: var(--line); }
  .fill-refused { fill: var(--refused); background: var(--refused); }
  tr.all td { font-weight: 600; background: var(--sunk); }
  tr.chosen td { box-shadow: inset 3px 0 0 var(--accent); }
  tr.chosen td + td { box-shadow: none; }
  .list { display: flex; flex-direction: column; gap: var(--space-3); }
  .list-head { display: flex; flex-direction: column; gap: var(--space-2); }
  .selection { background: var(--accent-soft); border-color: var(--accent); }
  .list-foot { display: flex; flex-wrap: wrap; justify-content: space-between; gap: var(--space-2); align-items: center; }
</style>
