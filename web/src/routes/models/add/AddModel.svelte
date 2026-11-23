<script lang="ts">
  import { onMount } from 'svelte';
  import Refusal from '../../../components/Refusal.svelte';
  import { ApiError } from '../../../lib/api';
  import { createDeployment, type CheckResult } from '../../../lib/deployment-check';
  import { newIdempotencyKey } from '../../../lib/idempotency';
  import { deploymentAttempt, retryable, send, type Attempt } from '../../../lib/operate';
  import { readEngines } from '../../../lib/reads';
  import { href, link, navigate } from '../../../lib/router';
  import DeploymentDraft from './DeploymentDraft.svelte';

  // SPEC R20–R22, F2 (plan U15): add a model from the guided form or its
  // YAML, with the fit check; Add stays allowed for every verdict.

  // docs/guide/deploy.md: three fields are enough.
  let text = $state('');
  let engines = $state<string[]>([]);
  let ready = $state(false);

  onMount(() => {
    readEngines().then(
      (rows) => {
        engines = [...new Set(rows.filter((row) => !row.retiring).map((row) => row.profile))].sort();
        text = `name: \nengine: ${engines[0] ?? 'vllm'}\nmodel: {hf: }\n`;
        ready = true;
      },
      () => {
        text = 'name: \nengine: vllm\nmodel: {hf: }\n';
        ready = true;
      },
    );
  });

  let check = $state<CheckResult | null>(null);
  let busy = $state(false);
  let error = $state<ApiError | null>(null);
  // ADR 0040 (plan KTD16): a Retry resends the same request identity.
  let pending = $state<{ config: Record<string, unknown>; key: string; start: boolean } | null>(null);
  // Added, but the start was refused: the model stays added and stopped.
  let added = $state<{ id: string; revision: string } | null>(null);
  let startError = $state<ApiError | null>(null);
  let startAttempt = $state<Attempt | null>(null);

  async function add(request: { config: Record<string, unknown>; key: string; start: boolean }): Promise<void> {
    pending = request;
    busy = true;
    error = null;
    try {
      const receipt = await createDeployment(request.config, request.key);
      pending = null;
      const id = receipt.deployment_id ?? '';
      if (!request.start) {
        navigate(`/models/${id}`);
        return;
      }
      added = { id, revision: receipt.revision ?? '1' };
      await start(deploymentAttempt(added, 'start'));
    } catch (failure) {
      error = failure as ApiError;
    } finally {
      busy = false;
    }
  }

  async function start(attempt: Attempt): Promise<void> {
    startAttempt = attempt;
    startError = null;
    busy = true;
    try {
      await send(attempt);
      navigate(`/models/${added?.id ?? ''}`);
    } catch (failure) {
      startError = failure as ApiError;
    } finally {
      busy = false;
    }
  }

  const canAdd = $derived(!busy && check !== null && check.config !== null && added === null);
</script>

<section>
  <p><a href={href('/models')} use:link>Models</a></p>
  <h1>Add model</h1>
  {#if !ready}
    <p class="muted">Reading the engines…</p>
  {:else}
    <DeploymentDraft {text} {engines} ontext={(next) => (text = next)} oncheck={(result) => (check = result)} />

    <div class="actions">
      <button type="button" disabled={!canAdd} onclick={() => check?.config && add({ config: check.config, key: newIdempotencyKey(), start: false })}>Add</button>
      <button type="button" disabled={!canAdd} onclick={() => check?.config && add({ config: check.config, key: newIdempotencyKey(), start: true })}>Add and start</button>
    </div>

    {#if error}
      <Refusal {error} />
      {#if pending && retryable(error)}
        <button type="button" onclick={() => pending && add(pending)}>Retry</button>
      {/if}
    {/if}

    {#if added && startError}
      <p>The model was added and is stopped. It did not start:</p>
      <Refusal error={startError} />
      <div class="actions">
        {#if startError.code === 'capacity_blocked'}
          <button type="button" disabled={busy} onclick={() => added && start(deploymentAttempt(added, 'start', true))}>Make room and start</button>
        {/if}
        {#if startAttempt && retryable(startError)}
          <button type="button" disabled={busy} onclick={() => startAttempt && start(startAttempt)}>Retry</button>
        {/if}
        <a href={href(`/models/${added.id}`)} use:link>Open the model</a>
      </div>
    {/if}
  {/if}
</section>

<style>
  .actions {
    display: flex;
    gap: 0.75rem;
    align-items: center;
    margin: 1rem 0;
  }
</style>
