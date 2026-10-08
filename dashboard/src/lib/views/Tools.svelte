<script lang="ts">
  import { toolCalls as listCalls, toolSummary as loadSummary } from '$lib/api/client';
  import type { Page, ToolCallItem, ToolSummaryRow } from '$lib/api/types';
  import { PERIODS, about, average, count, outcomeClass, periodSince, ratePercent } from '$lib/format';
  import { href, link, navigate, runPath, transcriptPath, withQuery } from '$lib/router';
  import Empty from '$lib/ui/Empty.svelte';
  import Loading from '$lib/ui/Loading.svelte';
  import Pager from '$lib/ui/Pager.svelte';
  import Problem from '$lib/ui/Problem.svelte';
  import Status from '$lib/ui/Status.svelte';
  import Time from '$lib/ui/Time.svelte';

  /** `loadSummary`, `loadCalls` and `now` are replaced in the tests. */
  let {
    query,
    loadSummary: summary = loadSummary,
    loadCalls = listCalls,
    now = () => new Date(),
  }: {
    query: URLSearchParams;
    loadSummary?: (query: string) => Promise<ToolSummaryRow[]>;
    loadCalls?: (query: string) => Promise<Page<ToolCallItem>>;
    now?: () => Date;
  } = $props();

  const KINDS: Record<string, string> = {
    all: 'every kind',
    lane: 'review lanes',
    check: 'fact-checks',
    planner: 'planner',
    address: 'address runs',
  };
  const OUTCOMES = [
    'problems',
    'error',
    'refused_scope',
    'refused_repeat',
    'malformed_arguments',
    'unknown_tool',
    'not_run',
    'cancelled',
    'ok',
    'all',
  ];

  let period = $derived(PERIODS[query.get('period') ?? ''] === undefined ? '30d' : (query.get('period') as string));
  let kind = $derived(
    KINDS[query.get('session_kind') ?? ''] === undefined || query.get('session_kind') === 'all'
      ? 'all'
      : (query.get('session_kind') as string),
  );
  let outcome = $derived(query.get('outcome') ?? 'problems');
  let since = $derived(periodSince(period, now()));
  let base = $derived(Object.fromEntries([...query.entries()].filter(([key]) => key !== 'cursor')));

  const encode = (fields: Record<string, string | null | undefined>): string =>
    new URLSearchParams(
      Object.entries(fields).filter(
        (entry): entry is [string, string] => entry[1] !== null && entry[1] !== undefined && entry[1] !== '',
      ),
    ).toString();

  let kindParam = $derived(kind === 'all' ? null : kind);
  let summaryQuery = $derived(encode({ model: query.get('model'), session_kind: kindParam, since }));
  let callsQuery = $derived(
    encode({
      outcome: outcome === 'all' ? null : outcome,
      tool: query.get('tool'),
      model: query.get('model'),
      session_kind: kindParam,
      since,
      cursor: query.get('cursor'),
    }),
  );
  let rows = $derived(summary(summaryQuery));
  let calls = $derived(loadCalls(callsQuery));

  function at(changes: Record<string, string | null>): string {
    return withQuery('/tools', { ...base, ...changes });
  }

  function choose(event: SubmitEvent): void {
    event.preventDefault();
    const form = new FormData(event.currentTarget as HTMLFormElement);
    const value = (key: string): string => String(form.get(key) ?? '');
    navigate(
      withQuery('/tools', {
        period: value('period'),
        session_kind: value('session_kind'),
        model: value('model'),
        outcome,
        tool: query.get('tool'),
      }),
    );
  }
</script>

<div class="page-head">
  <h1>Tool calls</h1>
  <p class="intro">
    How each tool fared, per model and kind of session. An error is a call the tool itself failed;
    a refusal is one the scope guard or the repeat guard stopped; "not run" covers unknown tools,
    malformed arguments and calls cut off by a cancel.
  </p>
</div>

