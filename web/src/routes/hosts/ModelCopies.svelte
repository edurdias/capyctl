<script lang="ts">
  import { onMount } from 'svelte';
  import Refusal from '../../components/Refusal.svelte';
  import type { ApiError } from '../../lib/api';
  import { modelCopies, pruneCopies, type ModelCopy } from '../../lib/host-admin';
  import { gib } from '../../lib/presentation';

  // SPEC R27 (ADR 0040 §10): the model copies on a host, and pruning the
  // ones no deployment uses.
  let { host }: { host: string } = $props();

  let copies = $state<ModelCopy[] | null>(null);
  let error = $state<ApiError | null>(null);
  let busy = $state(false);
  let pruned = $state<{ count: number; bytes: number } | null>(null);

  async function load(): Promise<void> {
    try {
      copies = await modelCopies(host);
    } catch (failure) {
      error = failure as ApiError;
    }
  }
  onMount(() => void load());

  async function prune(): Promise<void> {
    busy = true;
    error = null;
    try {
      const result = await pruneCopies(host);
      pruned = { count: result.removed.length, bytes: result.removed_bytes };
      await load();
    } catch (failure) {
      error = failure as ApiError;
    } finally {
      busy = false;
    }
  }

  const unused = $derived((copies ?? []).filter((copy) => !copy.referenced).length);
</script>

<h2>Model copies</h2>
{#if error}
  <Refusal {error} />
{/if}
{#if copies === null}
  {#if !error}<p class="muted">Reading the model copies…</p>{/if}
{:else if copies.length === 0}
  <p class="muted">No model has been downloaded to this host.</p>
{:else}
  <ul>
    {#each copies as copy (copy.key)}
      <li><code>{copy.key}</code> {gib(copy.bytes)}{copy.referenced ? '' : ' (unused)'}</li>
    {/each}
  </ul>
  <button type="button" disabled={busy || unused === 0} onclick={() => void prune()}>Remove unused copies</button>
{/if}
{#if pruned}
  <p role="status">Removed {pruned.count} {pruned.count === 1 ? 'copy' : 'copies'}, {gib(pruned.bytes)}.</p>
{/if}
