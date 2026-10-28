<script lang="ts">
  import { onMount, type Component } from 'svelte';
  import { route } from '../lib/router';
  import { startLiveState } from '../lib/state';
  import Activity from '../routes/Activity.svelte';
  import Engines from '../routes/Engines.svelte';
  import Hosts from '../routes/Hosts.svelte';
  import Models from '../routes/Models.svelte';
  import NotFound from '../routes/NotFound.svelte';
  import Overview from '../routes/Overview.svelte';
  import Playground from '../routes/Playground.svelte';
  import Settings from '../routes/Settings.svelte';
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
    <button
      type="button"
      class="menu"
      aria-controls="sidebar"
      aria-expanded={menuOpen}
      onclick={() => (menuOpen = !menuOpen)}
    >
      <span class="visually-hidden">Menu</span>
      <svg viewBox="0 0 24 24" width="24" height="24" aria-hidden="true">
        <path d="M3 6h18M3 12h18M3 18h18" stroke="currentColor" stroke-width="2" stroke-linecap="round" />
      </svg>
    </button>
    <Brand height={36} />
  </header>
  <Sidebar open={menuOpen} onnavigate={() => (menuOpen = false)} />
  <main class="content">
    <ReconnectingIndicator />
    <Screen />
  </main>
</div>

<style>
  .shell {
    min-height: 100vh;
    display: grid;
    grid-template-columns: 15rem minmax(0, 1fr);
  }
  .phone-header {
    display: none;
  }
  .content {
    padding: 1.5rem 2rem;
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
    .menu {
      display: inline-flex;
      align-items: center;
      justify-content: center;
      width: 2.75rem;
      height: 2.75rem;
      padding: 0;
      border: none;
      background: transparent;
    }
    .content {
      padding: 1rem;
    }
  }
</style>
