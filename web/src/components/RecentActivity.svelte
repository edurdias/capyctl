<script lang="ts">
  import { href, link } from '../lib/router';
  import { recentEvents } from '../lib/state';
  import EventList from './EventList.svelte';

  // SPEC R10: the newest events since this page opened.
  let { limit }: { limit: number } = $props();
</script>

{#if $recentEvents.length === 0}
  <p class="muted">Nothing has happened since this page opened.</p>
{:else}
  <EventList events={$recentEvents.slice(0, limit)} />
{/if}
<p><a href={href('/activity')} use:link>All activity</a></p>
