<script lang="ts">
  import { href, link, route } from '../lib/router';
  import { onMount } from 'svelte';
  import { snapshot } from '../lib/state';
  import { readHosts } from '../lib/reads';
  import { livePoll } from '../lib/poll';
  import Brand from './Brand.svelte';

  // ADR 0040, owner decision 2026-11-13: desktop sections, phone More sheet.
  const sections = [
    { path: '/', label: 'Overview', icon: 'M3 3h7v7H3zM14 3h7v7h-7zM3 14h7v7H3zM14 14h7v7h-7z' },
    { path: '/models', label: 'Models', icon: 'm12 3 9 5-9 5-9-5zM3 12l9 5 9-5M3 16l9 5 9-5' },
    { path: '/hosts', label: 'Hosts', icon: 'M3 4h18v6H3zM3 14h18v6H3zM7 7h1M7 17h1' },
    { path: '/engines', label: 'Engines', icon: 'M7 7h10v10H7zM9 3v4M15 3v4M9 17v4M15 17v4M3 9h4M3 15h4M17 9h4M17 15h4' },
    { path: '/playground', label: 'Playground', icon: 'm8 4 12 8-12 8z' },
    { path: '/activity', label: 'Activity', icon: 'M2 12h5l3-8 4 16 3-8h5' },
    { path: '/settings', label: 'Settings', icon: 'M4 6h16M4 12h16M4 18h16M8 3v6M16 9v6M10 15v6' },
  ];
  let online = $state<number | null>(null);
  const ready = $derived($snapshot?.deployments.filter((d) => d.observed_state === 'ready').length);
  onMount(() => livePoll(async () => { online = (await readHosts()).filter((h) => h.online && !h.revoked).length; }, 5000));

  let { open = false, onnavigate }: { open?: boolean; onnavigate?: () => void } = $props();

  const current = (path: string, at: string) => (path === '/' ? at === '/' : at === path || at.startsWith(`${path}/`));
</script>

<nav id="sidebar" class="sidebar" class:open aria-label="Sections">
  <a class="home" href={href('/')} use:link onclick={onnavigate}><Brand height={44} /></a>
  <ul>
    {#each sections as section (section.path)}
      <li>
        <a
          href={href(section.path)}
          use:link
          onclick={onnavigate}
          aria-current={current(section.path, $route) ? 'page' : undefined}
        ><svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" stroke-width="1.5" aria-hidden="true"><path d={section.icon} /></svg><span>{section.label}</span>{#if section.path === '/models' && ready !== undefined}<small aria-label={`${ready} of ${$snapshot?.deployments.length ?? 0} models ready`}>{ready}/{$snapshot?.deployments.length ?? 0} ready</small>{:else if section.path === '/hosts' && online !== null}<small aria-label={`${online} hosts online`}>{online} online</small>{/if}</a
        >
      </li>
    {/each}
  </ul>
</nav>

<style>
  /* The lockups carry the page background, so the sidebar uses it too and
     the logo sits without a visible box. */
  .sidebar {
    background: var(--capyctl-bg);
    border-right: 1px solid var(--capyctl-rule);
    padding: 1rem 0.75rem;
    display: flex;
    flex-direction: column;
    gap: 1rem;
  }
  .home {
    display: block;
    padding: 0 0.25rem;
  }
  ul {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 0.125rem;
  }
  li a {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.5rem 0.75rem;
    border-radius: 6px;
    color: var(--capyctl-text);
    text-decoration: none;
  }
  li a:hover {
    background: var(--capyctl-surface);
  }
  li a[aria-current='page'] {
    background: var(--capyctl-surface);
    color: var(--capyctl-accent-text);
    font-weight: 600;
    box-shadow: inset 3px 0 0 var(--capyctl-accent);
  }
  small { margin-left: auto; color: var(--capyctl-muted); font-size: 0.6875rem; white-space: nowrap; }
  /* ADR 0040 owner decision: More opens the remaining sections above the dock. */
  @media (max-width: 767.98px) {
    .sidebar {
      display: none;
      position: fixed;
      inset: 3.5rem 0 4.5rem 0;
      z-index: 10;
      border-right: none;
      overflow-y: auto;
    }
    .sidebar.open {
      display: flex;
    }
    .home {
      display: none;
    }
    li a {
      padding: 0.875rem 1rem;
      font-size: 1.0625rem;
    }
  }
</style>