<section class="panel">
  <div class="panel-head">
    <h2>Per tool, model and session kind</h2>
    <form class="filters" onsubmit={choose}>
      <label>
        period
        <select name="period" value={period}>
          {#each Object.entries(PERIODS) as [value, { label }] (value)}
            <option {value}>{label}</option>
          {/each}
        </select>
      </label>
      <label>
        sessions
        <select name="session_kind" value={kind}>
          {#each Object.entries(KINDS) as [value, label] (value)}
            <option {value}>{label}</option>
          {/each}
        </select>
      </label>
      <label>model <input name="model" value={query.get('model') ?? ''} placeholder="any model"></label>
      <button>Show</button>
    </form>
  </div>
  {#await rows}
    <Loading />
  {:then summaryRows}
    {#if summaryRows.length === 0}
      <Empty why="No tool calls in this period." />
    {:else}
      <div class="scroll">
        <table class="rates">
          <thead>
            <tr>
              <th>Tool</th><th>Model</th><th>Sessions</th><th class="num">Calls</th><th class="num">Errors</th>
              <th class="num">Refused</th><th class="num">Not run</th><th class="num">Average</th><th>Error rate</th>
              <th>Refusal rate</th>
            </tr>
          </thead>
          <tbody>
            {#each summaryRows as row (`${row.tool}/${row.model}/${row.session_kind}`)}
              <tr>
                <td>
                  <a href={href(at({ tool: row.tool, model: row.model, session_kind: row.session_kind, outcome: 'all' }))} use:link title="Show these calls">
                    <code>{row.tool}</code>
                  </a>
                </td>
                <td class="mono">{row.model}</td>
                <td>{row.session_kind}</td>
                <td class="num">{count(row.calls)}</td>
                <td class="num">{count(row.errors)}</td>
                <td class="num">{count(row.refusals)}</td>
                <td class="num">{count(row.other)}</td>
                <td class="num">{average(row.total_ms, row.calls)}</td>
                <td class="rate"><span class="rate-cell">
                  <meter min="0" max="1" low="0.1" high="0.25" optimum="0" value={row.error_rate}
                    title="{row.errors} of {row.calls} calls"></meter>
                  <span>{ratePercent(row.error_rate)}</span>
                </span></td>
                <td class="rate"><span class="rate-cell">
                  <meter min="0" max="1" low="0.1" high="0.25" optimum="0" value={row.refusal_rate}
                    title="{row.refusals} of {row.calls} calls"></meter>
                  <span>{ratePercent(row.refusal_rate)}</span>
                </span></td>
              </tr>
            {/each}
          </tbody>
        </table>
      </div>
    {/if}
  {:catch error}
    <Problem {error} />
  {/await}
</section>

<section class="list">
  <div class="list-head">
    <h2>
      {#if outcome === 'problems'}What went wrong{:else if outcome === 'all'}Every call{:else}Calls that ended {outcome.replaceAll('_', ' ')}{/if}
      {#if query.get('tool')}with <code>{query.get('tool')}</code>{/if}
      {#if query.get('model')}by <span class="mono">{query.get('model')}</span>{/if}
    </h2>
    <p class="chips">
      {#each OUTCOMES as option (option)}
        <a href={href(at({ outcome: option }))} use:link class:chosen={option === outcome}>{option.replaceAll('_', ' ')}</a>
      {/each}
      {#if query.get('tool') || query.get('model')}
        <a href={href(at({ tool: null, model: null }))} use:link>every tool and model</a>
      {/if}
    </p>
  </div>
  {#await calls}
    <Loading />
  {:then page}
    {#if page.items.length === 0}
      <Empty why="No calls match." />
    {:else}
      <ul class="cards drafts calls">
        {#each page.items as item, index (index)}
          <li>
            <div class="meta">
              <code><strong>{item.call.tool}</strong></code>
              <Status word={item.call.outcome} vocabulary="outcome" kind="outcome {outcomeClass(item.call.outcome)}" />
              <span class="mono">{item.call.model}</span>
              <span class="muted">{item.call.session}, turn {item.call.turn}</span>
              <a class="mono" href={href(runPath(item.run_id))} use:link>{item.run_id}</a>
              {#if item.target_url}
                <a href={item.target_url} rel="noreferrer">{about(item.repo, item.target)}</a>
              {/if}
            </div>
            <pre>{item.call.arguments}</pre>
            <div class="foot">
              <span class="muted">{item.call.elapsed_ms} ms, {count(item.call.result_chars)} chars back, <Time iso={item.call.at} at={now()} /></span>
              {#if item.transcript_kept}
                <a href={href(transcriptPath(item.run_id, item.call.session, item.call.turn))} use:link title="That turn of the conversation">Turn {item.call.turn} in the conversation</a>
              {/if}
            </div>
          </li>
        {/each}
      </ul>
    {/if}
    <Pager path="/tools" {query} next={page.next} />
  {:catch error}
    <Problem {error} />
  {/await}
</section>

<style>
  .list { display: flex; flex-direction: column; gap: var(--space-3); }
  .list-head { display: flex; flex-direction: column; gap: var(--space-2); }
  .foot { display: flex; flex-wrap: wrap; justify-content: space-between; gap: var(--space-2); font-size: 13px; }
</style>
