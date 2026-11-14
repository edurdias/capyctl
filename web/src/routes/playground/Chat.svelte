<script lang="ts">
  import { onDestroy, type Snippet } from 'svelte';
  import ConfirmDialog from '../../components/ConfirmDialog.svelte';
  import Refusal from '../../components/Refusal.svelte';
  import { ApiError } from '../../lib/api';
  import { deleteConversation, loadConversation, saveConversation, type ChatMessage } from '../../lib/history';
  import { playgroundError, streamChat } from '../../lib/playground';
  import { snapshot } from '../../lib/state';
  import type { DeploymentSnapshot } from '../../lib/types';
  import { stateWord } from '../../lib/vocabulary';
  import { autoGrow, followContent } from './interactions';
  import StartModel from './StartModel.svelte';

  // SPEC R29, R30, R32 (ADR 0040 §12): text-only, browser-local chat.
  let { deployment, heading }: { deployment: DeploymentSnapshot; heading: Snippet } = $props();
  const live = $derived($snapshot?.deployments.find((d) => d.id === deployment.id) ?? deployment);
  // svelte-ignore state_referenced_locally
  let messages = $state<ChatMessage[]>(loadConversation(deployment.id));
  let draft = $state('');
  let outstanding = $state(false);
  let streaming = $state<string | null>(null);
  let wakeAtSend = $state(false);
  let error = $state<ApiError | null>(null);
  let unsaved = $state(false);
  let offerStart = $state(false);
  let confirming = $state(false);
  let elapsed = $state(0);
  let sentAt = 0;
  let timer: ReturnType<typeof setInterval> | undefined;
  const controller = new AbortController();
  onDestroy(() => { controller.abort(); clearInterval(timer); });

  const modelState = $derived(stateWord(live.observed_state, live.suspended));
  const waking = $derived((wakeAtSend && modelState !== 'ready') || modelState === 'starting');
  const stopped = $derived(offerStart || modelState === 'stopped');

  function keep(next: ChatMessage[]): void {
    messages = next;
    unsaved = !saveConversation(deployment.id, next);
  }

  async function sendMessage(event: SubmitEvent): Promise<void> {
    event.preventDefault();
    const content = draft.trim();
    if (content.length === 0 || outstanding) return;
    draft = '';
    error = null;
    offerStart = false;
    const asked = [...messages, { role: 'user' as const, content }];
    keep(asked);
    outstanding = true;
    streaming = '';
    wakeAtSend = live.observed_state === 'parked';
    sentAt = Date.now();
    elapsed = 0;
    timer = setInterval(() => { elapsed = Math.floor((Date.now() - sentAt) / 1000); }, 1000);
    try {
      const result = await streamChat(deployment.name, asked, {
        signal: controller.signal,
        onContent: (text) => { streaming = (streaming ?? '') + text; },
      });
      const answer: ChatMessage = { role: 'assistant', content: result.content };
      if (result.timings) answer.timings = result.timings;
      keep([...asked, answer]);
    } catch (caught) {
      if (controller.signal.aborted) return;
      error = playgroundError(caught);
      offerStart = error.code === 'deployment_stopped';
    } finally {
      clearInterval(timer);
      outstanding = false;
      streaming = null;
      wakeAtSend = false;
    }
  }

  // SPEC R30, R35–R36: Enter sends on desktop, not on a touch keyboard.
  function keydown(event: KeyboardEvent): void {
    if (event.key === 'Enter' && !event.shiftKey && !event.isComposing && !window.matchMedia?.('(pointer: coarse)').matches) {
      event.preventDefault();
      if (event.currentTarget instanceof HTMLTextAreaElement) event.currentTarget.form?.requestSubmit();
    }
  }

  function clear(): void {
    deleteConversation(deployment.id);
    messages = [];
    unsaved = false;
    confirming = false;
  }

  function seconds(ms: number): string {
    return `${(ms / 1000).toFixed(2)} s`;
  }
</script>

<header class="playground-header">
  {@render heading()}
  <button class="conversation-delete" type="button" disabled={outstanding || messages.length === 0} onclick={() => (confirming = true)}>Delete conversation</button>
</header>
<div class="playground-scroll" use:followContent={[messages, streaming, error]}>
  {#if messages.length === 0 && streaming === null}
    <div class="playground-empty">
      <h2>Try a conversation</h2>
      <p class="muted">Conversations are kept only in this browser. CapyCTL stores no prompts or answers.</p>
    </div>
  {/if}
  {#if stopped}
    <StartModel deployment={live} onstarted={() => (offerStart = false)} />
  {/if}
  <ol class="messages" aria-live="polite">
    {#each messages as message, index (index)}
      <li class={message.role}>
        <span class="visually-hidden">{message.role === 'user' ? 'You' : deployment.name}: </span>
        <p class="content">{message.role === 'assistant' ? message.content.trim() : message.content}</p>
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
        {#if streaming.trim().length === 0}
          <p class="pending muted" role="status">{waking ? `Waking ${deployment.name}` : 'Waiting for the answer'} · {live.observed_state} · {elapsed} s</p>
        {:else}
          <p class="content">{streaming.trim()}</p>
        {/if}
      </li>
    {/if}
  </ol>
  {#if error}<Refusal {error} />{/if}
  {#if unsaved}
    <p class="muted" role="status">This browser’s storage is full, so this conversation is not being saved.</p>
  {/if}
</div>
<form class="playground-composer" onsubmit={sendMessage}>
  <label class="visually-hidden" for="playground-message">Message</label>
  <textarea id="playground-message" bind:value={draft} use:autoGrow={draft} onkeydown={keydown} rows="1" placeholder="Send a message"></textarea>
  <button type="submit" class="primary" disabled={outstanding}>Send</button>
</form>
<ConfirmDialog open={confirming} title="Delete conversation?" confirmLabel="Delete" onconfirm={clear} oncancel={() => (confirming = false)}>
  <p>This removes this model’s conversation from this browser. It cannot be undone.</p>
</ConfirmDialog>
