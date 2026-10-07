<script lang="ts">
  import { href, link } from '$lib/router';

  let { path }: { path: string } = $props();
</script>

<h1>Not here</h1>
<p>The dashboard has no page at <code>{path}</code>.</p>
<p><a href={href('/')} use:link>Back to the runs</a>.</p>
