<script lang="ts">
  import { eventWord } from '../lib/activity';
  import { snapshot } from '../lib/state';
  import type { ManagementEvent } from '../lib/types';
  import { relative } from '../lib/presentation';

  // SPEC R14, R15: events newest first, in the guide's words, with the model
  // each one is about.
  let { events, relativeTime = false }: { events: ManagementEvent[]; relativeTime?: boolean } = $props();

  function model(id: string | null): string {
    if (id === null) {
      return '';
    }
    return $snapshot?.deployments.find((d) => d.id === id)?.name ?? 'a deleted model';
  }

  function when(ms: string): string {
    return new Date(Number(ms)).toLocaleString();
  }
</script>

<ol class="events">
  {#each events as event (event.cursor)}
    <li>
      <time title={when(event.data.recorded_at_ms)} datetime={new Date(Number(event.data.recorded_at_ms)).toISOString()}>{relativeTime ? `${relative(Number(event.data.recorded_at_ms))} ago` : when(event.data.recorded_at_ms)}</time>
      <span class="severity" class:failure={/failed|refused|uncertain|expired|exited/.test(event.kind)} aria-label={/failed|refused|uncertain|expired|exited/.test(event.kind) ? 'warning' : 'activity'}></span>
      <span class="what">{eventWord(event.kind)}</span>
      {#if event.data.deployment_id !== null}
        <span class="model">{model(event.data.deployment_id)}</span>
      {/if}
    </li>
  {/each}
</ol>

<style>
  .events {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  li {
    display: flex;
    flex-wrap: wrap;
    gap: 0.25rem 0.75rem;
    padding: 0.5rem 0;
    border-bottom: 1px solid var(--capyctl-rule);
  }
  time {
    color: var(--capyctl-muted);
    font-size: 0.875rem;
    min-width: 3rem;
  }
  .model {
    font-weight: 600;
  }
  .severity { align-self: center; width: 6px; height: 6px; border-radius: 50%; background: var(--state-ready); }
  .severity.failure { background: var(--capyctl-danger); }
</style>
