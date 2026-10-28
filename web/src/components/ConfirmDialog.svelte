<script lang="ts">
  import type { Snippet } from 'svelte';

  // SPEC R18: deleting a model, draining a host and revoking a host each need
  // an explicit confirmation. With `typed`, the owner must type that text
  // before the action is offered.
  let {
    open,
    title,
    confirmLabel,
    typed,
    onconfirm,
    oncancel,
    children,
  }: {
    open: boolean;
    title: string;
    confirmLabel: string;
    typed?: string;
    onconfirm: () => void;
    oncancel: () => void;
    children?: Snippet;
  } = $props();

  let dialog: HTMLDialogElement | undefined = $state();
  let entered = $state('');

  $effect(() => {
    if (!dialog) {
      return;
    }
    if (open && !dialog.open) {
      entered = '';
      dialog.showModal();
    } else if (!open && dialog.open) {
      dialog.close();
    }
  });

  const allowed = $derived(typed === undefined || entered === typed);
</script>

<dialog bind:this={dialog} oncancel={(event) => { event.preventDefault(); oncancel(); }} aria-labelledby="confirm-title">
  <h2 id="confirm-title">{title}</h2>
  {#if children}
    {@render children()}
  {/if}
  {#if typed !== undefined}
    <label>
      Type <code>{typed}</code> to confirm
      <input bind:value={entered} autocomplete="off" autocapitalize="off" spellcheck="false" />
    </label>
  {/if}
  <div class="buttons">
    <button type="button" onclick={oncancel}>Cancel</button>
    <button type="button" class="primary" disabled={!allowed} onclick={onconfirm}>{confirmLabel}</button>
  </div>
</dialog>

<style>
  dialog {
    background: var(--capyctl-surface);
    color: var(--capyctl-text);
    border: 1px solid var(--capyctl-rule);
    border-radius: 8px;
    padding: 1.25rem;
    max-width: min(32rem, calc(100vw - 2rem));
  }
  dialog::backdrop {
    background: rgb(0 0 0 / 0.5);
  }
  label {
    display: flex;
    flex-direction: column;
    gap: 0.375rem;
    margin-top: 1rem;
  }
  input {
    font: inherit;
    padding: 0.5rem;
    border-radius: 6px;
    border: 1px solid var(--capyctl-rule);
    background: var(--capyctl-bg);
    color: var(--capyctl-text);
  }
  .buttons {
    display: flex;
    justify-content: flex-end;
    gap: 0.5rem;
    margin-top: 1.25rem;
  }
</style>
