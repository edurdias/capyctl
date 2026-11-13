<script lang="ts">
  import { onMount } from 'svelte';
  import { stringify } from 'yaml';
  import ActionBar from '../../components/ActionBar.svelte';
  import Refusal from '../../components/Refusal.svelte';
  import StateBadge from '../../components/StateBadge.svelte';
  import { ApiError } from '../../lib/api';
  import { livePoll } from '../../lib/poll';
  import { duration, engineName, gib, kvPercent, reservation, sparkPoints, timing, trend } from '../../lib/presentation';
  import { readEffectiveConfig, readEngineLog, readEngines, readLatency, readLoad, type EffectiveConfig, type EngineLog, type EngineRow, type LatencySeries, type LoadDeployment } from '../../lib/reads';
  import { href, link } from '../../lib/router';
  import { snapshot } from '../../lib/state';
  let { id }: { id: string } = $props();
  const deployment = $derived($snapshot?.deployments.find((d) => d.id === id));
  const live = $derived(deployment?.observed_state === 'ready');
  const why = $derived(deployment?.observed_state === 'failed' || Boolean(deployment?.latest_operation?.error_code));
  const since = $derived($snapshot?.operations.find((o) => o.id === deployment?.latest_operation?.id)?.accepted_at);
  let busy = $state<string | null>(null);
  let engines = $state<EngineRow[]>([]);
  let load = $state<LoadDeployment | null>(null);
  let series = $state<LatencySeries[]>([]);
  let samples = $state<Record<string, (number | null)[]>>({});
  let config = $state<EffectiveConfig | null>(null);
  let configError = $state<ApiError | null>(null);
  let logInstance = $state<number | null>(null);
  let log = $state<EngineLog | null>(null);
  let logError = $state<ApiError | null>(null);
  let noEngine = $state(false);
  let reading = $state(false);
  let autoLog = false;

  function metrics() {
    const ttft = timing(series, 'ttft');
    const tpot = timing(series, 'tpot');
    const requests = timing(series, 'request');
    const readings = [
      { key: 'flight', label: 'In flight', value: load?.router.in_flight ?? null, text: String(load?.router.in_flight ?? '–'), source: 'CapyCTL router' },
      { key: 'waiting', label: 'Waiting', value: load?.router.waiting ?? null, text: String(load?.router.waiting ?? '–'), source: 'CapyCTL router' },
      { key: 'kv', label: 'KV cache', value: kvPercent(load), text: kvPercent(load) === null ? '–' : `${kvPercent(load)!.toFixed(1)}%`, source: 'Fresh engine samples · mean' },
      { key: 'ttft50', label: 'TTFT p50', value: ttft?.p50_seconds ?? null, text: duration(ttft?.p50_seconds), source: ttft ? `${ttft.source} · ${ttft.tier}` : '' },
      { key: 'ttft95', label: 'TTFT p95', value: ttft?.p95_seconds ?? null, text: duration(ttft?.p95_seconds), source: ttft ? `${ttft.source} · ${ttft.tier}` : '' },
      { key: 'tpot', label: 'TPOT p50', value: tpot?.p50_seconds ?? null, text: duration(tpot?.p50_seconds), source: tpot ? `${tpot.source} · ${tpot.tier}` : '' },
      { key: 'requests', label: 'Requests measured', value: requests?.count ?? ttft?.count ?? null, text: String(requests?.count ?? ttft?.count ?? '–'), source: requests ? `${requests.source} · ${requests.tier}` : ttft ? `${ttft.source} · ${ttft.tier}` : '' },
      { key: 'duration', label: 'Request p50', value: requests?.p50_seconds ?? null, text: duration(requests?.p50_seconds), source: requests ? `${requests.source} · ${requests.tier}` : '' },
    ];
    // Retained histograms belong to previous runs when the model is not
    // ready. Keep them in the explicitly historical disclosure, not live tiles.
    return live ? readings : readings.map((metric) => ({ ...metric, value: null, text: '–', source: 'Not ready' }));
  }
  // ADR 0040 owner decision 2026-11-13: 24 samples of the existing 5 s
  // reads, only in this browser. Percentiles stay split by their source.
  onMount(() => livePoll(async () => {
    const [loadReport, latencyReport] = await Promise.all([readLoad(id), readLatency(id)]);
    load = loadReport.deployments.find((d) => d.deployment_id === id) ?? null;
    series = latencyReport.deployments.find((d) => d.deployment_id === id)?.instances.flatMap((i) => i.series) ?? [];
    samples = Object.fromEntries(metrics().map((m) => [m.key, trend(samples[m.key] ?? [], m.value)]));
  }, 5000));
  onMount(() => {
    readEngines().then((value) => engines = value).catch(() => {});
    readEffectiveConfig(id).then((value) => config = value, (error: unknown) => { if (error instanceof ApiError) configError = error; });
  });
  $effect(() => {
    if (why && deployment && !autoLog) {
      autoLog = true;
      void readLog(deployment.instances[0]?.index ?? 0);
    }
  });
  async function readLog(index: number) {
    logInstance = index;
    reading = true;
    log = null;
    logError = null;
    noEngine = false;
    try { log = await readEngineLog(id, index); }
    catch (error) {
      if (!(error instanceof ApiError)) throw error;
      if (error.status === 404 && error.code === 'not_found') noEngine = true;
      else logError = error;
    } finally { reading = false; }
  }
