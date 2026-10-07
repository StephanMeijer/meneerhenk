<script lang="ts">
  import { events as listEvents } from '$lib/api/client';
  import type { EventItem, Page } from '$lib/api/types';
  import { about } from '$lib/format';
  import { eventPath, href, link, navigate, withQuery } from '$lib/router';
  import Outcomes from './Outcomes.svelte';
  import Pager from './Pager.svelte';
  import Problem from './Problem.svelte';

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

<h1>Events</h1>
<form class="filters" onsubmit={filter}>
  {#each FILTERS as [name, hint] (name)}
    <label>{name} <input {name} value={query.get(name) ?? ''} placeholder={hint}></label>
  {/each}
  <button>Show</button>
</form>
{#await page}
  <p class="muted" aria-busy="true">Loading.</p>
{:then result}
  {#if result.items.length === 0}
    <p class="muted">None.</p>
  {:else}
    <table>
      <thead>
        <tr><th>Event</th><th>Received</th><th>Source</th><th>Kind</th><th>About</th><th>What the listeners did</th></tr>
      </thead>
      <tbody>
        {#each result.items as item (item.event.id)}
          <tr>
            <td><a href={href(eventPath(item.event.id))} use:link>{item.event.id}</a></td>
            <td>{item.event.received_at}</td>
            <td>{item.event.source}</td>
            <td>{item.event.kind}</td>
            <td>{about(item.event.repo, item.event.target)}</td>
            <td><Outcomes outcomes={item.outcomes} /></td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
  <Pager path="/events" {query} next={result.next} />
{:catch error}
  <Problem {error} />
{/await}
