<script lang="ts">
  import { onMount } from 'svelte';
  import HostActions from '../../components/HostActions.svelte';
  import HostCard from '../../components/HostCard.svelte';
  import StateBadge from '../../components/StateBadge.svelte';
  import HostConfig from './HostConfig.svelte';
  import ModelCopies from './ModelCopies.svelte';
  import { livePoll } from '../../lib/poll';
  import { readEngines, readHosts, readRoleKind, type EngineRow, type HostRow, type RoleKind } from '../../lib/reads';
  import { href, link } from '../../lib/router';
  import { snapshot } from '../../lib/state';

  // SPEC R13, R19: a host's GPUs and memory, installed engines, the models
  // placed on it, and Drain and Revoke.
  let { id }: { id: string } = $props();

  let host = $state<HostRow | null | undefined>(undefined);
  let engines = $state<EngineRow[]>([]);
  let role = $state<RoleKind | null>(null);

  const refresh = async () => {
    const [hosts, rows] = await Promise.all([readHosts(), readEngines()]);
    host = hosts.find((h) => h.host_id === id) ?? null;
    engines = rows.filter((row) => row.host_id === id);
  };

  onMount(() => livePoll(refresh, 5000));
  onMount(() => {
    readRoleKind().then(
      (kind) => (role = kind),
      () => {},
    );
  });

  const placed = $derived(
    ($snapshot?.deployments ?? []).flatMap((deployment) =>
      deployment.instances
        .filter((instance) => instance.host_id === id)
        .map((instance) => ({ deployment, instance })),
    ),
  );
</script>

{#if host === undefined}
  <p class="muted">Reading the host…</p>
{:else if host === null}
  <section>
    <h1>Host not found</h1>
    <p>CapyCTL does not know this host. <a href={href('/hosts')} use:link>All hosts</a></p>
  </section>
{:else}
  <section class="detail">
    <p><a href={href('/hosts')} use:link>Hosts</a></p>
    <h1>{host.name}</h1>
    {#if role !== null}
      <!-- A standalone role's one host is not enrolled: no Revoke. -->
      <HostActions host={host.host_id} name={host.name} revoked={host.revoked} canRevoke={role === 'server'} onchange={() => void refresh()} />
    {/if}

    <h2>GPUs and memory</h2>
    <HostCard {host} />

    <h2>Engines</h2>
    {#if engines.length === 0}
      <p class="muted">No engines are installed on this host.</p>
    {:else}
      <ul>
        {#each engines as engine (engine.profile)}
          <li>{engine.profile}: {engine.engine} {engine.version}{engine.custom ? ' (custom)' : ''}</li>
        {/each}
      </ul>
    {/if}

    <h2>Models on this host</h2>
    {#if placed.length === 0}
      <p class="muted">No model is placed on this host.</p>
    {:else}
      <ul>
        {#each placed as { deployment, instance } (`${deployment.id}:${instance.index}`)}
          <li>
            <a href={href(`/models/${deployment.id}`)} use:link>{deployment.name}</a>
            instance {instance.index}
            <StateBadge state={instance.observed_state} />
          </li>
        {/each}
      </ul>
    {/if}

    <ModelCopies host={host.host_id} />
    <HostConfig host={host.host_id} />
  </section>
{/if}

<style>
  .detail h2 {
    margin-top: 1.5rem;
    font-size: 1.125rem;
  }
  li {
    margin: 0.375rem 0;
  }
</style>
