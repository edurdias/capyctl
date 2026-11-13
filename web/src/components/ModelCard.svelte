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
  const ttft = $derived(timing(series, 'ttft'));
  const tpot = $derived(timing(series, 'tpot'));
  const kv = $derived(kvPercent(load));
  const downloads = $derived(deployment.model_sources?.filter((s) => s.state === 'downloading') ?? []);
  const done = $derived(downloads.reduce((sum, s) => sum + s.bytes_done, 0));
  const total = $derived(downloads.reduce((sum, s) => sum + s.bytes_total, 0));
  const since = $derived($snapshot?.operations.find((o) => o.id === deployment.latest_operation?.id)?.accepted_at);
  onMount(() => livePoll(async () => {
    const report = await readLatency(deployment.id);
    series = report.deployments.find((d) => d.deployment_id === deployment.id)?.instances.flatMap((i) => i.series) ?? [];
  }, 5000));
</script>

<!-- ADR 0040, owner decision 2026-11-13: the model is the operating unit. -->
<article class="model panel" data-model={deployment.name} aria-busy={busy !== null}>
  <header>
    <div class="identity"><a class="name" href={href(`/models/${deployment.id}`)} use:link>{deployment.name}</a><span class="chip">{engineName(deployment, engines)}</span></div>
    <StateBadge state={deployment.observed_state} suspended={deployment.suspended} {since} {busy} />
  </header>
  <div class="body">
    <div class="telemetry">
      <div class="metric"><span class="eyebrow">TTFT p50</span><strong>{duration(ttft?.p50_seconds)}</strong></div>
      <div class="metric"><span class="eyebrow">tok/s / stream <small>derived</small></span><strong>{tpot?.p50_seconds ? (1 / tpot.p50_seconds).toFixed(1) : '–'}</strong></div>
      <div class="support"><span class="chip">{load?.router.in_flight ?? '–'} in flight · {load?.router.waiting ?? '–'} waiting</span><span class="fact"><span class="muted">KV</span><span>{kv === null ? '–' : `${kv.toFixed(1)}%`}</span></span></div>
    </div>
    <div class="model-end"><div class="reserved fact"><span class="eyebrow">Reserved</span><strong>{gib(reservation(deployment))}</strong></div><div class="card-actions"><ActionBar {deployment} onbusy={(value) => busy = value} />{#if deployment.observed_state === 'failed'}<a class="why-link" href={href(`/models/${deployment.id}`)} use:link>Why?</a>{/if}</div></div>
  </div>
  {#if downloads.length}<div class="download"><div class="fact"><span>Downloading model</span><span>{total > 0 ? `${Math.floor(done / total * 100)}%` : gib(done)}</span></div><progress aria-label={`Download ${deployment.name}`} value={done} max={total || 1}></progress></div>{/if}
</article>
<style>
  .model { padding: 1rem 1.25rem; }
  header, .identity, .body, .model-end, .card-actions { display: flex; align-items: center; gap: 0.75rem; }
  header { justify-content: space-between; flex-wrap: wrap; }
  .identity { min-width: 0; flex-wrap: wrap; }
  .name { font-size: 1rem; font-weight: 650; text-decoration: none; color: var(--capyctl-text); overflow-wrap: anywhere; }
  .body { justify-content: space-between; margin-top: 1.25rem; }
  .telemetry { display: grid; grid-template-columns: minmax(7rem, 1fr) minmax(10rem, 1fr) minmax(12rem, 1.5fr); align-items: center; gap: 1.5rem; }
  .metric { display: flex; flex-direction: column; gap: 0.375rem; }
  .metric strong { font-size: 1.625rem; font-weight: 550; line-height: 1.15; letter-spacing: -0.04em; }
  small { font-size: 0.5625rem; color: var(--capyctl-muted); }
  .support { display: flex; align-items: flex-start; flex-direction: column; gap: 0.5rem; }
  .support .fact { width: 100%; font-size: 0.75rem; }
  .model-end { align-items: flex-end; flex-direction: column; min-width: 9rem; }
  .reserved { width: 100%; font-size: 0.8125rem; }
  .reserved strong { font-weight: 500; }
  .why-link { font-size: 0.8125rem; }
  .download { margin-top: 1rem; font-size: 0.75rem; }
  progress { width: 100%; height: 5px; accent-color: var(--state-starting); display: block; margin-top: 0.5rem; }
  @media (max-width: 1100px) { .telemetry { grid-template-columns: 1fr 1fr; } .support { grid-column: 1 / -1; flex-direction: row; } .support .fact { width: auto; } }
  @media (max-width: 767.98px) {
    .model { padding: 1rem; } .body { flex-direction: column; align-items: stretch; gap: 1rem; }
    .telemetry { gap: 1rem; } .model-end { flex-direction: row; align-items: center; justify-content: space-between; }
    .reserved { flex-direction: column; align-items: flex-start; width: auto; gap: 0.25rem; }
    .metric strong { font-size: 1.75rem; } .card-actions { gap: 0.5rem; }
  }
</style>
