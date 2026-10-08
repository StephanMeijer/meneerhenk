<script lang="ts">
  import { laneStats } from '$lib/api/client';
  import type { LaneReasons, LaneRow, LaneStats } from '$lib/api/types';
  import { LANE_REASONS, PERIODS, count, periodSince, ratePercent } from '$lib/format';
  import { withQuery } from '$lib/router';
  import Empty from '$lib/ui/Empty.svelte';
  import Loading from '$lib/ui/Loading.svelte';
  import Problem from '$lib/ui/Problem.svelte';
  import Segmented from '$lib/ui/Segmented.svelte';

  /** The lanes compared over a period (#229): how often each finished,
   * timed out or did not, and why. `load` and `now` are replaced in the
   * tests. */
  let {
    query,
    load = laneStats,
    now = () => new Date(),
  }: {
    query: URLSearchParams;
    load?: (query: string) => Promise<LaneStats>;
    now?: () => Date;
  } = $props();

  const REASONS = Object.keys(LANE_REASONS) as (keyof LaneReasons)[];

  let period = $derived(PERIODS[query.get('period') ?? ''] === undefined ? '30d' : (query.get('period') as string));
  let stats = $derived.by(() => {
    const since = periodSince(period, now());
    return load(new URLSearchParams({ since: since ?? '1970-01-01T00:00:00Z' }).toString());
  });

  /** The reasons that occurred, most first: `rate limit 3`. */
  const why = (reasons: LaneReasons): string[] =>
    REASONS.filter((reason) => reasons[reason] > 0)
      .sort((a, b) => reasons[b] - reasons[a])
      .map((reason) => `${LANE_REASONS[reason]} ${count(reasons[reason])}`);

  const share = (row: LaneRow): number | null => (row.ran === 0 ? null : row.did_not_finish / row.ran);
</script>

<div class="page-head">
  <h1>Lanes</h1>
  <p class="intro">
    How each review lane and fact-check session ended. A review stands on the lanes that finished;
    a lane that times out keeps what it drafted until then. Superseded reviews are left out: their
    lanes were cancelled for a newer commit.
  </p>
</div>

<div class="controls">
  <Segmented
    label="Period"
    options={Object.entries(PERIODS).map(([value, { label }]) => ({ value, label }))}
    current={period}
    to={(value) => withQuery('/lanes', { period: value })}
  />
</div>

<section class="panel">
  {#await stats}
    <Loading />
  {:then result}
    {#if result.lanes.length === 0}
      <Empty why="No review ended in this period." />
    {:else}
      <div class="scroll">
        <table class="compare">
          <thead>
            <tr>
              <th>Lane</th><th>Models</th><th class="num">Ran</th><th class="num">Finished</th>
              <th class="num">Timed out</th><th>Did not finish</th><th>Why</th>
            </tr>
          </thead>
          <tbody>
            {#each result.lanes as row (row.name)}
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
</style>
