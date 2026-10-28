<script lang="ts">
  import { href, link } from '../lib/router';
  import { hostHealth, type HostRow } from '../lib/reads';
  import MemoryBar from './MemoryBar.svelte';

  // SPEC R10: a host's health and the GPU memory and host RAM in use.
  let { host }: { host: HostRow } = $props();

  // A domain whose size the host could not read reports -1.
  const domains = $derived((host.session?.domains ?? []).filter((d) => d.capacity_bytes > 0 && d.available_bytes >= 0));
  const gpus = $derived(domains.filter((d) => d.kind === 'device'));
  const ram = $derived(domains.filter((d) => d.kind !== 'device'));
</script>

<article class="host" data-host={host.name}>
  <header>
    <a href={href(`/hosts/${encodeURIComponent(host.host_id)}`)} use:link>{host.name}</a>
    <span class="health">{hostHealth(host)}</span>
  </header>
  {#each gpus as gpu (gpu.domain_id)}
    <MemoryBar label={`GPU ${gpu.device_id ?? gpu.domain_id}`} used={gpu.capacity_bytes - gpu.available_bytes} total={gpu.capacity_bytes} />
  {/each}
  {#each ram as domain (domain.domain_id)}
    <MemoryBar label="Host RAM" used={domain.capacity_bytes - domain.available_bytes} total={domain.capacity_bytes} />
  {/each}
  {#if domains.length === 0}
    <p class="muted">No memory reading from this host right now.</p>
  {/if}
</article>

<style>
  .host {
    border: 1px solid var(--capyctl-rule);
    border-radius: 8px;
    padding: 1rem;
    background: var(--capyctl-surface);
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }
  header {
    display: flex;
    justify-content: space-between;
    gap: 0.5rem;
    font-weight: 600;
  }
  .health {
    font-weight: 400;
    color: var(--capyctl-muted);
  }
  p {
    margin: 0;
  }
</style>
