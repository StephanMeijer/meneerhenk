<script lang="ts">
  /** Waiting for Henk: a few grey rows and "Loading." */
  let { lines = 3 }: { lines?: number } = $props();

  let skeleton = $derived(Array.from({ length: lines }, (_, index) => index));
</script>

<div class="loading" aria-busy="true">
  {#each skeleton as index (index)}
    <div class="skeleton" aria-hidden="true"></div>
  {/each}
  <p class="muted">Loading.</p>
</div>
