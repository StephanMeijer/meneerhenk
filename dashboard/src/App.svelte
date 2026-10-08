<script lang="ts">
  import { onMount } from 'svelte';
  import { me } from '$lib/api/client';
  import type { Me } from '$lib/api/types';
  import { href, link, navigate, route, start, type Route } from '$lib/router';
  import Dialog from '$lib/ui/Dialog.svelte';
  import Icon from '$lib/ui/Icon.svelte';
  import Problem from '$lib/ui/Problem.svelte';
  import Events from '$lib/views/Events.svelte';
  import Health from '$lib/views/Health.svelte';
  import NotFound from '$lib/views/NotFound.svelte';
  import Overview from '$lib/views/Overview.svelte';
  import Lanes from '$lib/views/Lanes.svelte';
  import Quality from '$lib/views/Quality.svelte';
  import Tools from '$lib/views/Tools.svelte';
  import Run from '$lib/views/Run.svelte';
  import StartForm from '$lib/views/StartForm.svelte';
  import Transcript from '$lib/views/Transcript.svelte';

  let who: Promise<Me> = $state(new Promise(() => {}));
  let menuOpen = $state(false);
  let starting = $state(false);

  /** Where a start lands: the request's page, with the dialog closed. */
  function landed(path: string): void {
    starting = false;
    navigate(path);
  }

  /** The tabs, in groups: what happened, how well, how Henk is set up. */
  const TABS: { path: string; label: string; routes: Route['name'][] }[][] = [
    [
      { path: '/', label: 'Runs', routes: ['overview', 'run', 'transcript'] },
      { path: '/events', label: 'Events', routes: ['events', 'event'] },
    ],
    [
      { path: '/quality', label: 'Quality', routes: ['quality'] },
      { path: '/lanes', label: 'Lanes', routes: ['lanes'] },
      { path: '/tools', label: 'Tools', routes: ['tools'] },
    ],
    [{ path: '/health', label: 'Health', routes: ['health'] }],
  ];

  onMount(() => {
    who = me();
    return start();
  });

  // A tab or link followed closes the phone menu.
  $effect(() => {
    void $route;
    menuOpen = false;
  });
</script>

<header class="shell">
  <div class="bar">
    <a href={href('/')} use:link class="brand"><span class="mark" aria-hidden="true">H</span>Meneer Henk</a>
    <button
      type="button"
      class="icon-button menu-button"
      aria-expanded={menuOpen}
      aria-controls="shell-menu"
      aria-label="Menu"
      onclick={() => (menuOpen = !menuOpen)}
    ><Icon name="menu" size={20} /></button>
    <div id="shell-menu" class="menu" class:open={menuOpen}>
      <nav aria-label="Pages">
        {#each TABS as group, index (index)}
          <div class="group">
            {#each group as tab (tab.path)}
              <a
                href={href(tab.path)}
                use:link
                class:current={tab.routes.includes($route.name)}
                aria-current={tab.routes.includes($route.name) ? 'page' : undefined}
              >{tab.label}</a>
            {/each}
          </div>
        {/each}
      </nav>
      <div class="actions">
        {#await who then viewer}
          {#if viewer.startable.length > 0}
            <button type="button" class="primary" onclick={() => {
              menuOpen = false;
              starting = true;
            }}><Icon name="plus" />Start a run</button>
          {/if}
          <form class="signout" method="post" action="/dashboard/logout">
            <span class="viewer" title="github:{viewer.github_id}">{viewer.login}</span>
            <input type="hidden" name="csrf" value={viewer.csrf}>
            <button class="plain">Sign out</button>
          </form>
        {/await}
      </div>
    </div>
  </div>
</header>

{#await who then viewer}
  <Dialog bind:open={starting} title="Start a run">
    <StartForm startable={viewer.startable} go={landed} />
  </Dialog>
{/await}

<main>
  {#await who}
    <p class="muted" aria-busy="true">Loading.</p>
  {:then viewer}
    {#if $route.name === 'overview'}
      <Overview me={viewer} query={$route.query} />
    {:else if $route.name === 'run'}
      {#key $route.id}
        <Run id={$route.id} who="github:{viewer.github_id}" />
      {/key}
    {:else if $route.name === 'transcript'}
      <Transcript id={$route.id} session={$route.session} />
    {:else if $route.name === 'events' || $route.name === 'event'}
      <!-- One branch for both, so choosing an event keeps the list mounted. -->
      <Events query={$route.query} selected={$route.name === 'event' ? $route.id : null} />
    {:else if $route.name === 'tools'}
      <Tools query={$route.query} />
    {:else if $route.name === 'quality'}
      <Quality query={$route.query} />
    {:else if $route.name === 'lanes'}
      <Lanes query={$route.query} />
    {:else if $route.name === 'health'}
      <Health />
    {:else}
      <NotFound path={$route.path} />
    {/if}
  {:catch error}
    <Problem {error} />
  {/await}
</main>

<style>
  .shell {
    background: var(--surface);
    border-bottom: 1px solid var(--line-soft);
  }
  .bar {
    max-width: 82rem;
    margin: 0 auto;
    padding: 0 var(--space-6);
    min-height: 56px;
    display: flex;
    align-items: center;
    gap: var(--space-6);
  }
  .brand {
    display: inline-flex;
    align-items: center;
    gap: 10px;
    font-weight: 600;
    font-size: 15px;
    color: var(--ink);
    white-space: nowrap;
  }
  .brand:hover { text-decoration: none; }
  .mark {
    display: inline-grid;
    place-items: center;
    width: 28px;
    height: 28px;
    border-radius: 6px;
    background: var(--ink);
    color: var(--bg);
    font-family: var(--mono);
    font-size: 13px;
    font-weight: 700;
  }
  .menu {
    flex: 1;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-4);
    min-width: 0;
  }
  nav { display: flex; align-items: center; }
  .group { display: flex; align-items: center; }
  .group + .group { border-left: 1px solid var(--line-soft); margin-left: var(--space-2); padding-left: var(--space-2); }
  nav a {
    color: var(--ink);
    font-size: 14px;
    padding: 17px 10px 15px;
    border-bottom: 2px solid transparent;
  }
  nav a:hover { text-decoration: none; color: var(--accent); }
  nav a.current { border-bottom-color: var(--ink); font-weight: 600; }
  .actions { display: flex; align-items: center; gap: var(--space-3); }
  .signout { display: flex; align-items: center; gap: var(--space-2); margin: 0; }
  .viewer { color: var(--muted); }
  .plain { border-color: transparent; background: transparent; }
  .menu-button { display: none; margin-left: auto; }

  @media (max-width: 860px) {
    .bar { flex-wrap: wrap; gap: 0; padding: 0 var(--space-4); }
    .menu-button { display: inline-flex; }
    .menu { display: none; flex-basis: 100%; flex-direction: column; align-items: stretch; padding-bottom: var(--space-3); }
    .menu.open { display: flex; }
    nav { flex-direction: column; align-items: stretch; }
    .group { flex-direction: column; align-items: stretch; }
    .group + .group { border-left: 0; margin-left: 0; padding-left: 0; border-top: 1px solid var(--line-soft); }
    nav a { padding: 10px 4px; border-bottom: 0; border-left: 2px solid transparent; }
    nav a.current { border-left-color: var(--ink); }
    .actions { flex-wrap: wrap; justify-content: space-between; }
  }
</style>
