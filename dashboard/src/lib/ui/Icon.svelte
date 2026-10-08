<script lang="ts">
  import type { IconName } from '$lib/status';

  /** One of the dashboard's icons, drawn inline (the CSP allows no image
   * from elsewhere) in the current text colour. Decorative: the word next
   * to it says the same. */
  let { name, size = 16 }: { name: IconName; size?: number } = $props();
</script>

<svg
  class="icon icon-{name}"
  width={size}
  height={size}
  viewBox="0 0 16 16"
  fill="none"
  stroke="currentColor"
  stroke-width="1.6"
  stroke-linecap="round"
  stroke-linejoin="round"
  aria-hidden="true"
  focusable="false"
>
  {#if name === 'spinner'}
    <path d="M8 2.5a5.5 5.5 0 1 0 5.5 5.5" />
  {:else if name === 'circle'}
    <circle cx="8" cy="8" r="5" />
  {:else if name === 'check'}
    <path d="M3.5 8.5l3 3 6-7" />
  {:else if name === 'cross'}
    <path d="M4.5 4.5l7 7M11.5 4.5l-7 7" />
  {:else if name === 'slash'}
    <circle cx="8" cy="8" r="5.5" /><path d="M4.2 11.8l7.6-7.6" />
  {:else if name === 'minus'}
    <circle cx="8" cy="8" r="5.5" /><path d="M5.5 8h5" />
  {:else if name === 'broken'}
    <path d="M7 2.6A5.5 5.5 0 0 0 7 13.4M9 13.4A5.5 5.5 0 0 0 9 2.6" />
  {:else if name === 'hourglass'}
    <path d="M4.5 2.5h7M4.5 13.5h7M5.5 2.5c0 3 5 3.5 5 5.5s-5 2.5-5 5.5M10.5 2.5c0 3-5 3.5-5 5.5" />
  {:else if name === 'triangle'}
    <path d="M8 2.5l6 10.5H2zM8 6.5v3M8 11.3v.2" />
  {:else if name === 'shield'}
    <path d="M8 2l5 2v4c0 3-2.2 5-5 6-2.8-1-5-3-5-6V4z" />
  {:else if name === 'clock'}
    <circle cx="8" cy="8" r="5.5" /><path d="M8 5v3l2 1.5" />
  {:else if name === 'link'}
    <path d="M6.5 9.5l3-3M7 4.5l1-1a2.5 2.5 0 0 1 3.5 3.5l-1 1M9 11.5l-1 1a2.5 2.5 0 0 1-3.5-3.5l1-1" />
  {:else if name === 'pencil'}
    <path d="M3 13l.7-3L10.5 3.2l2.3 2.3L6 12.3zM9.3 4.4l2.3 2.3" />
  {:else if name === 'undo'}
    <path d="M5.5 4L3 6.5 5.5 9M3 6.5h6.5a3.5 3.5 0 0 1 0 7H7" />
  {:else if name === 'copy'}
    <rect x="5.5" y="5.5" width="8" height="8" rx="1.5" /><path d="M10.5 5.5V3.5a1 1 0 0 0-1-1h-6a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h2" />
  {:else if name === 'menu'}
    <path d="M2.5 4.5h11M2.5 8h11M2.5 11.5h11" />
  {:else if name === 'plus'}
    <path d="M8 3v10M3 8h10" />
  {:else if name === 'info'}
    <circle cx="8" cy="8" r="5.5" /><path d="M8 7.5v3.5M8 5v.2" />
  {/if}
</svg>
