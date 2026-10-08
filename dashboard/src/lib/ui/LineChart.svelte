<script lang="ts">
  /** Several lines over days, 0 to 100% (#228), drawn inline. A missing
   * value (`null`) is a gap, not a zero. Each series has its colour in the
   * legend and a table of its numbers for screen readers. */
  let {
    series,
    labels,
    caption,
  }: {
    series: { key: string; values: (number | null)[] }[];
    labels: string[];
    caption: string;
  } = $props();

  const WIDTH = 300;
  const HEIGHT = 100;

  let step = $derived(labels.length > 1 ? WIDTH / (labels.length - 1) : 0);
  const y = (value: number): number => HEIGHT - (value / 100) * HEIGHT;

  /** The runs of values without a gap, as polyline points. */
  function segments(values: (number | null)[]): string[] {
    const runs: string[] = [];
    let current: string[] = [];
    values.forEach((value, i) => {
      if (value === null) {
        if (current.length > 0) runs.push(current.join(' '));
        current = [];
      } else {
        current.push(`${(i * step).toFixed(1)},${y(value).toFixed(1)}`);
      }
    });
    if (current.length > 0) runs.push(current.join(' '));
    return runs;
  }
</script>

<figure class="line-chart">
  <div class="plot">
    <span class="axis top">100%</span>
    <span class="axis middle">50%</span>
    <span class="axis bottom">0</span>
    <svg viewBox="-2 -2 {WIDTH + 4} {HEIGHT + 4}" preserveAspectRatio="none" aria-hidden="true" focusable="false">
      {#each [0, 50, 100] as level (level)}
        <line class="grid" x1="0" x2={WIDTH} y1={y(level)} y2={y(level)} vector-effect="non-scaling-stroke" />
      {/each}
      {#each series as one, index (one.key)}
        {#each segments(one.values) as points, part (part)}
          {#if points.includes(' ')}
            <polyline class="series s{index}" {points} fill="none" vector-effect="non-scaling-stroke" />
          {:else}
            {@const [cx, cy] = points.split(',')}
            <circle class="series-dot s{index}" {cx} {cy} r="1.6" />
          {/if}
        {/each}
      {/each}
    </svg>
  </div>
  <div class="dates muted"><span>{labels[0] ?? ''}</span><span>{labels.at(-1) ?? ''}</span></div>
  <ul class="legend">
    {#each series as one, index (one.key)}
      <li><span class="swatch s{index}" aria-hidden="true"></span><span class="mono">{one.key}</span></li>
    {/each}
  </ul>
  {#each series as one (one.key)}
    <table class="visually-hidden">
      <caption>{caption}: {one.key}</caption>
      <tbody>
        {#each one.values as value, i (i)}
          <tr><th scope="row">{labels[i] ?? ''}</th><td>{value === null ? 'nothing judged' : `${Math.round(value)}%`}</td></tr>
        {/each}
      </tbody>
    </table>
  {/each}
</figure>

<style>
  .line-chart { margin: 0; display: flex; flex-direction: column; gap: 4px; }
  .plot { position: relative; padding-left: 2.6rem; }
  svg { display: block; width: 100%; height: 140px; overflow: visible; }
  .axis { position: absolute; left: 0; font-size: 11px; color: var(--muted); font-variant-numeric: tabular-nums; }
  .axis.top { top: -6px; }
  .axis.middle { top: calc(50% - 7px); }
  .axis.bottom { bottom: -6px; }
  .grid { stroke: var(--line-soft); stroke-width: 1; }
  polyline.series { fill: none; stroke-width: 2; stroke-linejoin: round; stroke-linecap: round; }
  .dates { display: flex; justify-content: space-between; padding-left: 2.6rem; font-size: 12px; }
  .legend { list-style: none; margin: 0; padding: 0; display: flex; flex-wrap: wrap; gap: 4px var(--space-3); font-size: 13px; }
  .legend li { display: inline-flex; align-items: center; gap: 6px; }
  .swatch { width: 12px; height: 3px; border-radius: 2px; }
  .s0 { stroke: var(--accent); fill: var(--accent); background: var(--accent); }
  .s1 { stroke: var(--drop); fill: var(--drop); background: var(--drop); }
  .s2 { stroke: var(--ok); fill: var(--ok); background: var(--ok); }
  .s3 { stroke: var(--warn); fill: var(--warn); background: var(--warn); }
  .s4 { stroke: var(--refused); fill: var(--refused); background: var(--refused); }
  .s5 { stroke: var(--fail); fill: var(--fail); background: var(--fail); }
</style>
