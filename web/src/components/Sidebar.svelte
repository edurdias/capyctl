<script lang="ts">
  import { href, link, route } from '../lib/router';
  import Brand from './Brand.svelte';

  // SPEC R35: the classic sidebar (session-settled layout decision).
  const sections = [
    { path: '/', label: 'Overview' },
    { path: '/models', label: 'Models' },
    { path: '/hosts', label: 'Hosts' },
    { path: '/engines', label: 'Engines' },
    { path: '/playground', label: 'Playground' },
    { path: '/activity', label: 'Activity' },
    { path: '/settings', label: 'Settings' },
  ];

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
          aria-current={current(section.path, $route) ? 'page' : undefined}>{section.label}</a
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
    display: block;
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
  /* SPEC R35: below 768 px the sidebar is a menu the header's button opens. */
  @media (max-width: 767.98px) {
    .sidebar {
      display: none;
      position: fixed;
      inset: 3.5rem 0 0 0;
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
