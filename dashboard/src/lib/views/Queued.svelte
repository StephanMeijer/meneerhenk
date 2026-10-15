<script lang="ts">
  import { ApiError, cancelRun, moveQueuedRun } from '$lib/api/client';
  import type { Cancelled, Moved, Slots, WaitingReview } from '$lib/api/types';
  import { about, kindText } from '$lib/format';
  import Commit from '$lib/ui/Commit.svelte';
  import Dialog from '$lib/ui/Dialog.svelte';
  import Elapsed from '$lib/ui/Elapsed.svelte';
  import Empty from '$lib/ui/Empty.svelte';
  import Icon from '$lib/ui/Icon.svelte';
  import Time from '$lib/ui/Time.svelte';

  /** What has been accepted but not started, in the order it will start
   * (#251), from the running stream's `slots`. A queued review has its run
   * id but no run page until it starts, so a row links its target. `shown`
   * are the run ids "Running now" lists: a review that just took its slot
   * may be there a moment before the next `slots` message drops it here.
   * A colleague may move one to another place (#401): by dragging its row,
   * or with "Move to top" and "Move to bottom", but only among runs of its
   * priority: a rebase waits ahead of every review (#580), and a review
   * waiting for a rebase does not move at all. The list keeps the server's
   * order: the `slots` message after a move redraws it for everyone.
   * `cancel`, `move` and `now` are replaced in the tests. */
  let {
    slots,
    shown = [],
    cancel = cancelRun,
    move = moveQueuedRun,
    now,
  }: {
    slots: Slots | null;
    shown?: string[];
    cancel?: (id: string) => Promise<Cancelled>;
    move?: (id: string, position: number) => Promise<Moved>;
    now?: () => Date;
  } = $props();

  let queued = $derived((slots?.waiting ?? []).filter((entry) => !shown.includes(entry.run_id)));

  /** The entry the dialog asks about, and whether it is open. */
  let asking: WaitingReview | null = $state(null);
  let confirming = $state(false);
  let cancelling: string | null = $state(null);
  let note: string | null = $state(null);
  /** The run being moved, and the rows a drag is over. */
  let moving: string | null = $state(null);
  let dragged: string | null = $state(null);
  let over: string | null = $state(null);
  /** Whether an entry waits for a slot, and so may move. */
  const movable = (entry: WaitingReview): boolean => entry.reason === 'no_slot';

  /** The first and last places an entry may move to: those of its
   * priority, rows "Running now" shows included. */
  function places(entry: WaitingReview): [number, number] {
    const peers = (slots?.waiting ?? [])
      .filter((e) => movable(e) && e.priority === entry.priority)
      .map((e) => e.position);
    return [Math.min(...peers, entry.position), Math.max(...peers, entry.position)];
  }

  async function moveTo(entry: WaitingReview, position: number): Promise<void> {
    if (position === entry.position) return;
    moving = entry.run_id;
    note = null;
    try {
      const moved = await move(entry.run_id, position);
      note = `Moved ${moved.run_id} from position ${moved.from} to ${moved.to}.`;
    } catch (error) {
      note = error instanceof ApiError ? error.message : 'The move did not go through.';
    } finally {
      moving = null;
    }
  }

  function dropOn(target: WaitingReview): void {
    const entry = queued.find((e) => e.run_id === dragged);
    dragged = null;
    over = null;
    if (entry === undefined || !movable(entry) || !movable(target)) return;
    if (entry.priority === target.priority) void moveTo(entry, target.position);
  }

  function why(entry: WaitingReview): string {
    if (entry.reason === 'no_slot' && slots !== null) {
      return `no free slot (${slots.in_use} of ${slots.limit} in use)`;
    }
    if (entry.reason === 'after_rebase') {
      return entry.waits_for === null ? 'after the rebase' : `after the rebase ${entry.waits_for}`;
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
        <li
          draggable={movable(entry)}
          class:dragged={dragged === entry.run_id}
          class:over={over === entry.run_id && dragged !== entry.run_id}
          ondragstart={(event) => {
            dragged = entry.run_id;
            event.dataTransfer?.setData('text/plain', entry.run_id);
          }}
          ondragover={(event) => {
            if (dragged === null) return;
            event.preventDefault();
            over = entry.run_id;
          }}
          ondragleave={() => {
            if (over === entry.run_id) over = null;
          }}
          ondrop={(event) => {
            event.preventDefault();
            dropOn(entry);
          }}
          ondragend={() => {
            dragged = null;
            over = null;
          }}
        >
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
            {#if entry.commit !== null}<Commit sha={entry.commit} />{/if}
          </span>
          <span class="trigger">{entry.trigger}{#if entry.requester}<span class="sub who">{entry.requester}</span>{/if}</span>
          <span class="when">
            <span>waiting <strong class="num"><Elapsed since={entry.since} {now} /></strong></span>
            <span class="muted">requested <Time iso={entry.since} /></span>
          </span>
          <span class="why muted">{why(entry)}</span>
          <span class="actions">
            {#if movable(entry) && entry.position > places(entry)[0]}
              <button type="button" aria-label="Move {entry.run_id} to the top" onclick={() => moveTo(entry, places(entry)[0])}
                disabled={moving === entry.run_id}>
                <Icon name="arrow-up" />Move to top
              </button>
            {/if}
            {#if movable(entry) && entry.position < places(entry)[1]}
              <button type="button" aria-label="Move {entry.run_id} to the bottom" onclick={() => moveTo(entry, places(entry)[1])}
                disabled={moving === entry.run_id}>
                <Icon name="arrow-down" />Move to bottom
              </button>
            {/if}
            <button type="button" class="danger" onclick={() => {
                asking = entry;
                confirming = true;
              }} disabled={cancelling === entry.run_id}>
              <Icon name="slash" />Cancel
            </button>
          </span>
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
      <Elapsed since={asking.since} {now} /> for a slot and has not started.
    </p>
    <ul class="effects">
      <li>It ends as cancelled while queued, and its queued check on the pull request closes as cancelled.</li>
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

  .queued > li[draggable='true'] { cursor: grab; }
  .queued > li.dragged { opacity: 0.5; }
  .queued > li.over { box-shadow: inset 0 2px 0 var(--accent); }
  .actions { display: flex; flex-wrap: wrap; gap: var(--space-2); justify-self: end; }
  .queued button { white-space: nowrap; }

  @media (max-width: 960px) {
    .queued > li { grid-template-columns: 2rem minmax(0, 1fr) minmax(0, 1fr); }
    .position { grid-column: 1; grid-row: 1 / span 3; align-self: start; }
  }
  @media (max-width: 600px) {
    .queued > li { grid-template-columns: 2rem minmax(0, 1fr); align-items: start; }
    .queued > li > :not(.position) { grid-column: 2; }
    .position { grid-row: 1 / span 6; }
    .actions { justify-self: start; }
  }
</style>
