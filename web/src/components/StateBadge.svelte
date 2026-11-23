<script lang="ts">
  import { onMount } from 'svelte';
  import type { ObservedState } from '../lib/types';
  import { stateWord } from '../lib/vocabulary';
  import { relative } from '../lib/presentation';
  let { state: observed, suspended = false, since, busy = null }: { state: ObservedState; suspended?: boolean; since?: string; busy?: string | null } = $props();
  const word = $derived(stateWord(observed, suspended));
  let now = $state(Date.now());
  const transitioning = $derived(['starting', 'waking', 'parking', 'stopping', 'queued', 'uncertain', 'reconciling'].includes(observed));
  onMount(() => { const timer = setInterval(() => now = Date.now(), 1000); return () => clearInterval(timer); });
</script>

<!-- ADR 0040, owner decision 2026-11-13: semantic state, elapsed transition time. -->
<span class="badge {word}" class:pending={busy !== null}>
  <span class="dot" aria-hidden="true"></span>{busy ?? word}{#if transitioning && since && Number.isFinite(Date.parse(since))}<small>{relative(Date.parse(since), now)}</small>{/if}
</span>

<style>
  .badge { display: inline-flex; align-items: center; gap: 0.375rem; padding: 0.2rem 0.5rem; border-radius: 999px; font-size: 0.75rem; white-space: nowrap; color: var(--capyctl-muted); background: color-mix(in srgb, currentColor 9%, transparent); }
  .dot { width: 6px; height: 6px; border-radius: 50%; background: currentColor; }
  .ready { color: var(--state-ready); }
  .starting { color: var(--state-starting); }
  .stopping, .parking, .checking { color: var(--state-changing); }
  .failed { color: var(--capyctl-danger); }
  .pending { color: var(--state-starting); }
  small { font: inherit; }
</style>
