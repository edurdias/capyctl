<script lang="ts">
  import Brand from '../../components/Brand.svelte';
  import Refusal from '../../components/Refusal.svelte';
  import { ApiError, parseError, send } from '../../lib/api';
  import { navigate } from '../../lib/router';
  import Instructions from './Instructions.svelte';

  const SESSIONS_PATH = '/management/v1/ui/sessions';

  // ADR 0040 (plan KTD6): the link is `/ui/sign-in#t=<token>`. The fragment
  // never reaches the server; it leaves the address bar (and so the history
  // and anything the owner shares) before this page sends any request.
  const fragment = new URLSearchParams(window.location.hash.slice(1));
  let token = $state<string | null>(fragment.get('t') || null);
  if (window.location.hash !== '') {
    window.history.replaceState(window.history.state, '', window.location.pathname + window.location.search);
  }

  let sending = $state(false);
  let linkRefused = $state(false);
  let failure = $state<ApiError | null>(null);

  // Only the owner's press spends the token: a GET never does, so link
  // previewers and scanners cannot use it up.
  async function signIn() {
    if (token === null) {
      return;
    }
    sending = true;
    failure = null;
    try {
      const response = await send(SESSIONS_PATH, { method: 'POST', body: { token }, signInOn401: false });
      if (response.status === 401) {
        token = null;
        linkRefused = true;
        return;
      }
      if (!response.ok) {
        let body: unknown = undefined;
        try {
          body = await response.json();
        } catch {
          body = undefined;
        }
        failure = parseError(response.status, body);
        return;
      }
      token = null;
      navigate('/', { replace: true });
    } catch {
      failure = new ApiError({
        status: 0,
        code: null,
        message: 'CapyCTL could not be reached. Check the connection and try again.',
        retryable: true,
        operationId: null,
        details: {},
        envelope: 'none',
      });
    } finally {
      sending = false;
    }
  }
</script>

<main class="sign-in">
  <Brand height={64} />
  <div class="panel">
    {#if token !== null}
      <h1>Sign in to CapyCTL</h1>
      <p>This link signs this browser in to CapyCTL. It works once.</p>
      <button type="button" class="primary" disabled={sending} onclick={signIn}>Sign in this browser</button>
      {#if failure}
        <Refusal error={failure} />
      {/if}
    {:else}
      <Instructions {linkRefused} />
    {/if}
  </div>
</main>

<style>
  .sign-in {
    min-height: 100vh;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 1.5rem;
    padding: 2rem 1rem;
  }
  .panel {
    width: 100%;
    max-width: 34rem;
    background: var(--capyctl-surface);
    border: 1px solid var(--capyctl-rule);
    border-radius: 8px;
    padding: 1.5rem;
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
  }
  .panel :global(p) {
    margin: 0;
  }
  button {
    align-self: flex-start;
  }
</style>
