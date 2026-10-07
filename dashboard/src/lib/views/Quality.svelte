<script lang="ts">
  import { drafts as listDrafts, quality as loadQuality } from '$lib/api/client';
  import type { DraftItem, Page, QualityRow } from '$lib/api/types';
  import { PERIODS, about, draftWhat, periodSince, ratePercent, verdictText } from '$lib/format';
  import { href, link, navigate, runPath, withQuery } from '$lib/router';
  import Pager from './Pager.svelte';
  import Problem from './Problem.svelte';

  /** `loadRates`, `loadDrafts` and `now` are replaced in the tests. */
  let {
    query,
    loadRates = loadQuality,
    loadDrafts = listDrafts,
    now = () => new Date(),
  }: {
    query: URLSearchParams;
    loadRates?: (query: string) => Promise<QualityRow[]>;
    loadDrafts?: (query: string) => Promise<Page<DraftItem>>;
    now?: () => Date;
  } = $props();

  const GROUPS: Record<string, string> = {
    model: 'model',
    lane: 'lane',
    repo: 'repository',
    target: 'pull request',
  };
  const VERDICTS = ['rejected', 'confirmed', 'same_as', 'unchecked', 'not_checked', 'cancelled', 'failed', 'waiting'];

  let group = $derived(GROUPS[query.get('group') ?? ''] === undefined ? 'model' : (query.get('group') as string));
  let period = $derived(PERIODS[query.get('period') ?? ''] === undefined ? '30d' : (query.get('period') as string));
  let verdict = $derived(query.get('verdict') ?? 'rejected');
  let since = $derived(periodSince(period, now()));

  /** The query of the page itself, without the list's cursor. */
  let base = $derived(
    Object.fromEntries([...query.entries()].filter(([key]) => key !== 'cursor')),
  );

  let ratesQuery = $derived(
    new URLSearchParams(
      Object.entries({ group, since, repo: query.get('repo') }).filter(
        (entry): entry is [string, string] => entry[1] !== null && entry[1] !== '',
      ),
    ).toString(),
  );
  let draftsQuery = $derived(
    new URLSearchParams(
      Object.entries({
        verdict: verdict === 'all' ? null : verdict,
        model: query.get('model'),
        lane: query.get('lane'),
        repo: query.get('repo'),
        since,
        cursor: query.get('cursor'),
      }).filter((entry): entry is [string, string] => entry[1] !== null && entry[1] !== ''),
    ).toString(),
  );
  let rates = $derived(loadRates(ratesQuery));
  let list = $derived(loadDrafts(draftsQuery));

  /** The page with `changes` to its query, and the list back at its start. */
  function at(changes: Record<string, string | null>): string {
    const next: Record<string, string | null> = { ...base, ...changes };
    return withQuery('/quality', next);
  }

  function choose(event: SubmitEvent): void {
    event.preventDefault();
    const form = new FormData(event.currentTarget as HTMLFormElement);
    const value = (key: string): string => String(form.get(key) ?? '');
    navigate(
      withQuery('/quality', {
        group: value('group'),
        period: value('period'),
        repo: value('repo'),
        verdict: value('verdict'),
        model: query.get('model'),
        lane: query.get('lane'),
      }),
    );
  }

  /** A row's key links the list to it, for a model or lane. */
  function filterOf(row: QualityRow): Record<string, string | null> | null {
    if (group === 'model') return { model: row.key, lane: null };
    if (group === 'lane') return { lane: row.key, model: null };
    return null;
  }
</script>

<h1>Review quality</h1>
<p class="muted">
  What the fact-check made of the lanes' drafts. The rejection rate is the share it
  rejected of the drafts it judged: confirmed, rejected and repeats. Drafts not checked or
  still waiting are counted, but not in the rate.
</p>

