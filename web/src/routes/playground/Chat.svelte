<script lang="ts">
  import { onDestroy } from 'svelte';
  import Refusal from '../../components/Refusal.svelte';
  import { ApiError } from '../../lib/api';
  import { deleteConversation, loadConversation, saveConversation, type ChatMessage } from '../../lib/history';
  import { playgroundError, streamChat } from '../../lib/playground';
  import { snapshot } from '../../lib/state';
  import type { DeploymentSnapshot } from '../../lib/types';
  import { stateWord } from '../../lib/vocabulary';
  import StartModel from './StartModel.svelte';

  // SPEC R29, R30, R32 (ADR 0040 §12): chat with a deployment through the
  // playground route; output renders as text only (plan KTD18).
  let { deployment }: { deployment: DeploymentSnapshot } = $props();

  // The deployment as the live snapshot shows it now.
  const live = $derived($snapshot?.deployments.find((d) => d.id === deployment.id) ?? deployment);

  // svelte-ignore state_referenced_locally
  let messages = $state<ChatMessage[]>(loadConversation(deployment.id));
  let draft = $state('');
  let outstanding = $state(false);
  // The answer being streamed, before its first chunk and after.
  let streaming = $state<string | null>(null);
  let wakeAtSend = $state(false);
  let error = $state<ApiError | null>(null);
  let unsaved = $state(false);
  let offerStart = $state(false);

  const controller = new AbortController();
  onDestroy(() => controller.abort());

  // Plan U17: until the first chunk the answer is pending, reading "Waking
  // model…" while the model is coming up.
  const waking = $derived(wakeAtSend || stateWord(live.observed_state, live.suspended) === 'starting');
  const stopped = $derived(offerStart || stateWord(live.observed_state, live.suspended) === 'stopped');

  function keep(next: ChatMessage[]): void {
    messages = next;
    unsaved = !saveConversation(deployment.id, next);
  }

  async function sendMessage(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    const content = draft.trim();
    if (content.length === 0 || outstanding) {
      return;
    }
    draft = '';
    error = null;
    offerStart = false;
    const asked = [...messages, { role: 'user' as const, content }];
    keep(asked);
    outstanding = true;
    streaming = '';
    wakeAtSend = live.observed_state === 'parked';
    try {
      const result = await streamChat(deployment.name, asked, {
        signal: controller.signal,
        onContent: (text) => {
          streaming = (streaming ?? '') + text;
        },
      });
      const answer: ChatMessage = { role: 'assistant', content: result.content };
      if (result.timings) {
        answer.timings = result.timings;
      }
      keep([...asked, answer]);
    } catch (caught) {
      if (controller.signal.aborted) {
        return;
      }
      error = playgroundError(caught);
      offerStart = error.code === 'deployment_stopped';
    } finally {
      outstanding = false;
      streaming = null;
      wakeAtSend = false;
    }
  }

  function clear(): void {
    deleteConversation(deployment.id);
    messages = [];
    unsaved = false;
  }

  function seconds(ms: number): string {
    return `${(ms / 1000).toFixed(2)} s`;
  }
</script>

{#if stopped}
  <StartModel deployment={live} onstarted={() => (offerStart = false)} />
{/if}

<ol class="messages" aria-live="polite">
  {#each messages as message, index (index)}
    <li class={message.role}>
      <p class="content">{message.content}</p>
      {#if message.timings}
        <p class="timings muted">
          First token {seconds(message.timings.ttft_ms)}{#if message.timings.tokens_per_second !== null}
            · {message.timings.tokens_per_second.toFixed(1)} tokens/s{/if}
        </p>
      {/if}
    </li>
  {/each}
  {#if streaming !== null}
    <li class="assistant">
      {#if streaming.length === 0}
        <p class="pending muted" role="status">{waking ? 'Waking model…' : 'Waiting for the answer…'}</p>
      {:else}
        <p class="content">{streaming}</p>
      {/if}
    </li>
  {/if}
</ol>

{#if error}
  <Refusal {error} />
{/if}
{#if unsaved}
  <p class="muted" role="status">This browser’s storage is full, so this conversation is not being saved.</p>
{/if}

<form onsubmit={sendMessage}>
  <label class="visually-hidden" for="playground-message">Message</label>
  <textarea id="playground-message" bind:value={draft} rows="3" placeholder="Send a message"></textarea>
  <div class="buttons">
    <button type="button" disabled={outstanding || messages.length === 0} onclick={clear}>Delete conversation</button>
    <button type="submit" class="primary" disabled={outstanding}>Send</button>
  </div>
</form>

<style>
  .messages {
    list-style: none;
    padding: 0;
    margin: 1rem 0;
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
  }
  .messages li {
    padding: 0.75rem 1rem;
    border-radius: 6px;
    border: 1px solid var(--capyctl-rule);
  }
  .messages li.user {
    background: var(--capyctl-surface);
  }
  .content {
    margin: 0;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  .timings,
  .pending {
    margin: 0.375rem 0 0;
    font-size: 0.875em;
  }
  textarea {
    width: 100%;
    font: inherit;
    padding: 0.5rem;
    border-radius: 6px;
    border: 1px solid var(--capyctl-rule);
    background: var(--capyctl-bg);
    color: var(--capyctl-text);
    box-sizing: border-box;
  }
  .buttons {
    display: flex;
    justify-content: flex-end;
    gap: 0.5rem;
    margin-top: 0.5rem;
  }
</style>
