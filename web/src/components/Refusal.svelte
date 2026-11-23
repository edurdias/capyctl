<script lang="ts">
  import type { ApiError } from '../lib/api';
  import { describeRefusal } from '../lib/vocabulary';

  // SPEC R15: a refusal shows its reason and what the owner can do, with the
  // server's own message under Details.
  let { error }: { error: ApiError } = $props();
  const text = $derived(describeRefusal(error));
</script>

<div class="refusal" role="alert">
  <p class="reason">{text.reason}</p>
  {#if text.action}
    <p class="action">{text.action}</p>
  {/if}
  {#if text.details && text.details !== text.reason}
    <details>
      <summary>Details</summary>
      <p class="details">{text.details}</p>
    </details>
  {/if}
</div>

<style>
  .refusal {
    border: 1px solid var(--capyctl-accent);
    border-radius: 6px;
    padding: 0.75rem 1rem;
    background: var(--capyctl-surface);
  }
  p {
    margin: 0 0 0.25rem;
  }
  .reason {
    font-weight: 600;
  }
  .details {
    font-family: var(--capyctl-font-mono);
    font-size: 0.875rem;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    color: var(--capyctl-muted);
  }
</style>
