<script lang="ts">
  import { eventWord } from '../lib/activity';
  import { snapshot } from '../lib/state';
  import type { ManagementEvent } from '../lib/types';

  // SPEC R14, R15: events newest first, in the guide's words, with the model
  // each one is about.
  let { events }: { events: ManagementEvent[] } = $props();

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
      <time datetime={new Date(Number(event.data.recorded_at_ms)).toISOString()}>{when(event.data.recorded_at_ms)}</time>
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
    min-width: 11rem;
  }
  .model {
    font-weight: 600;
  }
</style>
