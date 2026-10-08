<script lang="ts">
  import { ApiError, cancelRun } from '$lib/api/client';
  import type { Cancelled, Slots, WaitingReview } from '$lib/api/types';
  import { now as clock } from '$lib/clock';
  import { about, duration, kindText } from '$lib/format';
  import Commit from '$lib/ui/Commit.svelte';
  import Dialog from '$lib/ui/Dialog.svelte';
  import Empty from '$lib/ui/Empty.svelte';
  import Icon from '$lib/ui/Icon.svelte';
  import Time from '$lib/ui/Time.svelte';

  /** What has been accepted but not started, in the order it will start
   * (#251), from the running stream's `slots`. A queued review has its run
   * id but no run page until it starts, so a row links its target. `shown`
   * are the run ids "Running now" lists: a review that just took its slot
   * may be there a moment before the next `slots` message drops it here.
   * `cancel` is replaced in the tests. */
  let {
    slots,
    shown = [],
    cancel = cancelRun,
    now = () => new Date(),
  }: {
    slots: Slots | null;
    shown?: string[];
    cancel?: (id: string) => Promise<Cancelled>;
    now?: () => Date;
  } = $props();

  /** Now, again on every tick of the shared clock, so the time waiting
   * moves on. */
  let at = $derived.by(() => {
    void $clock;
    return now();
  });

  let queued = $derived((slots?.waiting ?? []).filter((entry) => !shown.includes(entry.run_id)));

  /** The entry the dialog asks about, and whether it is open. */
  let asking: WaitingReview | null = $state(null);
  let confirming = $state(false);
  let cancelling: string | null = $state(null);
  let note: string | null = $state(null);

  const waited = (since: string, at: Date): string => duration(Math.max(0, at.getTime() - Date.parse(since)));

  function why(entry: WaitingReview): string {
    if (entry.reason === 'no_slot' && slots !== null) {
      return `no free slot (${slots.in_use} of ${slots.limit} in use)`;
    }
    return entry.reason.replaceAll('_', ' ');
  }

  async function stop(): Promise<void> {
    const entry = asking;
    confirming = false;
    if (entry === null) return;
    cancelling = entry.run_id;
    note = null;
    try {
      await cancel(entry.run_id);
      note = `Cancel sent. ${entry.run_id} ends as cancelled while queued.`;
    } catch (error) {
      note = error instanceof ApiError ? error.message : 'The cancel did not go through.';
    } finally {
      cancelling = null;
    }
  }
</script>

<section class="panel" aria-labelledby="queued-title">
  <div class="panel-head">
    <h2 id="queued-title">Queued <span class="count-badge">({queued.length})</span></h2>
    <span class="note">In the order they will start</span>
  </div>
  {#if queued.length === 0}
    <Empty why="Nothing is waiting." />
  {:else}
    <ol class="queued">
      {#each queued as entry (entry.run_id)}
        <li>
          <span class="position" aria-label="Position {entry.position}">{entry.position}</span>
          <span class="who-what">
            <span class="mono">{entry.run_id}</span>
            <span class="muted">{kindText(entry.kind)}</span>
          </span>
          <span class="about">
            {#if entry.target_url}
              <a href={entry.target_url} rel="noreferrer">{about(entry.repo, entry.target)}</a>
            {:else}
              {about(entry.repo, entry.target)}
            {/if}
            <Commit sha={entry.commit} />
          </span>
          <span class="trigger">{entry.trigger}{#if entry.requester}<span class="sub who">{entry.requester}</span>{/if}</span>
          <span class="when">
            <span>waiting <strong class="num">{waited(entry.since, at)}</strong></span>
            <span class="muted">requested <Time iso={entry.since} /></span>
          </span>
          <span class="why muted">{why(entry)}</span>
          <button type="button" class="danger" onclick={() => {
              asking = entry;
              confirming = true;
            }} disabled={cancelling === entry.run_id}>
            <Icon name="slash" />Cancel
          </button>
        </li>
      {/each}
    </ol>
  {/if}
  {#if note !== null}
    <div class="panel-foot"><p class="note" role="status">{note}</p></div>
  {/if}
</section>

<Dialog bind:open={confirming} title="Cancel this {kindText(asking?.kind ?? 'review')}?">
  {#if asking}
    <p>
      <code>{asking.run_id}</code> on <strong>{about(asking.repo, asking.target)}</strong> has waited
      {waited(asking.since, at)} for a slot and has not started.
    </p>
    <ul class="effects">
      <li>It ends as cancelled while queued, and the pull request gets one comment saying so.</li>
      <li>The reviews behind it move up.</li>
    </ul>
  {/if}
  {#snippet actions()}
    <button type="button" data-default onclick={() => (confirming = false)}>Keep waiting</button>
    <button type="button" class="danger-solid" onclick={stop}>Cancel {kindText(asking?.kind ?? 'review')}</button>
  {/snippet}
</Dialog>

<style>
  .queued { list-style: none; margin: 0; padding: 0; border-top: 1px solid var(--line-soft); }
  .queued > li {
    display: grid;
    grid-template-columns: 2rem minmax(10rem, 1.2fr) minmax(10rem, 1.2fr) minmax(8rem, 1fr) minmax(9rem, 1fr) minmax(9rem, 1fr) auto;
    gap: var(--space-2) var(--space-3);
    align-items: center;
    padding: 10px var(--space-4);
    border-bottom: 1px solid var(--line-soft);
  }
  .queued > li:last-child { border-bottom: 0; }
  .position {
    display: inline-grid;
    place-items: center;
    width: 1.6rem;
    height: 1.6rem;
    border-radius: 50%;
    background: var(--sunk);
    font-weight: 600;
    font-variant-numeric: tabular-nums;
  }
  .who-what, .about, .trigger, .when { display: flex; flex-direction: column; min-width: 0; overflow-wrap: anywhere; }
  .why { font-size: 13px; }
  .effects { margin: 0; padding-left: 1.2rem; }
  .panel-foot .note { margin: 0; }

  .queued button { justify-self: end; white-space: nowrap; }

  @media (max-width: 960px) {
    .queued > li { grid-template-columns: 2rem minmax(0, 1fr) minmax(0, 1fr); }
    .position { grid-column: 1; grid-row: 1 / span 3; align-self: start; }
  }
  @media (max-width: 600px) {
    .queued > li { grid-template-columns: 2rem minmax(0, 1fr); align-items: start; }
    .queued > li > :not(.position) { grid-column: 2; }
    .position { grid-row: 1 / span 6; }
    .queued button { justify-self: start; }
  }
</style>
