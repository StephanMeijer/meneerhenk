<script lang="ts">
  /** Nothing to show, and why. */
  let { why = '' }: { why?: string } = $props();
</script>

<div class="empty">
  <strong>None.</strong>
  {#if why !== ''}<span class="muted">{why}</span>{/if}
</div>