<form class="filters" onsubmit={choose}>
  <label>
    group by
    <select name="group" value={group}>
      {#each Object.entries(GROUPS) as [value, label] (value)}
        <option {value}>{label}</option>
      {/each}
    </select>
  </label>
  <label>
    period
    <select name="period" value={period}>
      {#each Object.entries(PERIODS) as [value, { label }] (value)}
        <option {value}>{label}</option>
      {/each}
    </select>
  </label>
  <label>repo <input name="repo" value={query.get('repo') ?? ''} placeholder="owner/name"></label>
  <input type="hidden" name="verdict" value={verdict}>
  <button>Show</button>
</form>

{#await rates}
  <p class="muted" aria-busy="true">Loading.</p>
{:then rows}
  {#if rows.length === 0}
    <p class="muted">No drafts in this period.</p>
  {:else}
    <table class="rates">
      <thead>
        <tr>
          <th>{GROUPS[group]}</th><th>Drafts</th><th>Confirmed</th><th>Rejected</th><th>Repeats</th>
          <th>Unchecked</th><th>Waiting</th><th>Rejection rate</th>
        </tr>
      </thead>
      <tbody>
        {#each rows as row (row.key)}
          {@const only = filterOf(row)}
          <tr>
            <td>
              {#if only !== null}
                <a href={href(at({ ...only, verdict: 'rejected' }))} use:link title="Show what it got rejected">{row.key}</a>
              {:else if row.target_url}
                <a href={row.target_url} rel="noreferrer">{row.key}</a>
              {:else}
                {row.key}
              {/if}
            </td>
            <td>{row.drafts}</td>
            <td>{row.confirmed}</td>
            <td>{row.rejected}</td>
            <td>{row.same_as}</td>
            <td>{row.unchecked + row.not_checked}</td>
            <td>{row.waiting}</td>
            <td class="rate"><span class="rate-cell">
              <meter min="0" max="1" low="0.25" high="0.5" optimum="0" value={row.rejection_rate ?? 0}
                title="{row.rejected} of {row.judged} judged"></meter>
              <span>{ratePercent(row.rejection_rate)}</span>
              <span class="muted">of {row.judged}</span>
            </span></td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
{:catch error}
  <Problem {error} />
{/await}

<h2>
  {#if verdict === 'all'}Every draft{:else}Drafts {verdict.replaceAll('_', ' ')}{/if}
  {#if query.get('model')}by {query.get('model')}{/if}
  {#if query.get('lane')}on {query.get('lane')}{/if}
</h2>
<p class="filters chips">
  {#each [...VERDICTS, 'all'] as option (option)}
    <a href={href(at({ verdict: option }))} use:link class:chosen={option === verdict}>{option.replaceAll('_', ' ')}</a>
  {/each}
  {#if query.get('model') || query.get('lane')}
    <a href={href(at({ model: null, lane: null }))} use:link>every model and lane</a>
  {/if}
</p>
{#await list}
  <p class="muted" aria-busy="true">Loading.</p>
{:then page}
  {#if page.items.length === 0}
    <p class="muted">None.</p>
  {:else}
    <ul class="drafts">
      {#each page.items as item (`${item.run_id}/${item.draft.id}`)}
        <li>
          <div class="meta">
            <strong>{item.draft.model}</strong>
            <span class="muted">{item.draft.lane}</span>
            <code>{item.draft.path}:{item.draft.line}</code>
            <span class="verdict {item.draft.decision?.verdict ?? 'waiting'}">{verdictText(item.draft)}</span>
            <a href={href(runPath(item.run_id))} use:link>{item.run_id}</a>
            {#if item.target_url}
              <a href={item.target_url} rel="noreferrer">{about(item.repo, item.target)}</a>
            {:else}
              <span>{about(item.repo, item.target)}</span>
            {/if}
            <span class="muted">{item.draft.at}</span>
          </div>
          <p class="text">{draftWhat(item.draft)}</p>
          {#if item.draft.decision && item.draft.decision.reason !== ''}
            <p class="reason text"><span class="muted">{item.draft.decision.checker}:</span> {item.draft.decision.reason}</p>
          {/if}
        </li>
      {/each}
    </ul>
  {/if}
  <Pager path="/quality" {query} next={page.next} />
{:catch error}
  <Problem {error} />
{/await}
