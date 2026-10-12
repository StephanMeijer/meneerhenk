<script lang="ts">
  import { sessionStats } from '$lib/api/client';
  import type { SessionReasons, SessionRow, SessionStats } from '$lib/api/types';
  import { serverNow } from '$lib/clock';
  import { PERIODS, SESSION_REASONS, count, periodSince, ratePercent } from '$lib/format';
  import { isHistory, roleLabel, roleRank } from '$lib/roles';
  import { withQuery } from '$lib/router';
  import Empty from '$lib/ui/Empty.svelte';
  import Loading from '$lib/ui/Loading.svelte';
  import Problem from '$lib/ui/Problem.svelte';
  import Segmented from '$lib/ui/Segmented.svelte';

  /** The sessions compared over a period (#229), grouped by role (#326):
   * how often each finished, timed out or did not, and why. `load` and `now` are replaced in the
   * tests. */
  let {
    query,
    load = sessionStats,
    now = serverNow,
  }: {
    query: URLSearchParams;
    load?: (query: string) => Promise<SessionStats>;
    now?: () => Date;
  } = $props();

  const REASONS = Object.keys(SESSION_REASONS) as (keyof SessionReasons)[];

  let period = $derived(PERIODS[query.get('period') ?? ''] === undefined ? '30d' : (query.get('period') as string));
  let stats = $derived.by(() => {
    const since = periodSince(period, now());
    return load(new URLSearchParams({ since: since ?? '1970-01-01T00:00:00Z' }).toString());
  });

  /** The reasons that occurred, most first: `rate limit 3`. */
  const why = (reasons: SessionReasons): string[] =>
    REASONS.filter((reason) => reasons[reason] > 0)
      .sort((a, b) => reasons[b] - reasons[a])
      .map((reason) => `${SESSION_REASONS[reason]} ${count(reasons[reason])}`);

  const share = (row: SessionRow): number | null => (row.ran === 0 ? null : row.did_not_finish / row.ran);

  /** The rows by role, in the order roles come; each group keeps the order
   * the API gave its rows. */
  function byRole(rows: SessionRow[]): { role: string; rows: SessionRow[] }[] {
    const roles = [...new Set(rows.map((row) => row.role))].sort((a, b) => roleRank(a) - roleRank(b));
    return roles.map((role) => ({ role, rows: rows.filter((row) => row.role === role) }));
  }
</script>

<div class="page-head">
  <h1>Sessions</h1>
  <p class="intro">
    How each review session ended, by role: the loop's reviewer and executor, and the review lanes and
    fact-checks of reviews from before the loop. Cancelled and superseded reviews are left out: their sessions were
    stopped for them, by a person or for a newer commit.
  </p>
</div>

<div class="controls">
  <Segmented
    label="Period"
    options={Object.entries(PERIODS).map(([value, { label }]) => ({ value, label }))}
    current={period}
    to={(value) => withQuery('/sessions', { period: value })}
  />
</div>

<section class="panel">
  {#await stats}
    <Loading />
  {:then result}
    {#if result.sessions.length === 0}
      <Empty why="No review ended in this period." />
    {:else}
      <div class="scroll">
        <table class="compare">
          <thead>
            <tr>
              <th>Session</th><th>Models</th><th class="num">Ran</th><th class="num">Finished</th>
              <th class="num">Timed out</th><th>Did not finish</th><th>Why</th>
            </tr>
          </thead>
          {#each byRole(result.sessions) as group (group.role)}
            <tbody>
              <tr class="group">
                <th colspan="7" scope="rowgroup">
                  {roleLabel(group.role)}{#if isHistory(group.role)}<span class="muted">&nbsp;· before the loop</span>{/if}
                </th>
              </tr>
              {#each group.rows as row (row.name)}
                <tr>
                  <td class="mono">{row.name}</td>
                  <td class="mono models">{row.models.join(', ')}</td>
                  <td class="num">{count(row.ran)}</td>
                  <td class="num">{count(row.finished)}</td>
                  <td class="num">{count(row.timed_out)}</td>
                  <td class="rate"><span class="rate-cell">
                    <meter min="0" max="1" low="0.1" high="0.25" optimum="0" value={share(row) ?? 0}
                      title="{row.did_not_finish} of {row.ran}"></meter>
                    <strong>{ratePercent(share(row))}</strong>
                    <span class="muted">{count(row.did_not_finish)}</span>
                  </span></td>
                  <td class="why">
                    {#if why(row.reasons).length === 0}
                      <span class="muted">-</span>
                    {:else}
                      {why(row.reasons).join(', ')}
                    {/if}
                  </td>
                </tr>
              {/each}
            </tbody>
          {/each}
        </table>
      </div>
      <div class="panel-foot">
        <span>{count(result.reviews.length)} {result.reviews.length === 1 ? 'review' : 'reviews'}{#if result.reviews.length >= 1000}, the newest 1000{/if}.</span>
      </div>
    {/if}
  {:catch error}
    <Problem {error} />
  {/await}
</section>

<style>
  .controls { display: flex; flex-wrap: wrap; gap: var(--space-3); }
  .models { max-width: 16rem; white-space: normal; overflow-wrap: anywhere; }
  .why { min-width: 12rem; }
  .group th { text-align: left; font-weight: 600; background: var(--sunk); }
</style>
