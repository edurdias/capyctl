<script lang="ts">
  import { onMount } from 'svelte';
  import { stringify } from 'yaml';
  import Refusal from '../../components/Refusal.svelte';
  import type { ApiError } from '../../lib/api';
  import { hostConfig } from '../../lib/host-admin';

  // SPEC R28 (ADR 0040 §10): the host's effective configuration as text.
  let { host }: { host: string } = $props();

  let text = $state<string | null>(null);
  let error = $state<ApiError | null>(null);

  onMount(() => {
    hostConfig(host).then(
      (config) => (text = typeof config === 'string' ? config : stringify(config)),
      (failure: ApiError) => (error = failure),
    );
  });
</script>

<h2>Host configuration</h2>
{#if error}
  <Refusal {error} />
{:else if text === null}
  <p class="muted">Reading the host’s configuration…</p>
{:else}
  <pre class="config">{text}</pre>
{/if}

<style>
  .config {
    font-family: var(--capyctl-font-mono);
    font-size: 0.875rem;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    border: 1px solid var(--capyctl-rule);
    border-radius: 6px;
    padding: 0.5rem 0.75rem;
  }
</style>
