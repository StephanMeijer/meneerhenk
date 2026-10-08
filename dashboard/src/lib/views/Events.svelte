<script lang="ts">
  import { events as listEvents } from '$lib/api/client';
  import type { EventItem, Page } from '$lib/api/types';
  import { about } from '$lib/format';
  import { eventPath, href, link, navigate, withQuery } from '$lib/router';
  import Empty from '$lib/ui/Empty.svelte';
  import Loading from '$lib/ui/Loading.svelte';
  import Pager from '$lib/ui/Pager.svelte';
  import Problem from '$lib/ui/Problem.svelte';
  import Time from '$lib/ui/Time.svelte';
  import Outcomes from './Outcomes.svelte';

  let {
    query,
    load = listEvents,
  }: { query: URLSearchParams; load?: (query: string) => Promise<Page<EventItem>> } = $props();

  const FILTERS = [
    ['source', 'github_webhook'],
    ['kind', 'pull_request'],
    ['repo', 'owner/name'],
  ] as const;
  const KEYS = ['source', 'kind', 'repo', 'cursor'];

  let listQuery = $derived(
    new URLSearchParams([...query.entries()].filter(([key, value]) => KEYS.includes(key) && value !== '')).toString(),
  );
  let page = $derived(load(listQuery));

  function filter(event: SubmitEvent): void {
    event.preventDefault();
    const form = new FormData(event.currentTarget as HTMLFormElement);
    navigate(
      withQuery(
        '/events',
        Object.fromEntries(FILTERS.map(([name]) => [name, String(form.get(name) ?? '')])),
      ),
    );
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
      {#each FILTERS as [name, hint] (name)}
        <label>{name === 'repo' ? 'repository' : name} <input {name} value={query.get(name) ?? ''} placeholder={hint}></label>
      {/each}
      <button>Show</button>
    </form>
  </div>
  {#await page}
    <Loading />
  {:then result}
    {#if result.items.length === 0}
      <Empty why="No events match these filters." />
    {:else}
      <div class="scroll">
        <table>
          <thead>
            <tr><th>Event</th><th>Received</th><th>Source</th><th>Kind</th><th>About</th><th>What the listeners did</th></tr>
          </thead>
          <tbody>
            {#each result.items as item (item.event.id)}
              <tr>
                <td><a class="mono" href={href(eventPath(item.event.id))} use:link>{item.event.id}</a></td>
                <td class="nowrap"><Time iso={item.event.received_at} /></td>
                <td class="mono">{item.event.source}</td>
                <td class="mono">{item.event.kind}</td>
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
  {:catch error}
    <Problem {error} />
  {/await}
</section>
