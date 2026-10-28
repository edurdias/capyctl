<script lang="ts">
  import { snapshot } from '../../lib/state';
  import Chat from './Chat.svelte';
  import Decision from './Decision.svelte';

  // SPEC R29–R32 (ADR 0040 §12): try any chat or decision model. The mode
  // follows the deployment's API.
  const deployments = $derived(
    [...($snapshot?.deployments ?? [])].sort((a, b) => a.name.localeCompare(b.name)),
  );
  let chosen = $state<string | null>(null);
  const selected = $derived(deployments.find((d) => d.id === chosen) ?? deployments[0] ?? null);
</script>

<section>
  <h1>Playground</h1>
  <p class="muted">Conversations are kept only in this browser. CapyCTL stores no prompts or answers.</p>
  {#if $snapshot === null}
    <p class="muted">Reading models…</p>
  {:else if selected === null}
    <p class="muted">There are no models to try yet.</p>
  {:else}
    <label>
      Model
      <select value={selected.id} onchange={(event) => (chosen = event.currentTarget.value)}>
        {#each deployments as deployment (deployment.id)}
          <option value={deployment.id}>
            {deployment.name}{deployment.api === 'decisions' ? ' (decisions)' : ''}
          </option>
        {/each}
      </select>
    </label>
    {#key selected.id}
      {#if selected.api === 'decisions'}
        <Decision deployment={selected} />
      {:else}
        <Chat deployment={selected} />
      {/if}
    {/key}
  {/if}
</section>

<style>
  label {
    display: flex;
    flex-direction: column;
    gap: 0.375rem;
    max-width: 24rem;
  }
  select {
    font: inherit;
    padding: 0.5rem;
    border-radius: 6px;
    border: 1px solid var(--capyctl-rule);
    background: var(--capyctl-bg);
    color: var(--capyctl-text);
  }
</style>
