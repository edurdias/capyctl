<script lang="ts">
  import { onMount } from 'svelte';
  import ModelList from '../../components/ModelList.svelte';
  import { livePoll } from '../../lib/poll';
  import { readLoad, type LoadReport } from '../../lib/reads';
  import { href, link, route } from '../../lib/router';
  import { snapshot } from '../../lib/state';
  import AddModel from './add/AddModel.svelte';
  import EditModel from './edit/EditModel.svelte';
  import ModelDetail from './ModelDetail.svelte';

  // `/models`, `/models/new`, `/models/<deployment id>` and
  // `/models/<deployment id>/edit`.
  const id = $derived($route.split('/')[2] ?? '');
  const editing = $derived($route.split('/')[3] === 'edit');

  let load = $state<LoadReport | null>(null);
  onMount(() =>
    livePoll(async () => {
      if (id === '') {
        load = await readLoad();
      }
    }, 5000),
  );
</script>

{#if id === 'new'}
  <AddModel />
{:else if id !== '' && editing}
  {#key id}
    <EditModel {id} />
  {/key}
{:else if id !== ''}
  {#key id}
    <ModelDetail {id} />
  {/key}
{:else}
  <section>
    <div class="title">
      <h1>Models</h1>
      <a class="add" href={href('/models/new')} use:link>Add model</a>
    </div>
    {#if $snapshot === null}
      <p class="muted">Reading models…</p>
    {:else if $snapshot.deployments.length === 0}
      <p>No models are added yet.</p>
    {:else}
      <ModelList deployments={$snapshot.deployments} {load} />
    {/if}
  </section>
{/if}

<style>
  .title {
    display: flex;
    justify-content: space-between;
    align-items: baseline;
    gap: 1rem;
  }
</style>
