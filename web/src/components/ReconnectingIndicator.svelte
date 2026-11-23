<script lang="ts">
  import { reconnecting } from '../lib/state';

  // SPEC R11 (plan U12): after a few seconds without the event stream, say
  // so, and mark what is shown as possibly out of date.
</script>

{#if $reconnecting}
  <div class="reconnecting" role="status">
    <strong>Reconnecting…</strong>
    <span>What you see may be out of date.</span>
  </div>
{/if}

<style>
  .reconnecting {
    display: flex;
    flex-wrap: wrap;
    gap: 0.5rem;
    align-items: baseline;
    padding: 0.5rem 1rem;
    margin-bottom: 1rem;
    border: 1px solid var(--capyctl-accent);
    border-radius: 6px;
    background: var(--capyctl-surface);
  }
  strong {
    color: var(--capyctl-accent-text);
  }
</style>
