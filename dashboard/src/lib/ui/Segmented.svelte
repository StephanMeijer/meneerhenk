<script lang="ts">
  import { href, link } from '$lib/router';

  /** A choice of one, as a row of links that keep the page's query
   * (#228): the chosen one is marked for sight and for screen readers. */
  let {
    label,
    options,
    current,
    to,
  }: {
    label: string;
    options: { value: string; label: string }[];
    current: string;
    to: (value: string) => string;
  } = $props();
</script>

<div class="segmented-field">
  <span class="segmented-label">{label}</span>
  <nav class="segmented" aria-label={label}>
    {#each options as option (option.value)}
      <a
        href={href(to(option.value))}
        use:link
        class:chosen={option.value === current}
        aria-current={option.value === current ? 'true' : undefined}
      >{option.label}</a>
    {/each}
  </nav>
</div>

<style>
  .segmented-field { display: flex; flex-direction: column; gap: 2px; }
  .segmented-label { font-size: 13px; color: var(--muted); }
  .segmented {
    display: inline-flex;
    flex-wrap: wrap;
    gap: 2px;
    padding: 2px;
    border: 1px solid var(--line);
    border-radius: var(--radius);
    background: var(--surface);
  }
  a {
    padding: 4px 10px;
    border-radius: 3px;
    color: var(--ink);
    font-size: 13.5px;
    white-space: nowrap;
  }
  a:hover { background: var(--sunk); text-decoration: none; }
  a.chosen { background: var(--ink); color: var(--bg); font-weight: 600; }
</style>
