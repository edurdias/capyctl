<script lang="ts">
  import { onMount } from 'svelte';
  import ConfirmDialog from '../../components/ConfirmDialog.svelte';
  import Refusal from '../../components/Refusal.svelte';
  import { ApiError, request, signedOut } from '../../lib/api';
  import { BUILD_ASSETS } from '../../lib/endpoints';
  import { listSessions, revokeAllSessions, revokeSession, signOut, type UiSession } from '../../lib/sessions';

  // SPEC R8 (ADR 0040 §6): the active sessions, with this device marked, and
  // revoking one, all, or this one.
  let sessions = $state<UiSession[] | null>(null);
  let error = $state<ApiError | null>(null);
  let busy = $state(false);
  let confirmingAll = $state(false);
  let commit = $state<string | null>(null);
  let roleVersion = $state<string | null>(null);

  function failed(caught: unknown): void {
    error =
      caught instanceof ApiError
        ? caught
        : new ApiError({
            status: 0,
            code: null,
            message: 'CapyCTL’s answer did not arrive. Try again.',
            retryable: true,
            operationId: null,
            details: {},
            envelope: 'none',
          });
  }

  async function load(): Promise<void> {
    try {
      sessions = await listSessions();
    } catch (caught) {
      failed(caught);
    }
  }

  async function loadCommit(): Promise<void> {
    try {
      const response = await fetch(BUILD_ASSETS.commit, { cache: 'no-store' });
      if (response.ok) {
        const text = (await response.text()).trim();
        commit = /^[0-9a-f]{7,64}$/.test(text) ? text : null;
      }
    } catch {
      commit = null;
    }
  }

  async function loadRoleVersion(): Promise<void> {
    try {
      // U18: the hosts inventory exposes the role's actual running version.
      const inventory = await request<{ server_version?: string }>('/management/v1/hosts');
      roleVersion = inventory.server_version ?? null;
    } catch {
      // Session and build information remain useful when inventory is unavailable.
    }
  }

  onMount(() => {
    void load();
    void loadCommit();
    void loadRoleVersion();
  });

  async function act(run: () => Promise<void>): Promise<void> {
    busy = true;
    error = null;
    try {
      await run();
    } catch (caught) {
      failed(caught);
    } finally {
      busy = false;
    }
  }

  function revoke(session: UiSession): Promise<void> {
    return act(async () => {
      await revokeSession(session.id);
      sessions = (sessions ?? []).filter((s) => s.id !== session.id);
    });
  }

  // Revoking every session includes this one: the tab goes to sign-in.
  function revokeAll(): Promise<void> {
    confirmingAll = false;
    return act(async () => {
      await revokeAllSessions();
      signedOut();
    });
  }

  function leave(): Promise<void> {
    return act(async () => {
      await signOut();
      signedOut();
    });
  }

  function when(ms: number): string {
    return new Date(ms).toLocaleString();
  }
</script>

<section>
  <h1>Settings</h1>

  <h2>Sessions</h2>
  <p class="muted">
    Each signed-in browser has its own session, which lasts until it is revoked. Restarting CapyCTL with a different admin
    token ends every session.
  </p>
  {#if error}
    <Refusal {error} />
  {/if}
  {#if sessions === null}
    {#if !error}
      <p class="muted">Reading sessions…</p>
    {/if}
  {:else if sessions.length === 0}
    <p class="muted">No browser is signed in.</p>
  {:else}
    <ul class="sessions">
      {#each sessions as session (session.id)}
        <li data-session={session.id}>
          <div>
            <strong>{session.label || 'Unnamed browser'}</strong>
            {#if session.current}<span class="this">This device</span>{/if}
            <p class="muted">Signed in {when(session.created_at_ms)} · last seen {when(session.last_seen_at_ms)}</p>
          </div>
          {#if session.current}
            <button type="button" disabled={busy} onclick={leave}>Sign out</button>
          {:else}
            <button type="button" disabled={busy} onclick={() => revoke(session)}>Revoke</button>
          {/if}
        </li>
      {/each}
    </ul>
    <button type="button" disabled={busy} onclick={() => (confirmingAll = true)}>Revoke all sessions</button>
  {/if}

  <ConfirmDialog
    open={confirmingAll}
    title="Revoke all sessions?"
    confirmLabel="Revoke all"
    onconfirm={revokeAll}
    oncancel={() => (confirmingAll = false)}
  >
    <p>Every browser is signed out. You will be signed out here too.</p>
  </ConfirmDialog>

  <h2>About</h2>
  <p>
    {#if commit}Version (build commit) <code>{commit.slice(0, 12)}</code>.{:else}Version information is not available.{/if}
    {#if roleVersion}Running role version <code>{roleVersion}</code>.{/if}
    <a href={BUILD_ASSETS.licenses} target="_blank" rel="noopener">Third-party licenses</a>
  </p>
</section>

<style>
  .sessions {
    list-style: none;
    padding: 0;
    margin: 0 0 1rem;
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }
  .sessions li {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 1rem;
    padding: 0.75rem 1rem;
    border: 1px solid var(--capyctl-rule);
    border-radius: 6px;
    background: var(--capyctl-surface);
  }
  .sessions p {
    margin: 0.25rem 0 0;
  }
  .this {
    margin-left: 0.5rem;
    font-size: 0.875em;
    color: var(--capyctl-accent);
  }
</style>
