<script lang="ts">
  import { onDestroy } from 'svelte';
  import { ApiError } from '../lib/api';
  import {
    deploymentAttempt,
    instanceAttempt,
    retryable,
    send,
    waitForStop,
    type ActionBody,
    type Attempt,
    type Receipt,
  } from '../lib/operate';
  import { reloadSnapshot } from '../lib/state';
  import type { ActionHint, DeploymentSnapshot, HintedAction } from '../lib/types';
  import { describeRefusal } from '../lib/vocabulary';
  import { primaryAction } from '../lib/presentation';
  import ConfirmDialog from './ConfirmDialog.svelte';
  import Refusal from './Refusal.svelte';

  // SPEC R16–R18 (ADR 0040 §8, plan KTD16): a model's lifecycle actions, or one
  // instance's when `instance` is set, offered as the snapshot's hints say.
  // The server's validation still decides; a refusal shows its reason.
  let { deployment, instance, onbusy }: { deployment: DeploymentSnapshot; instance?: number; onbusy?: (busy: string | null) => void } = $props();

  const LABELS: Record<HintedAction, string> = {
    start: 'Start',
    stop: 'Stop',
    park: 'Park',
    preinitialize: 'Preinitialize',
    delete: 'Delete',
  };

  const hints = $derived.by((): ActionHint[] => {
    if (instance === undefined) {
      return deployment.actions ?? [];
    }
    return deployment.instances.find((i) => i.index === instance)?.actions ?? [];
  });

  // Plan KTD16: a delete refused only because the model still runs reads
  // "Stop and delete" and stops it first.
  function stopsFirst(hint: ActionHint): boolean {
    return hint.action === 'delete' && !hint.available && hint.reason === 'delete_requires_cleanup';
  }

  function label(hint: ActionHint): string {
    return stopsFirst(hint) ? 'Stop and delete' : LABELS[hint.action];
  }

  let busy = $state<string | null>(null);
  let menu = $state<HTMLDetailsElement>();
  const primary = $derived(hints.find((h) => h.action === primaryAction(instance === undefined ? deployment.observed_state : deployment.instances.find((i) => i.index === instance)?.observed_state ?? deployment.observed_state) && h.available));
  $effect(() => { onbusy?.(busy); });
  let error = $state<ApiError | null>(null);
  let changedElsewhere = $state(false);
  // The request a Retry resends unchanged (same key, same body), and what to
  // do once it is accepted.
  let retry = $state<{ attempt: Attempt; then?: (receipt: Receipt) => Promise<void> } | null>(null);
  let offerEvict = $state(false);
  let confirming = $state(false);
  let confirmStop = $state(false);

  const waiting = new AbortController();
  onDestroy(() => waiting.abort());

  async function run(attempt: Attempt, progress: string, then?: (receipt: Receipt) => Promise<void>) {
    if (waiting.signal.aborted) return;
    busy = progress;
    error = null;
    changedElsewhere = false;
    retry = null;
    offerEvict = false;
    try {
      let receipt: Receipt;
      try {
        receipt = await send(attempt);
      } catch (caught) {
        // SPEC §6.3 / delete.rs: a model holding nothing may refuse Stop.
        // Continue through the same wait and fresh-snapshot checks, never
        // through a separate, unguarded Delete continuation.
        if (then !== undefined && (attempt.body as ActionBody).action === 'stop' && caught instanceof ApiError && caught.code === 'lifecycle_conflict') {
          receipt = {};
        } else {
          throw caught;
        }
      }
      waiting.signal.throwIfAborted();
      await then?.(receipt);
    } catch (caught) {
      if (waiting.signal.aborted || (caught instanceof DOMException && caught.name === 'AbortError')) {
        return;
      }
      const failure = caught instanceof ApiError ? caught : null;
      if (failure === null) {
        throw caught;
      }
      error = failure;
      if (failure.code === 'revision_conflict') {
        // The model changed elsewhere: show its current version.
        changedElsewhere = true;
        try {
          await reloadSnapshot();
        } catch {
          // The live stream brings it otherwise.
        }
      } else if (retryable(failure)) {
        retry = { attempt, then };
      }
      const body = attempt.body as { action?: string };
      // Plan KTD16: a start refused for room offers to make room.
      if (body.action === 'start' && (failure.code === 'capacity_blocked' || failure.code === 'startup_requires_empty_host')) {
        offerEvict = true;
      }
    } finally {
      busy = null;
    }
  }

  function attemptFor(action: HintedAction, evict = false): Attempt {
    if (instance === undefined) {
      return deploymentAttempt(deployment, action, evict);
    }
    return instanceAttempt(deployment, instance, action as 'start' | 'stop', evict);
  }

  function press(hint: ActionHint) {
    if (menu) menu.open = false;
    if (hint.action === 'delete') {
      confirmStop = stopsFirst(hint);
      confirming = true;
      return;
    }
    void run(attemptFor(hint.action), `${LABELS[hint.action]}…`);
  }

  function makeRoom() {
    void run(attemptFor('start', true), 'Making room and starting…');
  }

  // SPEC §6.3, plan KTD16 / delete.rs: stop, wait for cleanup evidence, then
  // re-read the revision for ordinary Delete. Closing the screen at any
  // asynchronous boundary leaves the stopped model intact.
  async function deleteAfterStop(receipt: Receipt, deadlineMs: number) {
    busy = 'Waiting for the model to stop…';
    await waitForStop(deployment.id, receipt.operation_id ?? null, deadlineMs, waiting.signal);
    waiting.signal.throwIfAborted();
    const current = await reloadSnapshot(waiting.signal);
    waiting.signal.throwIfAborted();
    const target = current.deployments.find((value) => value.id === deployment.id);
    if (target !== undefined) {
      await run(deploymentAttempt(target, 'delete'), 'Deleting…');
    }
  }

  function confirmDelete() {
    confirming = false;
    if (!confirmStop) {
      void run(deploymentAttempt(deployment, 'delete'), 'Deleting…');
      return;
    }
    const stop = deploymentAttempt(deployment, 'stop');
    const deadlineMs = (stop.body as ActionBody).deadline_ms;
    void run(stop, 'Stopping…', (receipt) => deleteAfterStop(receipt, deadlineMs));
  }
