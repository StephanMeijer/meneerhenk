<script lang="ts">
  import { onMount } from 'svelte';
  import { me } from '$lib/api/client';
  import type { Me } from '$lib/api/types';
  import { href, link, route, start } from '$lib/router';
  import Event from '$lib/views/Event.svelte';
  import Events from '$lib/views/Events.svelte';
  import Health from '$lib/views/Health.svelte';
  import NotFound from '$lib/views/NotFound.svelte';
  import Overview from '$lib/views/Overview.svelte';
  import Problem from '$lib/views/Problem.svelte';
  import Quality from '$lib/views/Quality.svelte';
  import Tools from '$lib/views/Tools.svelte';
  import Run from '$lib/views/Run.svelte';
  import Transcript from '$lib/views/Transcript.svelte';

  let who: Promise<Me> = $state(new Promise(() => {}));

  onMount(() => {
    who = me();
    return start();
  });
</script>

<header>
  <nav>
    <a href={href('/')} use:link class="brand">Meneer Henk</a>
    <a href={href('/')} use:link>Runs</a>
    <a href={href('/events')} use:link>Events</a>
    <a href={href('/quality')} use:link>Quality</a>
    <a href={href('/tools')} use:link>Tools</a>
    <a href={href('/health')} use:link>Health</a>
  </nav>
  {#await who then viewer}
    <form class="signout" method="post" action="/dashboard/logout">
      <span class="muted">{viewer.login}</span>
      <input type="hidden" name="csrf" value={viewer.csrf}>
      <button>Sign out</button>
    </form>
  {/await}
</header>

<main>
  {#await who}
    <p class="muted" aria-busy="true">Loading.</p>
  {:then viewer}
    {#if $route.name === 'overview'}
      <Overview me={viewer} query={$route.query} />
    {:else if $route.name === 'run'}
      {#key $route.id}
        <Run id={$route.id} />
      {/key}
    {:else if $route.name === 'transcript'}
      <Transcript id={$route.id} session={$route.session} />
    {:else if $route.name === 'events'}
      <Events query={$route.query} />
    {:else if $route.name === 'event'}
      <Event id={$route.id} />
    {:else if $route.name === 'tools'}
      <Tools query={$route.query} />
    {:else if $route.name === 'quality'}
      <Quality query={$route.query} />
    {:else if $route.name === 'health'}
      <Health />
    {:else}
      <NotFound path={$route.path} />
    {/if}
  {:catch error}
    <Problem {error} />
  {/await}
</main>
