<script lang="ts">
  import type { SessionDot, StageDot } from '$lib/api/types';
  import { shownInRounds } from '$lib/roles';
  import { lookOf } from '$lib/status';
  import Icon from '$lib/ui/Icon.svelte';

  /** A run's stages as a row of dots (#225), a review's rounds as one dot
   * per session. Each dot says its stage and state on hover and to screen
   * readers. */
  let { stages, sessions }: { stages: StageDot[]; sessions: SessionDot[] } = $props();

  const LOOKS: Record<string, string> = { done: 'done', running: 'running', failed: 'failed', skipped: 'skipped' };
  let inRounds = $derived(sessions.filter(shownInRounds));
</script>

<ol class="stepper" aria-label="Stages">
  {#each stages as stage (stage.name)}
    {#if (stage.name === 'rounds' || stage.name === 'lanes') && inRounds.length > 0}
      <li class="group">
        <ol class="sessions" aria-label="Sessions">
          {#each inRounds as session (session.name)}
            {@const look = lookOf(session.status)}
            <li class="dot tone-{look.tone}" title="{session.name}: {look.label}">
              <Icon name={look.icon} size={12} /><span class="visually-hidden">{session.name}: {look.label}</span>
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
  .stepper, .sessions { list-style: none; margin: 0; padding: 0; display: flex; align-items: center; }
  .stepper > li + li::before {
    content: '';
    display: inline-block;
    width: 8px;
    height: 1px;
    background: var(--line);
    vertical-align: middle;
  }
  .stepper > li { display: inline-flex; align-items: center; }
  .group .sessions {
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
