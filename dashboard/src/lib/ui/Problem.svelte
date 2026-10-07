<script lang="ts">
  import { ApiError } from '$lib/api/client';
  import Icon from './Icon.svelte';

  /** Why something did not load. A refusal (403) reads as one, not as a
   * fault; `retry`, when given, offers "Try again". */
  let { error, retry }: { error: unknown; retry?: () => void } = $props();

  let message = $derived(
    error instanceof ApiError
      ? error.message
      : 'Something went wrong in the dashboard itself. Reload the page.',
  );
  let refused = $derived(error instanceof ApiError && error.status === 403);
</script>

<div class="problem" class:refused role="alert">
  <Icon name={refused ? 'shield' : 'cross'} />
  <div>
    <p><strong>Not loaded.</strong> {message}</p>
    {#if retry && !refused}
      <button type="button" onclick={retry}>Try again</button>
    {/if}
  </div>
</div>
