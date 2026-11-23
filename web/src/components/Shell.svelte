<script lang="ts">
  import { onMount, type Component } from 'svelte';
  import { href, link, route } from '../lib/router';
  import { startLiveState } from '../lib/state';
  import Activity from '../routes/activity/Activity.svelte';
  import Engines from '../routes/engines/Engines.svelte';
  import Hosts from '../routes/hosts/Hosts.svelte';
  import Models from '../routes/models/Models.svelte';
  import NotFound from '../routes/NotFound.svelte';
  import Overview from '../routes/overview/Overview.svelte';
  import Playground from '../routes/playground/Playground.svelte';
  import Settings from '../routes/settings/Settings.svelte';
  import Brand from './Brand.svelte';
  import ReconnectingIndicator from './ReconnectingIndicator.svelte';
  import Sidebar from './Sidebar.svelte';

  const SCREENS: Record<string, Component> = {
    '': Overview,
    models: Models,
    hosts: Hosts,
    engines: Engines,
    playground: Playground,
    activity: Activity,
    settings: Settings,
  };

  // The first path segment picks the screen; screens read the rest.
  const Screen = $derived.by(() => {
    const section = $route.split('/')[1] ?? '';
    return Object.hasOwn(SCREENS, section) ? SCREENS[section] : NotFound;
  });

  let menuOpen = $state(false);

  // SPEC R11: the live state runs while a signed-in screen is open.
  onMount(() => startLiveState());
</script>

<div class="shell">
  <header class="phone-header">
    <Brand height={36} />
  </header>
  <Sidebar open={menuOpen} onnavigate={() => (menuOpen = false)} />
  <main class="content">
    <ReconnectingIndicator />
    <Screen />
  </main>
  <!-- ADR 0040, owner decision 2026-11-13: quick actions from a phone. -->
  <nav class="phone-tabs" aria-label="Phone navigation">
    <a href={href('/')} use:link aria-current={$route === '/' ? 'page' : undefined} onclick={() => menuOpen = false}><span aria-hidden="true">▦</span>Overview</a>
    <a href={href('/models')} use:link aria-current={$route.startsWith('/models') ? 'page' : undefined} onclick={() => menuOpen = false}><span aria-hidden="true">◇</span>Models</a>
    <a href={href('/playground')} use:link aria-current={$route === '/playground' ? 'page' : undefined} onclick={() => menuOpen = false}><span aria-hidden="true">▷</span>Playground</a>
    <button type="button" aria-controls="sidebar" aria-expanded={menuOpen} onclick={() => menuOpen = !menuOpen}><span aria-hidden="true">···</span>More</button>
  </nav>
</div>

<style>
  .shell {
    min-height: 100vh;
    display: grid;
    grid-template-columns: 13.5rem minmax(0, 1fr);
  }
  .phone-header {
    display: none;
  }
  .phone-tabs { display: none; }
  .content {
    padding: 2rem 2.5rem;
    max-width: 1600px;
    width: 100%;
    min-width: 0;
  }
  /* SPEC R35, R36: on a phone the sidebar becomes a menu and the main area
     stacks. */
  @media (max-width: 767.98px) {
    .shell {
      display: block;
    }
    .phone-header {
      display: flex;
      align-items: center;
      gap: 0.5rem;
      height: 3.5rem;
      padding: 0 0.5rem;
      position: sticky;
      top: 0;
      z-index: 11;
      background: var(--capyctl-bg);
      border-bottom: 1px solid var(--capyctl-rule);
    }
    .phone-tabs { display: grid; grid-template-columns: repeat(4, 1fr); position: fixed; bottom: 0; left: 0; right: 0; z-index: 12; background: var(--capyctl-bg); border-top: 1px solid var(--capyctl-rule); padding: 0.375rem 0 max(0.375rem, env(safe-area-inset-bottom)); }
    .phone-tabs a, .phone-tabs button { display: flex; flex-direction: column; align-items: center; gap: 0.125rem; text-decoration: none; color: var(--capyctl-muted); font-size: 0.6875rem; border: none; padding: 0.25rem; background: transparent; }
    .phone-tabs span { font-size: 1.375rem; line-height: 1.25; }
    .phone-tabs a[aria-current='page'], .phone-tabs button[aria-expanded='true'] { color: var(--capyctl-accent-text); }
    .content {
      padding: 1.25rem 1rem 6rem;
    }
  }
</style>
