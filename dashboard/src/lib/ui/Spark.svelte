<script lang="ts">
  /** A small chart of numbers, drawn inline (the CSP allows no image from
   * elsewhere and no inline style: sizes are SVG attributes). `bars` or a
   * `line`; the last value can be marked as still counting. A table of the
   * same numbers is there for screen readers. */
  let {
    values,
    labels,
    kind = 'bars',
    tone = 'accent',
    caption,
    format = (n: number) => String(n),
    partialLast = false,
  }: {
    values: number[];
    labels: string[];
    kind?: 'bars' | 'line';
    tone?: 'accent' | 'ok' | 'fail';
    caption: string;
    format?: (n: number) => string;
    partialLast?: boolean;
  } = $props();

  const HEIGHT = 40;
  const STEP = 10;
  let top = $derived(Math.max(1, ...values));
  let width = $derived(Math.max(1, values.length) * STEP);
  let points = $derived(
    values
      .map((v, i) => `${i * STEP + STEP / 2},${(HEIGHT - 2 - (v / top) * (HEIGHT - 4)).toFixed(1)}`)
      .join(' '),
  );
</script>

<figure class="spark ink-{tone}">
  <svg viewBox="0 0 {width} {HEIGHT}" preserveAspectRatio="none" aria-hidden="true" focusable="false">
    {#if kind === 'bars'}
      {#each values as value, i (i)}
        {@const height = value === 0 ? 1 : Math.max(2, (value / top) * HEIGHT)}
        <rect
          class:partial={partialLast && i === values.length - 1}
          class:zero={value === 0}
          x={i * STEP + 1}
          y={HEIGHT - height}
          width={STEP - 2}
          {height}
          rx="1"
        />
      {/each}
    {:else}
      <polyline points={points} fill="none" vector-effect="non-scaling-stroke" />
    {/if}
  </svg>
  <table class="visually-hidden">
    <caption>{caption}</caption>
    <tbody>
      {#each values as value, i (i)}
        <tr><th scope="row">{labels[i] ?? ''}</th><td>{format(value)}</td></tr>
      {/each}
    </tbody>
  </table>
</figure>

<style>
  .spark { margin: 0; }
  svg { display: block; width: 100%; height: 44px; }
  rect { fill: currentColor; }
  rect.partial { opacity: 0.4; }
  rect.zero { opacity: 0.35; }
  polyline { stroke: currentColor; stroke-width: 1.8; stroke-linejoin: round; }
  .ink-accent { color: var(--accent); }
  .ink-ok { color: var(--ok); }
  .ink-fail { color: var(--fail); }
</style>
