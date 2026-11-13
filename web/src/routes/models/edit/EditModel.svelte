<script lang="ts">
  import { onMount } from 'svelte';
  import Refusal from '../../../components/Refusal.svelte';
  import { ApiError, request } from '../../../lib/api';
  import { replaceDeployment, type CheckResult } from '../../../lib/deployment-check';
  import { REDACTED } from '../../../lib/deployment-draft';
  import { newIdempotencyKey } from '../../../lib/idempotency';
  import { lostResponse, retryable } from '../../../lib/operate';
  import { href, link, navigate } from '../../../lib/router';
  import { DEPLOYMENTS } from '../../../lib/setup-endpoints';
  import DeploymentDraft from '../add/DeploymentDraft.svelte';

  // SPEC R23 (plan U15), C2: edit the accepted source, with secrets redacted,
  // and replace it against the revision it was read at.
  let { id }: { id: string } = $props();

  let text = $state('');
  let revision = $state<number | null>(null);
  let loadError = $state<ApiError | null>(null);

  async function load(): Promise<void> {
    loadError = null;
    try {
      const view = await request<{ revision: string; config: Record<string, unknown>; yaml: string }>(DEPLOYMENTS.source(id));
      text = view.yaml;
      revision = Number(view.revision);
      error = null;
    } catch (failure) {
      loadError = failure instanceof ApiError ? failure : lostResponse();
    }
  }
  onMount(() => void load());

  let check = $state<CheckResult | null>(null);
  let busy = $state(false);
  let error = $state<ApiError | null>(null);
  let pending = $state<{ config: Record<string, unknown>; key: string } | null>(null);

  // A secret the read redacted is unknown here, and saving the placeholder
  // would replace the real value with it.
  const redacted = $derived(text.includes(REDACTED));
  const canSave = $derived(!busy && revision !== null && check?.config != null && !redacted);

  async function save(attempt: { config: Record<string, unknown>; key: string }): Promise<void> {
    if (revision === null) {
      return;
    }
    pending = attempt;
    busy = true;
    error = null;
    try {
      await replaceDeployment(id, attempt.config, revision, attempt.key);
      pending = null;
      navigate(`/models/${id}`);
    } catch (failure) {
      // The draft stays as it is, whatever the refusal.
      error = failure as ApiError;
    } finally {
      busy = false;
    }
  }
</script>

<section>
  <p><a href={href(`/models/${id}`)} use:link>Back to the model</a></p>
  <h1>Edit model</h1>
  {#if loadError}
    <Refusal error={loadError} />
  {:else if revision === null}
    <p class="muted">Reading the model’s configuration…</p>
  {:else}
    <DeploymentDraft {text} ontext={(next) => (text = next)} oncheck={(result) => (check = result)} />
    {#if redacted}
      <p class="notice">Replace each {REDACTED} value with the real one before saving; CapyCTL never sends secrets to the browser.</p>
    {/if}
    <div class="actions">
      <button type="button" disabled={!canSave} onclick={() => check?.config && save({ config: check.config, key: newIdempotencyKey() })}>Save</button>
    </div>
    {#if error}
      <Refusal {error} />
      {#if error.code === 'revision_conflict'}
        <button type="button" onclick={() => void load()}>Load the current version</button>
        <p class="muted">Loading the current version replaces your draft.</p>
      {:else if pending && retryable(error)}
        <button type="button" onclick={() => pending && save(pending)}>Retry</button>
      {/if}
    {/if}
  {/if}
</section>

<style>
  .actions {
    margin: 1rem 0;
  }
  .notice {
    border-left: 3px solid var(--capyctl-accent);
    padding-left: 0.75rem;
  }
</style>
