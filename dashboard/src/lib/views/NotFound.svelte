<script lang="ts">
  import { href, link } from '$lib/router';
  import Icon from '$lib/ui/Icon.svelte';

  let { path }: { path: string } = $props();
</script>

<div class="page-head">
  <h1>Not here</h1>
</div>
<section class="panel">
  <div class="panel-body gone">
    <Icon name="minus" />
    <p><strong>No such page.</strong> The dashboard has no page at <code>{path}</code>.
      <a href={href('/')} use:link>Back to the runs</a>.</p>
  </div>
</section>

<style>
  .gone { display: flex; gap: var(--space-2); align-items: flex-start; color: var(--ink); }
  .gone :global(.icon) { color: var(--neutral); margin-top: 3px; flex: none; }
  .gone p { margin: 0; }
</style>
