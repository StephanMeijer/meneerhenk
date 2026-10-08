<script lang="ts">
  import type { Lane, Stage } from '$lib/api/types';
  import { clockTime, duration, utc } from '$lib/format';
  import Elapsed from '$lib/ui/Elapsed.svelte';
  import Status from '$lib/ui/Status.svelte';

  /** A run's stages as boxes in the order they come (#226), the lanes
   * inside theirs. Selecting a box (or a lane) selects its log below;
   * selecting it again lets go. */
  let {
    stages,
    lanes,
    selected = $bindable(null),
  }: {
    stages: Stage[];
    lanes: Lane[];
    selected?: string | null;
  } = $props();

  const NAMES: Record<string, string> = {
    requested: 'Requested',
    queued: 'Queued',
    started: 'Started',
    diff: 'Diff',
    checkout: 'Checkout',
    workspace: 'Workspace',
    lanes: 'Lanes',
    session: 'Session',
    fact_check: 'Fact-check',
    commit: 'Commit',
    push: 'Push',
    publish: 'Publish',
    replies: 'Replies',
    done: 'Done',
  };

  /** A stage's time: the clock time of a moment, or how long it took. */
  function when(stage: Stage): string {
    const start = Date.parse(stage.started_at);
    const end = Date.parse(stage.ended_at ?? '');
    if (Number.isNaN(start) || Number.isNaN(end)) return '';
    if (stage.state !== 'running' && end - start < 1000) return clockTime(stage.started_at).replace(' UTC', '');
    return duration(end - start);
  }

  function laneTime(lane: Lane): string {
    const start = Date.parse(lane.started_at);
    const end = Date.parse(lane.finished_at ?? '');
    return Number.isNaN(start) || Number.isNaN(end) ? '' : duration(end - start);
  }

  function choose(key: string): void {
    selected = selected === key ? null : key;
  }
</script>

<ol class="pipeline">
  {#each stages as stage (stage.name)}
    <li class="stage {stage.state}" class:chosen={selected === stage.name}>
      <button
        type="button"
        class="box"
        aria-pressed={selected === stage.name}
        title="{NAMES[stage.name] ?? stage.name}: {stage.state}, from {utc(stage.started_at)}"
        onclick={() => choose(stage.name)}
      >
        <span class="head">
          <span class="name">{NAMES[stage.name] ?? stage.name}</span>
          <span class="time">{#if stage.ended_at === null}<Elapsed since={stage.started_at} />{:else}{when(stage)}{/if}</span>
        </span>
        <Status word={stage.state === 'done' ? 'done' : stage.state} kind="stage-state" />
        {#if stage.detail}<span class="detail">{stage.detail}</span>{/if}
      </button>
      {#if stage.name === 'lanes' && lanes.length > 0}
        <ul class="lanes">
          {#each lanes.filter((lane) => !lane.name.startsWith('check-')) as lane (lane.name)}
            <li>
              <button
                type="button"
                class="lane"
                class:chosen={selected === `lane:${lane.name}`}
                aria-pressed={selected === `lane:${lane.name}`}
                onclick={() => choose(`lane:${lane.name}`)}
              >
                <span class="mono">{lane.name}</span>
                <span class="muted mono model">{lane.model}</span>
                <Status word={lane.status} kind="lane-status" />
                <span class="muted">{#if lane.finished_at === null}<Elapsed since={lane.started_at} />{:else}{laneTime(lane)}{/if}</span>
              </button>
            </li>
          {/each}
        </ul>
      {/if}
    </li>
  {/each}
</ol>

<style>
  .pipeline {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(11.5rem, 1fr));
    gap: var(--space-2);
  }
  .stage { display: flex; flex-direction: column; gap: 4px; min-width: 0; }
  .stage:has(.lanes) { grid-column: span 2; }
  .box, .lane {
    width: 100%;
    min-height: 0;
    text-align: left;
    font-weight: 400;
    background: var(--surface);
    border: 1px solid var(--line-soft);
    border-radius: var(--radius-lg);
  }
  .box {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 6px;
    padding: var(--space-2) var(--space-3);
    height: 100%;
  }
  .box:hover, .lane:hover { background: var(--sunk); }
  .chosen > .box, .lane.chosen { border-color: var(--accent); box-shadow: inset 0 0 0 1px var(--accent); }
  .stage.running > .box { border-color: var(--accent); }
  .stage.failed > .box { border-color: var(--fail); }
  .stage.skipped > .box { background: var(--sunk); }
  .head { display: flex; width: 100%; justify-content: space-between; gap: var(--space-2); }
  .name { font-weight: 600; }
  .time { color: var(--muted); font-family: var(--mono); font-size: 12.5px; font-variant-numeric: tabular-nums; }
  .detail { color: var(--muted); font-size: 13px; overflow-wrap: anywhere; }
  .lanes { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 4px; }
  .lane {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 4px var(--space-2);
    padding: 6px var(--space-3);
    font-size: 13px;
  }
  .model { flex: 1; min-width: 6rem; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }

  @media (max-width: 720px) {
    .pipeline { grid-template-columns: 1fr; }
    .stage:has(.lanes) { grid-column: auto; }
  }
</style>
