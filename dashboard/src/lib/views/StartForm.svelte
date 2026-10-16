<script lang="ts">
  import { ApiError, startRun } from '$lib/api/client';
  import type { StartRequest, Started } from '$lib/api/types';
  import { eventPath, navigate } from '$lib/router';
  import { checkUrl, needs, type Start } from '$lib/targets';
  import Icon from '$lib/ui/Icon.svelte';

  /** What the start dialog holds (#230). `start`, `go` and `reload` are
   * replaced in the tests. */
  let {
    startable,
    start = startRun,
    go = navigate,
    reload = () => window.location.reload(),
  }: {
    startable: string[];
    start?: (request: StartRequest) => Promise<Started>;
    go?: (path: string) => void;
    reload?: () => void;
  } = $props();

  const CHOICES: { kind: Start; label: string; line: string }[] = [
    {
      kind: 'review',
      label: 'Review a pull request',
      line: "The reviewer and the executor fix what holds on the pull request's branch.",
    },
    { kind: 'plan', label: 'Plan an issue', line: 'Henk writes a plan on the issue.' },
    {
      kind: 'execute',
      label: 'Take an issue to a pull request',
      line: 'The executor makes the change on a new branch, Henk opens the pull request and reviews it.',
    },
    {
      kind: 'ticket',
      label: 'Take an issue through plan, build and review',
      line: 'Henk plans it, the executor makes the change and opens the pull request, and Henk reviews it, with no one starting each step.',
    },
    {
      kind: 'rebase',
      label: 'Rebase a pull request',
      line: 'Henk replays it onto its base branch, has the executor resolve conflicts, pushes with a lease and reviews it.',
    },
    {
      kind: 'fix_ci',
      label: "Fix a pull request's failing checks",
      line: 'The executor reads the checks that failed and fixes the code, Henk pushes the fix and reviews it. A failure that is not about the code pushes nothing.',
    },
  ];
  const BUTTONS: Record<Start, string> = {
    review: 'Start review',
    plan: 'Start plan',
    execute: 'Take it on',
    ticket: 'Start the run',
    rebase: 'Rebase it',
    fix_ci: 'Fix the checks',
  };

  let choices = $derived(CHOICES.filter((choice) => startable.includes(choice.kind)));
  let kind: Start = $state('review');
  let url = $state('');
  let commit = $state('');
  let note = $state('');
  let sending = $state(false);
  let problem: string | null = $state(null);
  let reloadable = $state(false);

  // Start on the first thing this Henk can start.
  $effect.pre(() => {
    const first = choices[0];
    if (first !== undefined && !choices.some((choice) => choice.kind === kind)) kind = first.kind;
  });

  let check = $derived(checkUrl(kind, url));
  let fits = $derived(check === null || 'ok' in check);

  async function submit(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    if (!fits) return;
    sending = true;
    problem = null;
    reloadable = false;
    try {
      const started = await start({
        kind,
        url: url.trim(),
        commit: kind === 'review' && commit.trim() !== '' ? commit.trim() : null,
        note: (kind === 'plan' || kind === 'execute' || kind === 'ticket') && note.trim() !== '' ? note.trim() : null,
      });
      go(eventPath(started.event_id));
    } catch (error) {
      problem = error instanceof ApiError ? error.message : 'The start did not go through.';
      reloadable = error instanceof ApiError && error.code === 'csrf';
    } finally {
      sending = false;
    }
  }
</script>

<form class="start" onsubmit={submit}>
  <fieldset class="choices">
    <legend class="visually-hidden">What to start</legend>
    {#each choices as choice (choice.kind)}
      <label class="choice" class:chosen={kind === choice.kind}>
        <input type="radio" name="kind" value={choice.kind} bind:group={kind}>
        <span>
          <strong>{choice.label}</strong>
          <span class="muted">{choice.line}</span>
        </span>
      </label>
    {/each}
  </fieldset>

  <label class="field">
    <span>{needs(kind) === 'pull' ? 'Pull request URL' : 'Issue URL'}</span>
    <input
      name="url"
      required
      autocomplete="off"
      bind:value={url}
      aria-invalid={!fits}
      aria-describedby="start-url-check"
      placeholder={needs(kind) === 'pull' ? 'https://github.com/owner/name/pull/7' : 'https://github.com/owner/name/issues/52'}
    >
    <span id="start-url-check" class="check" aria-live="polite">
      {#if check !== null && 'ok' in check}
        <span class="fits"><Icon name="check" size={13} />{check.ok}</span>
      {:else if check !== null}
        <span class="misfit"><Icon name="cross" size={13} />{check.problem}</span>
      {/if}
    </span>
  </label>

  {#if kind === 'review'}
    <label class="field">
      <span>Commit <span class="muted">optional</span></span>
      <input name="commit" autocomplete="off" bind:value={commit} placeholder="default: the head">
    </label>
  {:else if kind !== 'rebase' && kind !== 'fix_ci'}
    <label class="field">
      <span>Note <span class="muted">optional, Henk reads it with the issue</span></span>
      <textarea name="note" rows="3" bind:value={note}></textarea>
    </label>
  {/if}

  {#if problem !== null}
    <div class="problem" role="alert">
      <Icon name="cross" />
      <div>
        <p><strong>Not started.</strong> {problem}</p>
        {#if reloadable}
          <button type="button" onclick={reload}>Reload</button>
        {/if}
      </div>
    </div>
  {/if}

  <div class="send">
    <button class="primary" disabled={sending || !fits}>{sending ? 'Starting' : BUTTONS[kind]}</button>
    <span class="muted">You land on the request's page.</span>
  </div>
</form>

<style>
  .start { display: flex; flex-direction: column; gap: var(--space-3); }
  .choices { border: 0; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 6px; min-width: 0; }
  .choice {
    display: flex;
    gap: var(--space-2);
    align-items: flex-start;
    padding: 10px var(--space-3);
    border: 1px solid var(--line);
    border-radius: var(--radius-lg);
    cursor: pointer;
  }
  .choice input { min-width: 0; min-height: 0; margin-top: 3px; }
  .choice > span { display: flex; flex-direction: column; }
  .choice .muted { font-size: 13px; }
  .choice.chosen { border-color: var(--accent); background: var(--accent-soft); }
  .field { display: flex; flex-direction: column; gap: 4px; font-weight: 500; }
  .field .muted { font-weight: 400; font-size: 13px; }
  .field input, .field textarea { font-weight: 400; min-width: 0; width: 100%; }
  .field input[aria-invalid='true'] { border-color: var(--fail); }
  .check { font-size: 13px; font-weight: 400; min-height: 1.2em; }
  .fits, .misfit { display: inline-flex; gap: 4px; align-items: center; }
  .fits { color: var(--ok); }
  .misfit { color: var(--fail); }
  .send { display: flex; flex-wrap: wrap; gap: var(--space-3); align-items: center; }
</style>
