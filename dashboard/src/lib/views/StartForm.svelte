<script lang="ts">
  import { ApiError, startRun } from '$lib/api/client';
  import type { StartRequest, Started } from '$lib/api/types';
  import { eventPath, navigate } from '$lib/router';

  /** `start` and `go` are replaced in the tests. */
  let {
    startable,
    start = startRun,
    go = navigate,
  }: {
    startable: string[];
    start?: (request: StartRequest) => Promise<Started>;
    go?: (path: string) => void;
  } = $props();

  const LABELS: Record<string, string> = {
    review: 'review a pull request',
    plan: 'plan an issue',
    address: 'address the review feedback',
  };

  let kind = $state('review');
  let url = $state('');
  let commit = $state('');
  let note = $state('');
  let sending = $state(false);
  let problem: string | null = $state(null);

  async function submit(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    sending = true;
    problem = null;
    try {
      const started = await start({
        kind,
        url: url.trim(),
        commit: commit.trim() === '' ? null : commit.trim(),
        note: note.trim() === '' ? null : note.trim(),
      });
      go(eventPath(started.event_id));
    } catch (error) {
      problem = error instanceof ApiError ? error.message : 'The start did not go through.';
    } finally {
      sending = false;
    }
  }
</script>

<form class="filters" onsubmit={submit}>
  <label>
    what
    <select name="kind" bind:value={kind}>
      {#each startable as option (option)}
        <option value={option}>{LABELS[option] ?? option}</option>
      {/each}
    </select>
  </label>
  <label>
    URL
    <input name="url" required bind:value={url} placeholder="https://github.com/owner/name/pull/7">
  </label>
  <label>
    commit
    <input name="commit" bind:value={commit} placeholder="review only; default the head">
  </label>
  <label>
    note
    <input name="note" bind:value={note} placeholder="plan or address only">
  </label>
  <button class="primary" disabled={sending}>{sending ? 'Starting' : 'Start'}</button>
</form>
{#if problem !== null}
  <p class="problem" role="alert">Not started. {problem}</p>
{/if}
