<script lang="ts">
  import { ApiError } from '../lib/api';
  import { drainAttempt, retryable, revokeHost, send, type Attempt } from '../lib/operate';
  import ConfirmDialog from './ConfirmDialog.svelte';
  import Refusal from './Refusal.svelte';

  // SPEC R18, R19: drain a host and, on a server, revoke one. Both need an
  // explicit confirmation. A standalone role has one embedded host and no
  // enrollment, so it offers no Revoke (`canRevoke` false).
  let {
    host,
    name,
    revoked,
    canRevoke,
    onchange,
  }: { host: string; name: string; revoked: boolean; canRevoke: boolean; onchange?: () => void } = $props();

  let confirming = $state<'drain' | 'revoke' | null>(null);
  let busy = $state<string | null>(null);
  let done = $state<string | null>(null);
  let error = $state<ApiError | null>(null);
  let retry = $state<Attempt | null>(null);

  async function drain(attempt: Attempt) {
    busy = 'Draining…';
    error = null;
    retry = null;
    done = null;
    try {
      await send(attempt);
      done = `Every model on ${name} is being stopped. They start again elsewhere or here when needed.`;
      onchange?.();
    } catch (caught) {
      if (!(caught instanceof ApiError)) {
        throw caught;
      }
      error = caught;
      // Plan KTD16: a Retry reuses the drain's key and body.
      if (retryable(caught)) {
        retry = attempt;
      }
    } finally {
      busy = null;
    }
  }

  async function revoke() {
    busy = 'Revoking…';
    error = null;
    retry = null;
    done = null;
    try {
      await revokeHost(host);
      done = `${name} is revoked and can no longer connect.`;
      onchange?.();
    } catch (caught) {
      if (!(caught instanceof ApiError)) {
        throw caught;
      }
      error = caught;
    } finally {
      busy = null;
    }
  }

  function confirm() {
    const which = confirming;
    confirming = null;
    if (which === 'drain') {
      void drain(drainAttempt(host));
    } else if (which === 'revoke') {
      void revoke();
    }
  }
</script>

{#if !revoked}
  <div class="host-actions">
    <div class="buttons">
      <button type="button" disabled={busy !== null} onclick={() => (confirming = 'drain')}>Drain</button>
      {#if canRevoke}
        <button type="button" class="danger" disabled={busy !== null} onclick={() => (confirming = 'revoke')}>Revoke</button>
      {/if}
    </div>
    {#if busy}
      <p role="status">{busy}</p>
    {/if}
    {#if done}
      <p role="status">{done}</p>
    {/if}
    {#if error}
      <Refusal {error} />
      {#if retry}
        {@const pending = retry}
        <button type="button" onclick={() => drain(pending)}>Retry</button>
      {/if}
    {/if}
  </div>
{/if}

<ConfirmDialog
  open={confirming !== null}
  title={confirming === 'revoke' ? `Revoke ${name}?` : `Drain ${name}?`}
  confirmLabel={confirming === 'revoke' ? 'Revoke' : 'Drain'}
  onconfirm={confirm}
  oncancel={() => (confirming = null)}
>
  {#if confirming === 'revoke'}
    <p>{name} can no longer connect, and its models stop being placed on it. Joining it again needs a new invitation.</p>
  {:else}
    <p>Every model running on {name} is stopped. Models start again when a request or a start needs them.</p>
  {/if}
</ConfirmDialog>

<style>
  .buttons {
    display: flex;
    gap: 0.5rem;
    flex-wrap: wrap;
  }
  .danger {
    color: var(--capyctl-danger);
  }
  @media (max-width: 767.98px) {
    .buttons {
      flex-direction: column;
    }
    button {
      width: 100%;
    }
  }
</style>
