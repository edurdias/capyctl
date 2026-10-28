<script lang="ts">
  import { onMount } from 'svelte';
  import HostCard from '../../components/HostCard.svelte';
  import { livePoll } from '../../lib/poll';
  import { readHosts, type HostRow } from '../../lib/reads';
  import { route } from '../../lib/router';
  import HostDetail from './HostDetail.svelte';

  // `/hosts` and `/hosts/<host id>`.
  const id = $derived(decodeURIComponent($route.split('/')[2] ?? ''));

  let hosts = $state<HostRow[] | null>(null);
  onMount(() =>
    livePoll(async () => {
      if (id === '') {
        hosts = await readHosts();
      }
    }, 5000),
  );
</script>

{#if id !== ''}
  {#key id}
    <HostDetail {id} />
  {/key}
{:else}
  <section>
    <h1>Hosts</h1>
    {#if hosts === null}
      <p class="muted">Reading hosts…</p>
    {:else}
      <div class="hosts">
        {#each hosts as host (host.host_id)}
          <HostCard {host} />
        {/each}
      </div>
    {/if}
  </section>
{/if}

<style>
  .hosts {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(16rem, 1fr));
    gap: 1rem;
  }
</style>
