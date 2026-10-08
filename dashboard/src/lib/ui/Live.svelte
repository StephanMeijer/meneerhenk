<script lang="ts">
  import type { Connection } from '$lib/live';

  /** The live connection of a running run's page (#224), nothing once the
   * run ended. */
  let { connection }: { connection: Connection } = $props();

  const TITLES: Record<Exclude<Connection, 'ended'>, string> = {
    connecting: 'Opening the stream of this run.',
    live: 'Updates as it happens.',
    reconnecting: 'The stream dropped. What is shown stays; missed events replay.',
  };
</script>

{#if connection !== 'ended'}
  <span class="badge live-{connection}" title={TITLES[connection]}><span class="dot" aria-hidden="true"></span>{connection}</span>
{/if}
