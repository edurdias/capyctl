<script lang="ts">
  import Refusal from '../../components/Refusal.svelte';
  import type { ApiError } from '../../lib/api';
  import { copyText, type CopyOutcome } from '../../lib/clipboard';
  import { invite } from '../../lib/host-admin';

  // SPEC R26, F4: create an invitation and show the `capyctl join host`
  // command for the new machine.
  let hostName = $state('');
  let busy = $state(false);
  let error = $state<ApiError | null>(null);
  let command = $state<string | null>(null);
  let copied = $state<CopyOutcome | null>(null);
  let commandElement = $state<HTMLElement>();

  // `join host` reads the invitation from a private file
  // (crates/capyctl-cli/src/remote_roles.rs `private_read`), the same JSON
  // `capyctl invite host --output FILE` writes, so the command writes that
  // file owner-only, joins, and removes it.
  function joinCommand(invitation: unknown): string {
    return [
      'umask 077',
      "cat > capyctl-host.join <<'CAPYCTL_INVITATION'",
      JSON.stringify(invitation),
      'CAPYCTL_INVITATION',
      'capyctl join host --join-file capyctl-host.join && rm capyctl-host.join',
    ].join('\n');
  }

  async function create(): Promise<void> {
    busy = true;
    error = null;
    command = null;
    copied = null;
    try {
      command = joinCommand(await invite(hostName.trim()));
    } catch (failure) {
      error = failure as ApiError;
    } finally {
      busy = false;
    }
  }
</script>

<div class="invite">
  <h2>Add a host</h2>
  <label>
    New host’s name
    <input name="invite-host" bind:value={hostName} placeholder="gpu-box-2" />
  </label>
  <button type="button" disabled={busy || hostName.trim() === ''} onclick={() => void create()}>Create invitation</button>
  {#if error}
    <Refusal {error} />
  {/if}
  {#if command !== null}
    <p>Run this on the new machine within an hour. The host shows up here once it joins.</p>
    <pre class="command" bind:this={commandElement}>{command}</pre>
    <button type="button" onclick={async () => commandElement && (copied = await copyText(command ?? '', commandElement))}>Copy</button>
    {#if copied === 'copied'}
      <span role="status">Copied.</span>
    {:else if copied === 'selected'}
      <span role="status">The command is selected; copy it with your keyboard or menu.</span>
    {/if}
  {/if}
</div>

<style>
  .invite {
    display: grid;
    gap: 0.5rem;
    max-width: 48rem;
    margin-top: 2rem;
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
