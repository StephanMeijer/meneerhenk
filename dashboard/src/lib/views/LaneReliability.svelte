<script lang="ts">
  import { laneStats } from '$lib/api/client';
  import type { LaneOutcome, LaneRow, LaneStats } from '$lib/api/types';
  import { LANE_REASONS, ratePercent } from '$lib/format';
  import { href, link, runPath } from '$lib/router';
  import { lookOf } from '$lib/status';
  import Empty from '$lib/ui/Empty.svelte';
  import Loading from '$lib/ui/Loading.svelte';
  import Problem from '$lib/ui/Problem.svelte';

  /** How each lane and fact-check session ended over the last reviews
   * (#229): a square per review, oldest left. A review stands on the
   * lanes that finished (§3.2), so a row with many gaps in green is a
   * lane that makes reviews thinner. */
  let { load = laneStats }: { load?: (query: string) => Promise<LaneStats> } = $props();

  const LAST = 30;
  /** How many of the newest outcomes the text equivalent reads out. */
  const SAID = 5;

  let stats = $derived(load(`last=${LAST}`));

  const day = (iso: string): string => iso.slice(0, 10);

  function says(outcome: LaneOutcome, at: string): string {
    const why = outcome.reason === null ? '' : `, ${LANE_REASONS[outcome.reason] ?? outcome.reason}`;
    return `${day(at)} ${outcome.model}: ${lookOf(outcome.status).label}${why}`;
  }

  /** The row as a sentence, for screen readers. */
  function sentence(row: LaneRow): string {
    const last = row.outcomes
      .filter((o): o is LaneOutcome => o !== null)
      .slice(-SAID)
      .map((o) => lookOf(o.status).label);
    const timed = row.timed_out > 0 ? `, ${row.timed_out} timed out` : '';
    return `${row.name}: ${row.did_not_finish} of ${row.ran} did not finish${timed}; last ${last.length}, newest last: ${last.join(', ')}.`;
  }

  /** The newest model, and how many others the lane ran. */
  const modelsText = (models: string[]): string =>
    models.length > 1 ? `${models[0] ?? ''} and ${models.length - 1} more` : (models[0] ?? '');

  const share = (row: LaneRow): number | null => (row.ran === 0 ? null : row.did_not_finish / row.ran);
</script>

<section class="panel" aria-labelledby="lanes-title">
  <div class="panel-head">
    <h2 id="lanes-title">Lane reliability</h2>
    <span class="note">Last {LAST} reviews, oldest left · <a href={href('/lanes')} use:link>Compare lanes</a></span>
  </div>
  {#await stats}
    <Loading />
  {:then result}
    {#if result.lanes.length === 0}
      <Empty why="No review has ended yet." />
    {:else}
      <ul class="lanes">
        {#each result.lanes as row (row.name)}
          <li>
            <span class="name">
              <span class="mono">{row.name}</span>
              <span class="muted model">{modelsText(row.models)}</span>
            </span>
            <span class="squares" aria-hidden="true">
              {#each row.outcomes as outcome, i (result.reviews[i]?.run_id ?? i)}
                {#if outcome === null}
                  <span class="square gap" title="{day(result.reviews[i]?.started_at ?? '')}: did not run"></span>
                {:else}
                  <a
                    class="square sq-{lookOf(outcome.status).tone}"
                    href={href(runPath(outcome.run_id))}
                    use:link
                    tabindex="-1"
                    title={says(outcome, result.reviews[i]?.started_at ?? '')}
                  ></a>
                {/if}
              {/each}
            </span>
            <span class="share">
              <strong class:bad={row.did_not_finish > 0}>{ratePercent(share(row))}</strong>
              <span class="muted">{row.did_not_finish} of {row.ran} reviews</span>
            </span>
            <span class="visually-hidden">{sentence(row)}</span>
          </li>
        {/each}
      </ul>
      <div class="panel-foot legend" aria-hidden="true">
        <span><span class="square sq-ok"></span>finished</span>
        <span><span class="square sq-warn"></span>timed out</span>
        <span><span class="square sq-drop"></span>did not finish</span>
        <span><span class="square gap"></span>did not run</span>
      </div>
    {/if}
  {:catch error}
    <Problem {error} />
  {/await}
</section>

<style>
  .lanes { list-style: none; margin: 0; padding: 0; border-top: 1px solid var(--line-soft); }
  .lanes > li {
    display: grid;
    grid-template-columns: minmax(9rem, 14rem) 1fr 9rem;
    gap: var(--space-2) var(--space-3);
    align-items: center;
    padding: 10px var(--space-4);
    border-bottom: 1px solid var(--line-soft);
  }
  .name { display: flex; flex-direction: column; min-width: 0; }
  .model { font-size: 0.8125rem; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .squares { display: flex; flex-wrap: wrap; gap: 3px; }
  .square {
    display: inline-block;
    width: 12px;
    height: 12px;
    border-radius: var(--radius-sm);
    background: var(--neutral);
  }
  a.square:hover { outline: 2px solid var(--ink); outline-offset: 1px; }
  .gap { background: transparent; border: 1px dashed var(--line); }
  .sq-ok { background: var(--ok); }
  .sq-warn { background: var(--warn); }
  .sq-drop { background: var(--drop); }
  .sq-fail { background: var(--fail); }
  .sq-accent { background: var(--accent); }
  .share { display: flex; flex-direction: column; text-align: right; font-variant-numeric: tabular-nums; }
  .share .bad { color: var(--drop); }
  .legend { display: flex; flex-wrap: wrap; justify-content: flex-start; gap: var(--space-2) var(--space-4); }
  .legend > span { display: inline-flex; align-items: center; gap: 6px; }
  @media (max-width: 720px) {
    .lanes > li { grid-template-columns: 1fr auto; }
    .squares { grid-column: 1 / -1; grid-row: 2; }
  }
</style>
