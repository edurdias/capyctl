<script lang="ts">
  import { onMount } from 'svelte';
  import HostCard from '../../components/HostCard.svelte';
  import ModelList from '../../components/ModelList.svelte';
  import RecentActivity from '../../components/RecentActivity.svelte';
  import { livePoll } from '../../lib/poll';
  import { readHosts, readLoad, type HostRow, type LoadReport } from '../../lib/reads';
  import { href, link } from '../../lib/router';
  import { snapshot } from '../../lib/state';

  // SPEC R10, F3: host memory, the models with their states and actions, and
  // recent activity, live from the snapshot, the event stream and the
  // load and host reads.
  let hosts = $state<HostRow[] | null>(null);
  let load = $state<LoadReport | null>(null);

  onMount(() =>
    livePoll(async () => {
      [hosts, load] = await Promise.all([readHosts(), readLoad()]);
    }, 5000),
  );
</script>

<section class="overview">
  <h1>Overview</h1>

  <h2>Hosts</h2>
  {#if hosts === null}
    <p class="muted">Reading hosts…</p>
  {:else}
    <div class="hosts">
      {#each hosts as host (host.host_id)}
        <HostCard {host} />
      {/each}
    </div>
  {/if}

  <h2>Models</h2>
  {#if $snapshot === null}
    <p class="muted">Reading models…</p>
  {:else if $snapshot.deployments.length === 0}
    <div class="empty">
      <p>No models are added yet.</p>
      <a class="button primary" href={href('/models/new')} use:link>Add model</a>
    </div>
  {:else}
    <ModelList deployments={$snapshot.deployments} {load} />
  {/if}

  <h2>Recent activity</h2>
  <RecentActivity limit={8} />
</section>

<style>
  .overview h2 {
    margin-top: 1.5rem;
    font-size: 1.125rem;
  }
  .hosts {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(16rem, 1fr));
    gap: 1rem;
  }
  .empty {
    border: 1px dashed var(--capyctl-rule);
    border-radius: 8px;
    padding: 1.5rem;
  }
  .button {
    display: inline-block;
    padding: 0.5rem 1rem;
    border-radius: 6px;
    text-decoration: none;
  }
  .button.primary {
    background: var(--capyctl-button-bg);
    color: var(--capyctl-button-text);
    font-weight: 600;
  }
</style>