</script>

{#if hints.length > 0}
  <div class="actions" aria-busy={busy !== null}>
    <div class="buttons">
      {#if primary}
        <button type="button" class="primary" disabled={busy !== null} onclick={() => press(primary)}>{#if busy}<span class="spinner" aria-hidden="true"></span>{/if}{label(primary)}</button>
      {/if}
      {#if hints.some((hint) => hint !== primary)}
        <details class="overflow" bind:this={menu}>
          <summary aria-label={`More actions for ${deployment.name}${instance === undefined ? '' : ` instance ${instance}`}`}>⋯</summary>
          <div class="menu-items">
            {#each hints.filter((hint) => hint !== primary) as hint (hint.action)}
              {@const enabled = hint.available || stopsFirst(hint)}
              <div>
                <button type="button" class:danger={hint.action === 'delete'} disabled={!enabled || busy !== null} onclick={() => press(hint)}>{label(hint)}</button>
                {#if !enabled && hint.reason}
                  <span class="why">{describeRefusal({ code: hint.reason, message: hint.reason, envelope: 'management' }).reason}</span>
                {/if}
              </div>
            {/each}
          </div>
        </details>
      {/if}
    </div>
    {#if busy}
      <p class="busy" role="status">{busy}</p>
    {/if}
    {#if error}
      {#if changedElsewhere}
        <p class="changed" role="status">This model was changed elsewhere; the page now shows its current version.</p>
      {/if}
      <Refusal {error} />
      <div class="follow-up">
        {#if retry}
          {@const pending = retry}
          <button type="button" onclick={() => run(pending.attempt, 'Retrying…', pending.then)}>Retry</button>
        {/if}
        {#if offerEvict}
          <button type="button" class="primary" onclick={makeRoom}>Make room and start</button>
        {/if}
      </div>
    {/if}
  </div>
{/if}

<ConfirmDialog
  open={confirming}
  title={confirmStop ? `Stop and delete ${deployment.name}?` : `Delete ${deployment.name}?`}
  confirmLabel={confirmStop ? 'Stop and delete' : 'Delete'}
  onconfirm={confirmDelete}
  oncancel={() => (confirming = false)}
>
  <p>
    {#if confirmStop}
      CapyCTL stops the model, waits until it has stopped, then deletes it.
    {:else}
      The model and its route are removed.
    {/if}
    Apps that use it stop getting answers.
  </p>
</ConfirmDialog>

<style>
  .buttons, .follow-up { display: flex; align-items: flex-start; gap: 0.5rem; }
  .danger { color: var(--capyctl-danger); }
  .busy, .changed { margin: 0.5rem 0; font-size: 0.8125rem; }
  .follow-up { margin-top: 0.5rem; flex-wrap: wrap; }
  .spinner { display: inline-block; width: 0.75rem; height: 0.75rem; margin-right: 0.375rem; border: 2px solid currentColor; border-right-color: transparent; border-radius: 50%; animation: spin 0.8s linear infinite; }
  @keyframes spin { to { transform: rotate(360deg); } }
  @media (prefers-reduced-motion: reduce) { .spinner { animation: none; } }
</style>
