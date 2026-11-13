<script lang="ts">
  import { onMount } from 'svelte';
  import HostCard from '../../components/HostCard.svelte';
  import ModelList from '../../components/ModelList.svelte';
  import RecentActivity from '../../components/RecentActivity.svelte';
  import { livePoll } from '../../lib/poll';
  import { readHosts, readLoad, type HostRow, type LoadReport } from '../../lib/reads';
  import { href, link } from '../../lib/router';
  import { snapshot } from '../../lib/state';
  import { gib } from '../../lib/presentation';

  // SPEC R10, F3: host memory, the models with their states and actions, and
  // recent activity, live from the snapshot, the event stream and the
  // load and host reads.
  let hosts = $state<HostRow[] | null>(null);
  let load = $state<LoadReport | null>(null);
  const domains = $derived((hosts ?? []).filter((h) => !h.revoked && h.online).flatMap((h) => h.session?.domains ?? []).filter((d) => d.capacity_bytes > 0 && d.available_bytes >= 0));
  const free = $derived(domains.reduce((sum, d) => sum + d.available_bytes, 0));
  const capacity = $derived(domains.reduce((sum, d) => sum + d.capacity_bytes, 0));
  const count = (state: string) => $snapshot?.deployments.filter((d) => d.observed_state === state).length ?? 0;

  onMount(() =>
    livePoll(async () => {
      [hosts, load] = await Promise.all([readHosts(), readLoad()]);
    }, 5000),
  );
</script>

<section class="overview">
  <h1>Overview</h1>
  <p class="status-line">{`${hosts?.filter((h) => !h.revoked).length ?? '–'} host${hosts?.filter((h) => !h.revoked).length === 1 ? '' : 's'} · `}<span class="online">{hosts?.filter((h) => h.online && !h.revoked).length ?? '–'} online</span>{` · ${gib(free)} free of ${gib(capacity)} · ${count('ready')} ready · ${count('parked')} parked · ${count('stopped')} stopped`}{#if count('failed')}<span class="failed">{` · ${count('failed')} failed`}</span>{/if}{` · ${load?.deployments.reduce((sum, d) => sum + d.router.in_flight, 0) ?? '–'} in flight`}</p>

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

  <div class="activity panel"><h2>Recent activity</h2><RecentActivity limit={8} /></div>
</section>

<style>
  .status-line { color: var(--capyctl-muted); margin: -0.25rem 0 2rem; line-height: 1.9; }
  .online { color: var(--state-ready); } .failed { color: var(--capyctl-danger); }
  .activity { margin-top: 1.5rem; }
  .activity h2 { margin-top: 0; }
  .overview h2 {
    margin-top: 1.5rem;
    font-size: 1.125rem;
  }
  .hosts {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(min(100%, 24rem), 1fr));
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
