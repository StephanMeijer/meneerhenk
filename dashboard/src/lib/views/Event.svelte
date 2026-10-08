<script lang="ts">
  import { event as loadEvent } from '$lib/api/client';
  import type { EventDetail } from '$lib/api/types';
  import { about, clockTime } from '$lib/format';
  import { href, link } from '$lib/router';
  import Loading from '$lib/ui/Loading.svelte';
  import Problem from '$lib/ui/Problem.svelte';
  import Status from '$lib/ui/Status.svelte';
  import Time from '$lib/ui/Time.svelte';
  import Outcomes from './Outcomes.svelte';

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
  <Loading />
{:else}
  <div class="page-head">
    <span class="crumbs"><a href={href('/events')} use:link>Events</a> / <code>{detail.event.id}</code></span>
    <h1>Event <span class="mono">{detail.event.id}</span></h1>
  </div>
  <section class="panel">
    <div class="panel-body">
      <dl class="facts">
        <div>
          <dt>Received</dt>
          <dd><Time iso={detail.event.received_at} /> <span class="muted mono">{clockTime(detail.event.received_at)}</span></dd>
        </div>
        <div><dt>Source</dt><dd class="mono">{detail.event.source}</dd></div>
        <div><dt>Kind</dt><dd class="mono">{detail.event.kind}</dd></div>
        <div>
          <dt>About</dt>
          <dd>{#if detail.event.repo}{about(detail.event.repo, detail.event.target)}{:else}<span class="muted">no repository</span>{/if}</dd>
        </div>
        <div>
          <dt>Asked by</dt>
          <dd>{#if detail.event.requester}<span class="mono">{detail.event.requester}</span>{:else}<span class="muted">not known</span>{/if}</dd>
        </div>
      </dl>
    </div>
  </section>
  <section class="panel">
    <div class="panel-head"><h2>What the listeners did</h2></div>
    <div class="panel-body">
      {#if detail.outcomes.length === 0}
        <p class="muted" aria-busy="true"><Status word="waiting" /> No listener has answered yet.</p>
      {:else}
        <Outcomes outcomes={detail.outcomes} />
      {/if}
    </div>
  </section>
  {#if detail.payload !== null}
    <section class="panel">
      <div class="panel-head">
        <h2>Payload as received <span class="data-note">Other people's text, shown as data</span></h2>
      </div>
      <div class="panel-body"><pre>{detail.payload}</pre></div>
    </section>
  {/if}
{/if}
