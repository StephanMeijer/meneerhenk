<script lang="ts">
  import { everySecond } from '$lib/clock';
  import { duration } from '$lib/format';

  /** How long something still going has been going, counting every second
   * (#252): `4m 12s`, or `1h 03m` from an hour. Shown only while it goes:
   * once it ends the view shows the final time from the stream, this
   * unmounts, and the clock stops when nothing else reads it. `now` is
   * for the tests. */
  let { since, now }: { since: string; now?: () => Date } = $props();

  let at = $derived.by(() => {
    const tick = $everySecond;
    return now ? now() : tick;
  });
  let start = $derived(Date.parse(since));
</script>

{#if !Number.isNaN(start)}{duration(at.getTime() - start)}{/if}
