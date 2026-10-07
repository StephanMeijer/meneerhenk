<script lang="ts">
  import { ApiError } from '$lib/api/client';

  let { error }: { error: unknown } = $props();

  let message = $derived(
    error instanceof ApiError
      ? error.message
      : 'Something went wrong in the dashboard itself. Reload the page.',
  );
</script>

<div class="problem" role="alert">
  <strong>Not loaded.</strong>
  {message}
</div>
