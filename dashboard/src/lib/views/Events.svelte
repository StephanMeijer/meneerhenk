<script lang="ts">
  import { event as loadEvent, eventFacets, events as listEvents } from '$lib/api/client';
  import type { EventDetail, EventFacets, EventItem, Page } from '$lib/api/types';
  import { about } from '$lib/format';
  import { eventPath, href, link, navigate, withQuery } from '$lib/router';
  import Empty from '$lib/ui/Empty.svelte';
  import Loading from '$lib/ui/Loading.svelte';
  import Pager from '$lib/ui/Pager.svelte';
  import Problem from '$lib/ui/Problem.svelte';
  import Time from '$lib/ui/Time.svelte';
  import EventInspector from './EventInspector.svelte';
  import Outcomes from './Outcomes.svelte';

  /** The events and the one chosen beside them (#227). `/events` shows
   * the newest; `/events/{id}` that one. The loaders are replaced in the
   * tests. */
  let {
    query,
    selected = null,
    load = listEvents,
    loadFacets = eventFacets,
    loadOne = loadEvent,
  }: {
    query: URLSearchParams;
    selected?: string | null;
    load?: (query: string) => Promise<Page<EventItem>>;
    loadFacets?: () => Promise<EventFacets>;
    loadOne?: (id: string) => Promise<EventDetail>;
  } = $props();

  const KEYS = ['source', 'kind', 'repo', 'cursor'];

  let listQuery = $derived(
    new URLSearchParams([...query.entries()].filter(([key, value]) => KEYS.includes(key) && value !== '')).toString(),
  );
  let page = $derived(load(listQuery));
  let facets = $derived(loadFacets());

  /** The page's own query, to keep when choosing an event. */
  let kept = $derived(Object.fromEntries([...query.entries()].filter(([key]) => KEYS.includes(key))));

  function filter(event: SubmitEvent): void {
    event.preventDefault();
    const form = new FormData(event.currentTarget as HTMLFormElement);
    const value = (key: string): string => String(form.get(key) ?? '');
    navigate(withQuery('/events', { source: value('source'), kind: value('kind'), repo: value('repo') }));
  }

  function choose(id: string): void {
    navigate(withQuery(eventPath(id), kept), { keepScroll: true });
  }
</script>

<div class="page-head">
  <h1>Events</h1>
  <p class="intro">
    Every request that reached Henk: webhooks, API calls, starts and cancels from here. And what
    each listener did with it, so "why did Henk not review my pull request?" has an answer.
  </p>
</div>

<section class="panel">
  <div class="panel-head">
    <form class="filters" onsubmit={filter}>
      {#await facets then known}
        {#each [['source', known.sources], ['kind', known.kinds]] as const as [name, options] (name)}
          <label>
            {name}
            <select {name} value={query.get(name) ?? ''}>
              <option value="">any</option>
              {#each options as option (option)}
                <option value={option}>{option}</option>
              {/each}
            </select>
          </label>
        {/each}
      {:catch}
        {#each ['source', 'kind'] as name (name)}
          <label>{name} <input {name} value={query.get(name) ?? ''}></label>
        {/each}
      {/await}
      <label>repository <input name="repo" value={query.get('repo') ?? ''} placeholder="owner/name"></label>
      <button>Show</button>
    </form>
  </div>
</section>

{#await page}
  <Loading />
{:then result}
  {@const shown = selected ?? result.items[0]?.event.id ?? null}
  <div class="split">
    <section class="panel list">
      {#if result.items.length === 0}
        <Empty why="No events match these filters." />
      {:else}
        <div class="scroll">
          <table class="events">
            <thead>
              <tr><th>Event</th><th>Received</th><th>Source</th><th>About</th><th>What the listeners did</th></tr>
            </thead>
            <tbody>
              {#each result.items as item (item.event.id)}
                <tr
                  class:chosen={item.event.id === shown}
                  aria-current={item.event.id === shown ? 'true' : undefined}
                  onclick={(e) => {
                    if (!(e.target instanceof HTMLAnchorElement)) choose(item.event.id);
                  }}
                >
                  <td>
                    <a class="mono" href={href(withQuery(eventPath(item.event.id), kept))} use:link={{ keepScroll: true }}>{item.event.id}</a>
                  </td>
                  <td class="nowrap"><Time iso={item.event.received_at} /></td>
                  <td>
                    <span class="chip-tag mono">{item.event.source}</span>
                    <span class="chip-tag mono kind">{item.event.kind}</span>
                  </td>
                  <td>
                    {#if item.event.repo}
                      {about(item.event.repo, item.event.target)}
                    {:else}
                      <span class="muted">no repository</span>
                    {/if}
                    {#if item.event.requester}<span class="sub who">{item.event.requester}</span>{/if}
                  </td>
                  <td>
                    {#if item.outcomes.length === 0}
                      <span class="muted">no listener</span>
                    {:else}
                      <Outcomes outcomes={item.outcomes} />
                    {/if}
                  </td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
      {/if}
      <div class="panel-foot">
        <span>Newest first.</span>
        <Pager path="/events" {query} next={result.next} />
      </div>
    </section>
    {#if shown !== null}
      {#key shown}
        <EventInspector id={shown} load={loadOne} />
      {/key}
    {/if}
  </div>
{:catch error}
  <Problem {error} />
{/await}

<style>
  .split { display: grid; grid-template-columns: minmax(0, 3fr) minmax(0, 2fr); gap: var(--space-4); align-items: start; }
  .events tbody tr { cursor: pointer; }
  .events tr.chosen td { background: var(--accent-soft); }
  .events tr.chosen td:first-child { box-shadow: inset 3px 0 0 var(--accent); }
  .chip-tag {
    display: inline-block;
    font-size: 12px;
    padding: 0 6px;
    border-radius: var(--radius-sm);
    background: var(--sunk);
    border: 1px solid var(--line-soft);
    margin: 0 4px 2px 0;
  }

  @media (max-width: 960px) {
    .split { grid-template-columns: 1fr; }
  }
</style>
