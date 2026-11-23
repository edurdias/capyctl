<script lang="ts">
  import Refusal from '../../components/Refusal.svelte';
  import type { ApiError } from '../../lib/api';
  import { confirmCustom, previewCustom, type CustomPreview, type CustomRequest } from '../../lib/host-admin';

  // SPEC R25, AE6 (ADR 0040 §11): preview, show the exact command, type the
  // profile name, confirm with the preview's digest.
  let { host, onchange }: { host: string; onchange: () => void } = $props();

  let operation = $state<'add' | 'change'>('add');
  let name = $state('');
  let executable = $state('');
  let manifest = $state('');

  // What was previewed; the confirmation carries exactly this.
  let previewed = $state<{ request: CustomRequest; preview: CustomPreview } | null>(null);
  let typed = $state('');
  let busy = $state(false);
  let error = $state<ApiError | null>(null);
  let pinned = $state(false);

  async function preview(request: CustomRequest): Promise<void> {
    busy = true;
    typed = '';
    pinned = false;
    try {
      previewed = { request, preview: await previewCustom(host, request) };
    } catch (failure) {
      previewed = null;
      error = failure as ApiError;
    } finally {
      busy = false;
    }
  }

  async function confirm(): Promise<void> {
    if (previewed === null) {
      return;
    }
    const { request, preview: shown } = previewed;
    busy = true;
    error = null;
    try {
      await confirmCustom(host, request, shown.digest, typed);
      pinned = true;
      previewed = null;
      typed = '';
      onchange();
    } catch (failure) {
      error = failure as ApiError;
      busy = false;
      // ADR 0040 §11: the host measured something else; show the new command.
      if (error.code === 'confirmation_stale') {
        await preview(request);
      }
      return;
    }
    busy = false;
  }
</script>

<div class="custom">
  <label>
    Operation
    <select bind:value={operation}>
      <option value="add">Add a new engine</option>
      <option value="change">Change an existing custom engine</option>
    </select>
  </label>
  <label>
    Profile name
    <input name="custom-name" bind:value={name} />
  </label>
  <label>
    Executable on the host
    <input name="custom-executable" bind:value={executable} placeholder="/opt/engine/bin/serve" />
  </label>
  <label>
    Manifest
    <textarea name="custom-manifest" rows="8" bind:value={manifest}></textarea>
  </label>
  <button
    type="button"
    disabled={busy || name.trim() === '' || executable.trim() === '' || manifest.trim() === ''}
    onclick={() => {
      error = null;
      void preview({ operation, name: name.trim(), executable: executable.trim(), manifest });
    }}>Preview</button
  >

  {#if error}
    <Refusal {error} />
  {/if}

  {#if previewed}
    <h4>CapyCTL will run this command on {host}</h4>
    <pre class="command">{previewed.preview.command.join(' ')}</pre>
    <pre class="summary">{previewed.preview.summary}</pre>
    <p>This runs a program as the CapyCTL user. Type <strong>{previewed.request.name}</strong> to confirm.</p>
    <input name="custom-confirm" aria-label="Type the profile name to confirm" bind:value={typed} autocomplete="off" />
    <button type="button" disabled={busy || typed !== previewed.request.name} onclick={() => void confirm()}>Confirm</button>
  {/if}
  {#if pinned}
    <p role="status">The engine is added and its installation is pinned.</p>
  {/if}
</div>

<style>
  .custom {
    display: grid;
    gap: 0.5rem;
    max-width: 48rem;
  }
  label {
    display: grid;
    gap: 0.25rem;
  }
  pre {
    font-family: var(--capyctl-font-mono);
    font-size: 0.875rem;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    border: 1px solid var(--capyctl-rule);
    border-radius: 6px;
    padding: 0.5rem 0.75rem;
  }
</style>
