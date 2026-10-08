<script lang="ts">
  import type { LaneDot, StageDot } from '$lib/api/types';
  import { lookOf } from '$lib/status';
  import Icon from '$lib/ui/Icon.svelte';

  /** A run's stages as a row of dots (#225), the lanes stage as one dot
   * per lane. Each dot says its stage and state on hover and to screen
   * readers. */
  let { stages, lanes }: { stages: StageDot[]; lanes: LaneDot[] } = $props();

  const LOOKS: Record<string, string> = { done: 'done', running: 'running', failed: 'failed', skipped: 'skipped' };
  let reviewLanes = $derived(lanes.filter((lane) => !lane.name.startsWith('check-')));
</script>

<ol class="stepper" aria-label="Stages">
  {#each stages as stage (stage.name)}
    {#if stage.name === 'lanes' && reviewLanes.length > 0}
      <li class="group">
        <ol class="lanes" aria-label="Lanes">
          {#each reviewLanes as lane (lane.name)}
            {@const look = lookOf(lane.status)}
            <li class="dot tone-{look.tone}" title="{lane.name}: {look.label}">
              <Icon name={look.icon} size={12} /><span class="visually-hidden">{lane.name}: {look.label}</span>
            </li>
          {/each}
        </ol>
      </li>
    {:else}
      {@const look = lookOf(LOOKS[stage.state] ?? stage.state)}
      <li class="dot tone-{look.tone}" title="{stage.name.replaceAll('_', ' ')}: {look.label}">
        <Icon name={look.icon} size={12} /><span class="visually-hidden">{stage.name.replaceAll('_', ' ')}: {look.label}</span>
      </li>
    {/if}
  {/each}
</ol>

<style>
  .stepper, .lanes { list-style: none; margin: 0; padding: 0; display: flex; align-items: center; }
  .stepper > li + li::before {
    content: '';
    display: inline-block;
    width: 8px;
    height: 1px;
    background: var(--line);
    vertical-align: middle;
  }
  .stepper > li { display: inline-flex; align-items: center; }
  .group .lanes {
    gap: 2px;
    padding: 1px 3px;
    border: 1px solid var(--line);
    border-radius: 999px;
  }
  .dot {
    display: inline-grid;
    place-items: center;
    width: 20px;
    height: 20px;
    border-radius: 50%;
  }
</style>
