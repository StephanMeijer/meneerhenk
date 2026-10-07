<script lang="ts">
  import { event as loadEvent } from '$lib/api/client';
  import type { EventDetail } from '$lib/api/types';
  import { about } from '$lib/format';
  import Outcomes from './Outcomes.svelte';
  import Problem from './Problem.svelte';

  /** While no listener has answered, the event is read again every
   * `every` ms, for at most `patience` ms. */
  let {
    id,
    load = loadEvent,
    every = 2000,
    patience = 30000,
  }: {
    id: string;
    load?: (id: string) => Promise<EventDetail>;
    every?: number;
    patience?: number;
  } = $props();

  let detail: EventDetail | null = $state(null);
  let problem: unknown = $state(null);

  $effect(() => {
    const wanted = id;
    const until = Date.now() + patience;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let live = true;
    detail = null;
    problem = null;
    const read = async (): Promise<void> => {
      try {
        const got = await load(wanted);
        if (!live) return;
        detail = got;
        if (got.outcomes.length === 0 && Date.now() < until) {
          timer = setTimeout(() => void read(), every);
        }
      } catch (error) {
        if (live) problem = error;
      }
    };
    void read();
    return () => {
      live = false;
      clearTimeout(timer);
    };
  });
</script>

{#if problem !== null}
  <Problem error={problem} />
{:else if detail === null}
  <p class="muted" aria-busy="true">Loading.</p>
{:else}
  <h1>Event {detail.event.id}</h1>
  <dl class="facts">
    <dt>Received</dt>
    <dd>{detail.event.received_at}</dd>
    <dt>Source</dt>
    <dd>{detail.event.source}</dd>
    <dt>Kind</dt>
    <dd>{detail.event.kind}</dd>
    {#if detail.event.repo}
      <dt>About</dt>
      <dd>{about(detail.event.repo, detail.event.target)}</dd>
    {/if}
    {#if detail.event.requester}
      <dt>Asked by</dt>
      <dd>{detail.event.requester}</dd>
    {/if}
  </dl>
  <h2>What the listeners did</h2>
  {#if detail.outcomes.length === 0}
    <p class="muted" aria-busy="true">No listener has answered yet.</p>
  {:else}
    <Outcomes outcomes={detail.outcomes} />
  {/if}
  {#if detail.payload !== null}
    <h2>Payload as received</h2>
    <pre>{detail.payload}</pre>
  {/if}
{/if}
