<script lang="ts">
  import { now } from '$lib/clock';
  import { ago, utc } from '$lib/format';

  /** A time as "6 min ago", with the exact UTC time on hover. `at` is
   * fixed in the tests; otherwise the shared clock moves it on. */
  let { iso, at }: { iso: string; at?: Date } = $props();
</script>

<time datetime={utc(iso)} title={utc(iso)}>{ago(iso, at ?? $now)}</time>
