<script lang="ts">
  import { onMount } from 'svelte';
  import type { Editor } from './codemirror';

  // SPEC R22: the deployment file as text. The editor loads on first use
  // (ADR 0040 §13); until then the area reads as loading.
  let { value, onchange }: { value: string; onchange: (text: string) => void } = $props();

  let host: HTMLDivElement;
  let editor = $state<Editor | null>(null);
  let failed = $state(false);

  onMount(() => {
    let closed = false;
    let created: Editor | null = null;
    import('./codemirror').then(
      ({ createEditor }) => {
        if (!closed) {
          created = createEditor(host, value, (text) => onchange(text));
          editor = created;
        }
      },
      () => (failed = true),
    );
    return () => {
      closed = true;
      created?.destroy();
    };
  });

  // A form edit or a reload replaces the text.
  $effect(() => {
    editor?.setText(value);
  });
</script>

{#if editor === null}
  <p class="muted">{failed ? 'The editor could not load. Reload the page to try again.' : 'Loading the editor…'}</p>
{/if}
<div class="editor" bind:this={host}></div>

<style>
  .editor {
    border: 1px solid var(--capyctl-rule);
    border-radius: 6px;
    min-height: 16rem;
    font-family: var(--capyctl-font-mono);
    font-size: 0.875rem;
  }
  .editor:empty {
    display: none;
  }
</style>
