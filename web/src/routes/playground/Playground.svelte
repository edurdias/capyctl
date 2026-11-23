<script lang="ts">
  import { snapshot } from '../../lib/state';
  import Chat from './Chat.svelte';
  import Decision from './Decision.svelte';
  import './playground.css';

  // SPEC R29–R32 (ADR 0040 §12): the selected deployment determines the API.
  const deployments = $derived(
    [...($snapshot?.deployments ?? [])].sort((a, b) => a.name.localeCompare(b.name)),
  );
  let chosen = $state<string | null>(null);
  const selected = $derived(deployments.find((d) => d.id === chosen) ?? deployments[0] ?? null);
</script>

{#snippet heading()}
  <h1>Playground</h1>
  {#if selected}
    <label class="playground-model">
      <span class="visually-hidden">Model</span>
      <select value={selected.id} onchange={(event) => (chosen = event.currentTarget.value)}>
        {#each deployments as deployment (deployment.id)}
          <option value={deployment.id}>{deployment.name}{deployment.api === 'decisions' ? ' (decisions)' : ''}</option>
        {/each}
      </select>
    </label>
  {/if}
{/snippet}

<section class="playground" aria-label="Playground">
  {#if $snapshot === null || selected === null}
    <header class="playground-header">{@render heading()}</header>
    <div class="playground-empty">
      <p class="muted">{$snapshot === null ? 'Reading models…' : 'There are no models to try yet.'}</p>
    </div>
  {:else}
    {#key selected.id}
      {#if selected.api === 'decisions'}
        <Decision deployment={selected} {heading} />
      {:else}
        <Chat deployment={selected} {heading} />
      {/if}
    {/key}
  {/if}
</section>