</script>

{#snippet logTail()}
  {#if reading}<p class="muted">Reading engine log…</p>{:else if noEngine}<p>Instance {logInstance} has no running engine, so there is no engine log to show.</p>{:else if logError}<Refusal error={logError} />{:else if log}
    <p class="muted log-caption">Instance {log.instance} · {log.host_id} · secrets removed{log.truncated ? ' · shortened' : ''}</p>
    <!-- ADR 0040 §14 / KTD18: logs are text, never HTML. -->
    <pre class="text log">{log.text}</pre>
  {/if}
{/snippet}

{#if $snapshot === null}<p class="muted">Reading the model…</p>{:else if !deployment}
  <h1>Model not found</h1><p>This model no longer exists. <a href={href('/models')} use:link>All models</a></p>
{:else}
  <section class="detail">
    <p class="breadcrumb"><a href={href('/models')} use:link>Models</a><span aria-hidden="true"> / </span>{deployment.name}</p>
    <div class="page-title">
      <div><div class="title"><h1>{deployment.name}</h1><StateBadge state={deployment.observed_state} suspended={deployment.suspended} {since} {busy} /></div>
        <div class="facts header-facts"><span class="chip">{engineName(deployment, engines)}</span><span>{[...new Set(deployment.instances.flatMap((i) => i.host_id ? [i.host_id] : []))].join(', ') || 'Not placed'}</span>{#if deployment.context?.tokens}<span>{deployment.context.tokens.toLocaleString()} context tokens</span>{/if}<span>{gib(reservation(deployment))} reserved</span></div>
      </div>
      <div class="header-actions"><ActionBar {deployment} onbusy={(value) => busy = value} /><a href={href(`/models/${deployment.id}/edit`)} use:link>Edit</a></div>
    </div>

    {#if why}
      <section class="why-panel panel" aria-label="Why this model failed">
        <h2>Why?</h2>
        {#if deployment.latest_operation?.reason}<p class="reason">{deployment.latest_operation.reason}</p>{:else if deployment.latest_operation?.error_code}<p class="reason">{deployment.latest_operation.error_code.replaceAll('_', ' ')}</p>{/if}
        {#if deployment.latest_operation?.hint}<p>{deployment.latest_operation.hint}</p>{/if}
        {#each deployment.instances.filter((i) => i.last_error) as instance (instance.index)}<p class="reason">{instance.last_error}</p>{/each}
        {#if deployment.conditions.length}<ul>{#each deployment.conditions as condition (condition)}<li>{condition.replaceAll('_', ' ')}</li>{/each}</ul>{/if}
        {@render logTail()}
      </section>
    {/if}
    {#if deployment.context?.warning}<p class="notice">{deployment.context.warning}</p>{/if}
    {#if deployment.switch}<p class="notice">Making room · {deployment.switch.role === 'target' ? 'Starting after other models release memory' : `Releasing memory for ${deployment.switch.target}`} · {deployment.switch.phase.replaceAll('_', ' ')}</p>{/if}
    {#each deployment.model_sources ?? [] as source (`${source.host_id}:${source.source_key}`)}{#if source.state === 'downloading'}<div class="download panel"><div class="fact"><span>Downloading model · {source.host_id}</span><span>{gib(source.bytes_done)} of {gib(source.bytes_total)}</span></div><progress aria-label="Model download" value={source.bytes_done} max={source.bytes_total || 1}></progress></div>{/if}{/each}

    <div class="section-heading"><h2>{live ? 'Live readings' : 'Readings'}</h2><span class="muted">{live ? '5 s refresh · trends from this visit' : 'Not ready · live readings paused'}</span></div>
    <div class="metrics">
      {#each metrics() as metric (metric.key)}
        <article class="metric panel"><span class="eyebrow">{metric.label}</span><strong class="numeral">{metric.text}</strong><svg viewBox="0 0 100 28" preserveAspectRatio="none" aria-label={`${metric.label} trend, ${live ? (samples[metric.key] ?? []).filter((v) => v !== null).length : 0} readings`} role="img"><polyline points={live ? sparkPoints(samples[metric.key] ?? []) : ''} fill="none" stroke="currentColor" stroke-width="1.5" vector-effect="non-scaling-stroke" /></svg><small class="muted">{metric.source || 'No measurements'}</small></article>
      {/each}
    </div>
    {#if series.length}
      <details class="measurements"><summary>{live ? 'All latency readings by source' : 'Previous latency readings by source'}</summary><div class="table-scroll"><table><thead><tr><th>Measure</th><th>Source</th><th>Requests</th><th>p50</th><th>p95</th></tr></thead><tbody>{#each series as row, index (index)}<tr><td>{row.name.replaceAll('_', ' ')}</td><td>{row.source} · {row.tier}</td><td>{row.count}</td><td>{duration(row.p50_seconds)}</td><td>{duration(row.p95_seconds)}</td></tr>{/each}</tbody></table></div></details>
    {/if}
    {#if deployment.instances.length > 1}
      <h2>Instances</h2><div class="instances">{#each deployment.instances as instance (instance.index)}<article class="panel" data-instance={instance.index}><div class="facts"><strong>Instance {instance.index}</strong><span>{instance.host_id ?? 'Not placed'}</span><StateBadge state={instance.observed_state} /></div><div class="instance-actions"><ActionBar {deployment} instance={instance.index} /><button type="button" onclick={() => readLog(instance.index)}>Engine log</button></div></article>{/each}</div>
    {/if}
    {#if deployment.development_controls.residency || deployment.parked}
      <div class="parking facts"><span class="eyebrow">Parking</span><span>{deployment.development_controls.residency === 'host_backed' ? 'Keeps weights in host memory' : deployment.development_controls.residency === 'deep' ? 'Keeps weights on disk' : deployment.development_controls.residency === 'restart_only' ? 'Stops the engine; Start launches it again' : ''}</span>{#if deployment.parked}<span>{gib(deployment.parked.bytes)} per parked instance</span>{/if}</div>
    {/if}
    <details class="panel disclosure"><summary>Effective configuration</summary>{#if configError}<Refusal error={configError} />{:else if config}<pre class="text">{stringify({ effective: config.effective, hosts: config.hosts })}</pre>{:else}<p class="muted">Reading configuration…</p>{/if}</details>
    {#if !why}<details class="panel disclosure"><summary>Engine log</summary><button type="button" onclick={() => readLog(deployment.instances[0]?.index ?? 0)}>Engine log</button>{@render logTail()}</details>{/if}
  </section>
{/if}
<style>
  .breadcrumb { font-size: 0.8125rem; color: var(--capyctl-muted); margin: 0 0 1.5rem; }
  .title { display: flex; align-items: center; gap: 0.75rem; flex-wrap: wrap; }
  .breadcrumb { display: flex; align-items: center; gap: 0.375rem; }
  .title h1 { margin: 0; overflow-wrap: anywhere; }
  .header-facts { align-items: center; margin-top: 0.75rem; font-size: 0.8125rem; color: var(--capyctl-muted); }
  .header-actions { display: flex; align-items: center; gap: 1rem; }
  .why-panel { border-left: 3px solid var(--capyctl-danger); margin: 1.5rem 0; }
  .why-panel h2 { color: var(--capyctl-danger); } .reason { font-weight: 600; }
  .why-panel p { margin: 0.5rem 0; }
  .log-caption { font-size: 0.75rem; }
  .why-panel .log { max-height: 12rem; margin-bottom: 0; }
  .section-heading { display: flex; align-items: baseline; flex-wrap: wrap; justify-content: space-between; gap: 0.75rem; margin-top: 2rem; }
  .section-heading span { font-size: 0.75rem; }
  .metrics { display: grid; grid-template-columns: repeat(4, minmax(0, 1fr)); gap: 0.75rem; }
  .metric { display: flex; flex-direction: column; gap: 0.75rem; min-width: 0; }
  .metric svg { width: 100%; height: 28px; color: var(--state-starting); }
  .metric small { font-size: 0.625rem; }
  .measurements { margin: 0.75rem 0 1.5rem; }
  .measurements summary { color: var(--capyctl-muted); font-size: 0.8125rem; }
  .parking { padding: 1.5rem 0; font-size: 0.8125rem; }
  .disclosure { margin-top: 0.75rem; padding-top: 0.5rem; padding-bottom: 0.5rem; }
  .disclosure summary { font-weight: 600; }
  .notice { border-left: 2px solid var(--state-changing); padding-left: 1rem; }
  .instances { display: grid; gap: 0.75rem; } .instance-actions { display: flex; gap: 1rem; margin-top: 1rem; }
  .download progress { width: 100%; accent-color: var(--state-starting); height: 5px; }
  @media (max-width: 1000px) { .metrics { grid-template-columns: repeat(2, minmax(0, 1fr)); } }
  @media (max-width: 767.98px) { .header-actions { width: 100%; justify-content: space-between; } .header-facts { gap: 0.5rem 1rem; } .metric { padding: 1rem; } .metric .numeral { font-size: 1.75rem; } .instance-actions { flex-wrap: wrap; } }
</style>
