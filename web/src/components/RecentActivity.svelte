<script lang="ts">
  import { onDestroy } from 'svelte';
  import { href, link } from '../lib/router';
  import { recentEvents, snapshot } from '../lib/state';
  import { cursorParts, readActivityPage } from '../lib/activity';
  import type { ManagementEvent } from '../lib/types';
  import EventList from './EventList.svelte';
  let { limit }: { limit: number } = $props();
  let history = $state<ManagementEvent[]>([]);
  let reading = $state(true);
  let failed = $state(false);
  let requested = false;
  const controller = new AbortController();
  onDestroy(() => controller.abort());
  // ADR 0040, owner decision 2026-11-13: use Activity's retained history on
  // first load, then merge the live feed without repeating cursors.
  async function read(cursor: string) {
    try {
      const { incarnation, sequence } = cursorParts(cursor);
      history = (await readActivityPage(incarnation, sequence, controller.signal)).events;
    } catch {
      if (!controller.signal.aborted) failed = true;
    } finally { reading = false; }
  }
  $effect(() => { if (!requested && $snapshot) { requested = true; void read($snapshot.cursor); } });
  const events = $derived([...new Map([...history, ...$recentEvents].map((event) => [event.cursor, event])).values()].sort((a, b) => cursorParts(b.cursor).sequence - cursorParts(a.cursor).sequence).slice(0, limit));
</script>
{#if events.length}<EventList {events} relativeTime />{:else if reading}<p class="muted">Reading recent activity…</p>{:else if failed}<p class="muted">Recent activity could not be read. <a href={href('/activity')} use:link>Open Activity</a></p>{:else}<p class="muted">No recent activity.</p>{/if}
<p class="all"><a href={href('/activity')} use:link>All activity →</a></p>
<style>.all { margin: 1rem 0 0; font-size: 0.8125rem; }</style>
