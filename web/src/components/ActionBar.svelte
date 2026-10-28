<script lang="ts">
  import { onDestroy } from 'svelte';
  import { ApiError } from '../lib/api';
  import {
    deploymentAttempt,
    instanceAttempt,
    retryable,
    send,
    waitForStop,
    type Attempt,
    type Receipt,
  } from '../lib/operate';
  import { reloadSnapshot } from '../lib/state';
  import type { ActionHint, DeploymentSnapshot, HintedAction } from '../lib/types';
  import { describeRefusal } from '../lib/vocabulary';
  import ConfirmDialog from './ConfirmDialog.svelte';
  import Refusal from './Refusal.svelte';

  // SPEC R16–R18 (ADR 0040 §8, plan KTD16): a model's lifecycle actions, or one
  // instance's when `instance` is set, offered as the snapshot's hints say.
  // The server's validation still decides; a refusal shows its reason.
  let { deployment, instance }: { deployment: DeploymentSnapshot; instance?: number } = $props();

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
  let error = $state<ApiError | null>(null);
  let changedElsewhere = $state(false);
  // The request a Retry resends unchanged (same key, same body), and what to
  // do once it is accepted.
  let retry = $state<{ attempt: Attempt; then: (receipt: Receipt) => Promise<void> } | null>(null);
  let offerEvict = $state(false);
  let confirming = $state(false);
  let confirmStop = $state(false);

  const waiting = new AbortController();
  onDestroy(() => waiting.abort());

  async function run(attempt: Attempt, progress: string, then: (receipt: Receipt) => Promise<void> = async () => {}) {
    busy = progress;
    error = null;
    changedElsewhere = false;
    retry = null;
    offerEvict = false;
    try {
      const receipt = await send(attempt);
      await then(receipt);
    } catch (caught) {
      if (caught instanceof DOMException && caught.name === 'AbortError') {
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

  // crates/capyctl-cli/src/client/delete.rs: stop, wait for the stop to
  // settle on the event stream, then send the ordinary delete, which the
  // server checks again.
  async function deleteAfterStop(receipt: Receipt) {
    busy = 'Waiting for the model to stop…';
    await waitForStop(deployment.id, receipt.operation_id ?? null, waiting.signal);
    await run(deploymentAttempt(deployment, 'delete'), 'Deleting…');
  }

  function confirmDelete() {
    confirming = false;
    if (!confirmStop) {
      void run(deploymentAttempt(deployment, 'delete'), 'Deleting…');
      return;
    }
    void run(deploymentAttempt(deployment, 'stop'), 'Stopping…', deleteAfterStop).then(() => {
      // SPEC §6.3: a model holding nothing has nothing to stop; delete it.
      if (error?.code === 'lifecycle_conflict') {
        void run(deploymentAttempt(deployment, 'delete'), 'Deleting…');
      }
    });
  }
</script>

{#if hints.length > 0}
  <div class="actions">
    <div class="buttons">
      {#each hints as hint (hint.action)}
        {@const enabled = hint.available || stopsFirst(hint)}
        <div class="action">
          <button
            type="button"
            class:danger={hint.action === 'delete'}
            disabled={!enabled || busy !== null}
            onclick={() => press(hint)}>{label(hint)}</button
          >
          {#if !enabled && hint.reason}
            <!-- Plan U14: the reason is visible text, not only a tooltip. -->
            <span class="why">{describeRefusal({ code: hint.reason, message: hint.reason, envelope: 'management' }).reason}</span>
          {/if}
        </div>
      {/each}
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
  .buttons {
    display: flex;
    flex-wrap: wrap;
    gap: 0.5rem;
  }
  .action {
    display: flex;
    flex-direction: column;
    gap: 0.125rem;
  }
  .why {
    font-size: 0.8125rem;
    color: var(--capyctl-muted);
    max-width: 14rem;
  }
  .danger {
    color: var(--capyctl-danger);
  }
  .busy,
  .changed {
    margin: 0.5rem 0;
  }
  .follow-up {
    display: flex;
    gap: 0.5rem;
    margin-top: 0.5rem;
  }
  /* SPEC R36: on a phone each action is a full-width button. */
  @media (max-width: 767.98px) {
    .buttons,
    .follow-up {
      flex-direction: column;
    }
    .why {
      max-width: none;
    }
    button {
      width: 100%;
    }
  }
</style>
