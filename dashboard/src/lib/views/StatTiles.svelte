<script lang="ts">
  import { overviewStats } from '$lib/api/client';
  import type { DayStats, OverviewStats, Slots } from '$lib/api/types';
  import { count, duration } from '$lib/format';
  import Problem from '$lib/ui/Problem.svelte';
  import Spark from '$lib/ui/Spark.svelte';

  /** Four tiles (#225): runs per day, the share that did not complete,
   * findings posted, and the review slots. Days are UTC days; today is
   * still counting. */
  let {
    slots,
    now = () => new Date(),
    load = overviewStats,
  }: {
    slots: Slots | null;
    now?: () => Date;
    load?: () => Promise<OverviewStats>;
  } = $props();

  let stats = $derived(load());

  const sum = (days: DayStats[], pick: (d: DayStats) => number): number =>
    days.reduce((total, day) => total + pick(day), 0);
  const labels = (days: DayStats[]): string[] => days.map((d) => d.day);
  const shares = (days: DayStats[]): number[] =>
    days.map((d) => (d.finished + d.failed === 0 ? 0 : (d.failed / (d.finished + d.failed)) * 100));
  const percent = (part: number, whole: number): string =>
    whole === 0 ? '-' : `${(Math.round((part / whole) * 1000) / 10).toString()}%`;
  const waited = (since: string): string => duration(now().getTime() - Date.parse(since));
</script>

{#await stats then result}
  {@const days = result.days}
  {@const failed = sum(days, (d) => d.failed)}
  {@const ended = sum(days, (d) => d.finished + d.failed)}
  <ul class="stats">
    <li class="tile">
      <h3 class="label">Runs, last {days.length} days</h3>
      <p class="figure"><strong>{count(sum(days, (d) => d.runs))}</strong>
        <span class="muted">{count(days.at(-2)?.runs ?? 0)} yesterday</span></p>
      <Spark values={days.map((d) => d.runs)} labels={labels(days)} caption="Runs per day" partialLast />
    </li>
    <li class="tile">
      <h3 class="label">Did not complete, {days.length} days</h3>
      <p class="figure"><strong>{percent(failed, ended)}</strong>
        <span class="muted">{count(failed)} of {count(ended)} runs</span></p>
      <Spark
        values={shares(days)}
        labels={labels(days)}
        kind="line"
        tone="fail"
        caption="Share of ended runs that failed, per day"
        format={(n) => `${Math.round(n)}%`}
      />
    </li>
    <li class="tile">
      <h3 class="label">Findings posted, {days.length} days</h3>
      <p class="figure"><strong>{count(sum(days, (d) => d.findings_posted))}</strong>
        <span class="muted">from {count(sum(days, (d) => d.drafts))} drafts</span></p>
      <Spark
        values={days.map((d) => d.findings_posted)}
        labels={labels(days)}
        tone="ok"
        caption="Findings posted per day"
        partialLast
      />
    </li>
    <li class="tile slots">
      <h3 class="label">Review slots</h3>
      {#if slots === null}
        <p class="muted">Waiting for the stream.</p>
      {:else}
        <p class="figure"><strong>{slots.in_use} of {slots.limit}</strong>
          <span class="muted">in use{#if slots.waiting.length > 0}, {slots.waiting.length} waiting{/if}</span></p>
        <svg class="slot-bars" viewBox="0 0 {slots.limit * 10} 6" preserveAspectRatio="none" aria-hidden="true" focusable="false">
          {#each Array.from({ length: slots.limit }, (_, i) => i) as i (i)}
            <rect class:used={i < slots.in_use} x={i * 10 + 0.5} y="0" width="9" height="6" rx="1.5" />
          {/each}
        </svg>
        {#if slots.waiting.length > 0}
          <ul class="waiting">
            {#each slots.waiting as review (`${review.repo}#${review.target}`)}
              <li><span class="mono">{review.repo} #{review.target}</span> <span class="muted">{waited(review.since)}, no free slot</span></li>
            {/each}
          </ul>
        {/if}
      {/if}
    </li>
  </ul>
{:catch error}
  <Problem {error} />
{/await}

<style>
  .stats {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(15rem, 1fr));
    gap: var(--space-3);
  }
  .tile {
    background: var(--surface);
    border: 1px solid var(--line-soft);
    border-radius: var(--radius-lg);
    padding: var(--space-3) var(--space-4);
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    min-width: 0;
  }
  .label { font-size: 11.5px; letter-spacing: 0.06em; text-transform: uppercase; color: var(--muted); font-weight: 600; }
  .figure { margin: 0; display: flex; align-items: baseline; gap: var(--space-2); flex-wrap: wrap; }
  .figure strong { font-size: 26px; font-weight: 600; font-variant-numeric: tabular-nums; }
  .slot-bars { width: 100%; height: 8px; }
  .slot-bars rect { fill: var(--sunk); }
  .slot-bars rect.used { fill: var(--accent); }
  .waiting { list-style: none; margin: 0; padding: 0; font-size: 13px; display: flex; flex-direction: column; gap: 2px; }
</style>
