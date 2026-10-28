<script lang="ts">
  import type { ObservedState } from '../lib/types';
  import { stateWord } from '../lib/vocabulary';

  // SPEC R10, R15: a model state in the guide's words.
  let { state, suspended = false }: { state: ObservedState; suspended?: boolean } = $props();
  const word = $derived(stateWord(state, suspended));
</script>

<span class="badge {word}">{word}</span>

<style>
  .badge {
    display: inline-flex;
    align-items: center;
    gap: 0.375rem;
    padding: 0.125rem 0.5rem;
    border: 1px solid var(--capyctl-rule);
    border-radius: 999px;
    font-size: 0.875rem;
    white-space: nowrap;
  }
  .badge::before {
    content: '';
    width: 0.5rem;
    height: 0.5rem;
    border-radius: 50%;
    background: var(--capyctl-muted);
  }
  .ready::before {
    background: var(--capyctl-accent);
  }
  .starting::before,
  .parking::before,
  .stopping::before,
  .checking::before {
    background: transparent;
    border: 2px solid var(--capyctl-accent);
  }
  .failed {
    color: var(--capyctl-danger);
    border-color: var(--capyctl-danger);
  }
  .failed::before {
    background: var(--capyctl-danger);
  }
</style>
