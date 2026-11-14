<script lang="ts">
  import { onDestroy } from 'svelte';
  import EventList from '../../components/EventList.svelte';
  import Refusal from '../../components/Refusal.svelte';
  import { ACTIVITY_PAGE, cursorParts, readActivityPage } from '../../lib/activity';
  import { ApiError } from '../../lib/api';
  import { recentEvents, snapshot } from '../../lib/state';
  import type { ManagementEvent } from '../../lib/types';

  // SPEC R14: the management event history the role keeps, newest first,
  // a page at a time. Events newer than the page opened arrive live.
  let start = $state<{ incarnation: string; sequence: number } | null>(null);
  // Older pages, newest first.
  let older = $state<ManagementEvent[]>([]);
  // The next page ends at this sequence.
  let next = $state(0);
  let boundary = $state(false);
  let loading = $state(false);
  let error = $state<ApiError | null>(null);

  const controller = new AbortController();
  onDestroy(() => controller.abort());

  async function loadPage() {
    if (start === null || loading || boundary) {
      return;
    }
    loading = true;
    error = null;
    try {
      const page = await readActivityPage(start.incarnation, next, controller.signal);
      older = [...older, ...page.events.reverse()];
      boundary = page.boundary;
      next -= ACTIVITY_PAGE;
    } catch (caught) {
      if (caught instanceof ApiError) {
        error = caught;
      } else if (!(caught instanceof DOMException && caught.name === 'AbortError')) {
        throw caught;
      }
    } finally {
      loading = false;
    }
  }

  // The history up to the snapshot's cursor when the page opened.
  $effect(() => {
    if (start === null && $snapshot !== null) {
      start = cursorParts($snapshot.cursor);
      next = start.sequence;
      if (next <= 0) {
        boundary = true;
      } else {
        void loadPage();
      }
    }
  });

  const live = $derived(
    start === null
      ? []
      : $recentEvents.filter((event) => {
          const parts = cursorParts(event.cursor);
          return parts.incarnation === start!.incarnation && parts.sequence > start!.sequence;
        }),
  );
  const events = $derived([...live, ...older]);
</script>

<section>
  <h1>Activity</h1>
  {#if start === null}
    <p class="muted">Reading activity…</p>
  {:else}
    {#if events.length === 0 && !loading}
      <p class="muted">No events yet.</p>
    {/if}
    <EventList {events} />
    {#if error}
      <Refusal {error} />
    {/if}
    {#if loading}
      <p class="muted" role="status">Reading older events…</p>
    {:else if boundary}
      <!-- docs/operations/data-retention.md: management events. -->
      <p class="note">Older events are not kept. CapyCTL keeps management events for 7 days, at most 100,000 of them.</p>
    {:else}
      <button type="button" onclick={loadPage}>Show older events</button>
    {/if}
  {/if}
</section>

<style>
  .note {
    color: var(--capyctl-muted);
    margin-top: 1rem;
  }
  button {
    margin-top: 1rem;
  }
</style>
