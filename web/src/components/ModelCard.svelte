<script lang="ts">
  import { onMount } from 'svelte';
  import { href, link } from '../lib/router';
  import { snapshot } from '../lib/state';
  import { livePoll } from '../lib/poll';
  import { readLatency, type EngineRow, type LatencySeries, type LoadDeployment } from '../lib/reads';
  import type { DeploymentSnapshot } from '../lib/types';
  import { duration, engineName, gib, kvPercent, reservation, timing } from '../lib/presentation';
  import ActionBar from './ActionBar.svelte';
  import StateBadge from './StateBadge.svelte';

  let { deployment, load, engines }: { deployment: DeploymentSnapshot; load: LoadDeployment | null; engines: EngineRow[] } = $props();
  let series = $state<LatencySeries[]>([]);
  let busy = $state<string | null>(null);
  // ADR 0040 owner decision 2026-11-13: retained histograms are not live
  // readings for a model that cannot currently serve requests.
  const live = $derived(deployment.observed_state === 'ready');
  const ttft = $derived(live ? timing(series, 'ttft') : undefined);
  const tpot = $derived(live ? timing(series, 'tpot') : undefined);
  const kv = $derived(live ? kvPercent(load) : null);
  const downloads = $derived(deployment.model_sources?.filter((s) => s.state === 'downloading') ?? []);
  const done = $derived(downloads.reduce((sum, s) => sum + s.bytes_done, 0));
  const total = $derived(downloads.reduce((sum, s) => sum + s.bytes_total, 0));
  const since = $derived($snapshot?.operations.find((o) => o.id === deployment.latest_operation?.id)?.accepted_at);
  onMount(() => livePoll(async () => {
    const report = await readLatency(deployment.id);
    series = report.deployments.find((d) => d.deployment_id === deployment.id)?.instances.flatMap((i) => i.series) ?? [];
  }, 5000));
</script>

<!-- ADR 0040 owner decision: dense operating rows on desktop, cards on phone. -->
<article class="model panel" data-model={deployment.name} aria-busy={busy !== null}>
  <div class="identity">
    <a class="name" href={href(`/models/${deployment.id}`)} use:link>{deployment.name}</a>
    <span class="chip">{engineName(deployment, engines)}</span>
  </div>
  <div class="state"><StateBadge state={deployment.observed_state} suspended={deployment.suspended} {since} {busy} /></div>
  <div class="reserved fact"><span class="eyebrow">Reserved</span><strong>{gib(reservation(deployment))}</strong></div>
  <div class="metric ttft"><span class="eyebrow">TTFT p50</span><strong>{duration(ttft?.p50_seconds)}</strong></div>
  <div class="metric rate"><span class="eyebrow">tok/s / stream <small>derived</small></span><strong>{tpot?.p50_seconds ? (1 / tpot.p50_seconds).toFixed(1) : '–'}</strong></div>
  <div class="support"><span class="eyebrow">Requests</span><span class="queue">{load?.router.in_flight ?? '–'} in flight · {load?.router.waiting ?? '–'} waiting</span></div>
  <div class="kv"><span class="eyebrow">KV</span><strong>{kv === null ? '–' : `${kv.toFixed(1)}%`}</strong></div>
  <div class="card-actions">
    <ActionBar {deployment} onbusy={(value) => busy = value} />
    {#if deployment.observed_state === 'failed'}<a class="why-link" href={href(`/models/${deployment.id}`)} use:link>Why?</a>{/if}
  </div>
  {#if downloads.length}
    <div class="download">
      <div class="fact"><span>Downloading model</span><span>{total > 0 ? `${Math.floor(done / total * 100)}%` : gib(done)}</span></div>
      <progress aria-label={`Download ${deployment.name}`} value={done} max={total || 1}></progress>
    </div>
  {/if}
</article>

<style>
  .model { display: grid; grid-template-columns: minmax(8rem, 1.5fr) minmax(4.5rem, 0.8fr) 4.75rem 4.5rem 5.75rem minmax(7.5rem, 1.1fr) 3.25rem minmax(8rem, auto); align-items: center; gap: 0.875rem; padding: 0.875rem 1rem; }
  .identity { min-width: 0; display: flex; flex-direction: column; align-items: flex-start; gap: 0.375rem; }
  .name { font-size: 0.875rem; font-weight: 650; text-decoration: none; color: var(--capyctl-text); overflow-wrap: anywhere; }
  .state { min-width: 0; }
  .metric, .reserved, .support, .kv { display: flex; flex-direction: column; align-items: flex-start; gap: 0.375rem; }
  .eyebrow { font-size: 0.625rem; white-space: nowrap; }
  .metric strong { font-size: 1.125rem; font-weight: 550; line-height: 1.15; letter-spacing: -0.04em; }
  .reserved strong, .kv strong { font-size: 0.8125rem; font-weight: 500; }
  .state :global(.badge) { white-space: normal; overflow-wrap: anywhere; }
  small { display: block; font-size: 0.5rem; color: var(--capyctl-muted); }
  .queue { font-size: 0.75rem; white-space: nowrap; }
  .card-actions { display: flex; align-items: center; justify-content: flex-end; flex-wrap: wrap; gap: 0.375rem; }
  .why-link { font-size: 0.75rem; }
  .download { grid-column: 1 / -1; font-size: 0.75rem; }
  progress { width: 100%; height: 5px; accent-color: var(--state-starting); display: block; margin-top: 0.5rem; }
  @media (min-width: 768px) and (max-width: 1199.98px) {
    .model { grid-template-columns: minmax(7rem, 1.5fr) 1fr 1fr 1fr; gap: 1rem; }
    .identity { grid-column: 1 / 3; } .state { grid-column: 3 / 5; justify-self: end; }
    .reserved { grid-row: 3; } .ttft { grid-column: 1; } .rate { grid-column: 2; }
    .support { grid-column: 3; } .kv { grid-column: 4; }
    .card-actions { grid-column: 3 / 5; grid-row: 3; }
  }
  @media (max-width: 767.98px) {
    .model { grid-template-columns: 1fr 1fr; gap: 1rem; padding: 1rem; }
    .identity { grid-column: 1; flex-direction: row; align-items: center; flex-wrap: wrap; gap: 0.5rem; }
    .state { grid-column: 2; grid-row: 1; justify-self: end; }
    .name { font-size: 1rem; }
    .ttft { grid-column: 1; grid-row: 2; } .rate { grid-column: 2; grid-row: 2; }
    .metric strong { font-size: 1.75rem; }
    .eyebrow { font-size: 0.6875rem; } small { display: inline; font-size: 0.5rem; }
    .support { grid-column: 1; grid-row: 3; } .kv { grid-column: 2; grid-row: 3; }
    .kv strong { font-size: 1rem; }
    .reserved { grid-column: 1; grid-row: 4; } .card-actions { grid-column: 2; grid-row: 4; }
  }
</style>
