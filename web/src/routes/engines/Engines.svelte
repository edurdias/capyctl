<script lang="ts">
  import { onMount } from 'svelte';
  import { livePoll } from '../../lib/poll';
  import { readEngines, readHosts, type EngineRow, type HostRow } from '../../lib/reads';
  import HostEngines from './HostEngines.svelte';

  // SPEC R24, R25 (plan U16): each host's engines, detection, built-in adds
  // and removes, and the custom-engine flow.
  let hosts = $state<HostRow[] | null>(null);
  let engines = $state<EngineRow[]>([]);

  const refresh = async () => {
    const [hostRows, engineRows] = await Promise.all([readHosts(), readEngines()]);
    hosts = hostRows.filter((host) => !host.revoked);
    engines = engineRows;
  };
  onMount(() => livePoll(refresh, 5000));
</script>

<section>
  <h1>Engines</h1>
  {#if hosts === null}
    <p class="muted">Reading hosts…</p>
  {:else if hosts.length === 0}
    <p class="muted">No host has joined yet.</p>
  {:else}
    {#each hosts as host (host.host_id)}
      <HostEngines {host} engines={engines.filter((row) => row.host_id === host.host_id)} onchange={() => void refresh()} />
    {/each}
  {/if}
</section>
