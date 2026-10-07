<script lang="ts">
  import { link } from '$lib/router';

  let { path }: { path: string } = $props();
</script>

<h1>Not here</h1>
<p>The dashboard has no page at <code>{path}</code>.</p>
<p><a href="/dashboard/app/" use:link>Back to the start</a>.</p>
