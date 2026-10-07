<script lang="ts">
  import { onMount } from 'svelte';
  import { me } from '$lib/api/client';
  import type { Me } from '$lib/api/types';
  import { link, route, start } from '$lib/router';
  import Health from '$lib/views/Health.svelte';
  import Home from '$lib/views/Home.svelte';
  import NotFound from '$lib/views/NotFound.svelte';
  import Problem from '$lib/views/Problem.svelte';

  let who: Promise<Me> = $state(new Promise(() => {}));

  onMount(() => {
    who = me();
    return start();
  });
</script>

<header>
  <nav>
    <a href="/dashboard/app/" use:link class="brand">Meneer Henk</a>
    <a href="/dashboard">Runs</a>
    <a href="/dashboard/events">Events</a>
    <a href="/dashboard/app/health" use:link>Health</a>
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
  {:then}
    {#if $route.name === 'home'}
      <Home />
    {:else if $route.name === 'health'}
      <Health />
    {:else}
      <NotFound path={$route.path} />
    {/if}
  {:catch error}
    <Problem {error} />
  {/await}
</main>
