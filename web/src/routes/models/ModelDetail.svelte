<script lang="ts">
  import { onMount } from 'svelte';
  import { stringify } from 'yaml';
  import ActionBar from '../../components/ActionBar.svelte';
  import Refusal from '../../components/Refusal.svelte';
  import StateBadge from '../../components/StateBadge.svelte';
  import { ApiError } from '../../lib/api';
  import { livePoll } from '../../lib/poll';
  import {
    readEffectiveConfig,
    readEngineLog,
    readLatency,
    readLoad,
    type EffectiveConfig,
    type EngineLog,
    type LatencySeries,
    type LoadDeployment,
  } from '../../lib/reads';
  import { href, link } from '../../lib/router';
  import { snapshot } from '../../lib/state';

  // SPEC R12, R28: a model's instances and hosts, parking residency, load,
  // latency split by source, effective configuration and engine log.
  let { id }: { id: string } = $props();

  const deployment = $derived($snapshot?.deployments.find((d) => d.id === id));

  const RESIDENCY: Record<string, string> = {
    host_backed: 'Parking keeps the model in host memory.',
    deep: 'Parking keeps the model on disk.',
    restart_only: 'Parking stops the engine; waking starts it again.',
  };

  let load = $state<LoadDeployment | null>(null);
  let latency = $state<{ capyctl: LatencySeries[]; engine: LatencySeries[] } | null>(null);
  let config = $state<EffectiveConfig | null>(null);
  let configError = $state<ApiError | null>(null);

  onMount(() =>
    livePoll(async () => {
      const [loadReport, latencyReport] = await Promise.all([readLoad(id), readLatency(id)]);
      load = loadReport.deployments.find((d) => d.deployment_id === id) ?? null;
      const series = (latencyReport.deployments.find((d) => d.deployment_id === id)?.instances ?? []).flatMap((i) => i.series);
      // SPEC §17: CapyCTL's own timings and the engine's are kept apart.
      latency = {
        capyctl: series.filter((s) => s.source === 'capyctl'),
        engine: series.filter((s) => s.source === 'engine'),
      };
    }, 5000),
  );

  onMount(() => {
    readEffectiveConfig(id).then(
      (value) => (config = value),
      (error: unknown) => {
        if (error instanceof ApiError) {
          configError = error;
        }
      },
    );
  });

  // The engine log of one instance, read when asked.
  let logInstance = $state<number | null>(null);
  let log = $state<EngineLog | null>(null);
  let logError = $state<ApiError | null>(null);
  let noEngine = $state(false);
  let reading = $state(false);

  async function readLog(index: number) {
    logInstance = index;
    reading = true;
    log = null;
    logError = null;
    noEngine = false;
    try {
      log = await readEngineLog(id, index);
    } catch (error) {
      if (!(error instanceof ApiError)) {
        throw error;
      }
      // SPEC §13.3: an instance with no running launch has no log to read.
      if (error.status === 404 && error.code === 'not_found') {
        noEngine = true;
      } else {
        logError = error;
      }
    } finally {
      reading = false;
    }
  }

  function seconds(value: number | null): string {
    return value === null ? '–' : `${(value * 1000).toFixed(0)} ms`;
  }

  function engineLoad(index: number): string {
    const engine = load?.instances.find((i) => i.index === index)?.sample?.engine;
    if (!engine) {
      return 'no reading';
    }
    return `${engine.running ?? '–'} running, ${engine.waiting ?? '–'} waiting`;
  }
</script>

{#snippet seriesTable(title: string, rows: LatencySeries[])}
  <h3>{title}</h3>
  {#if rows.length === 0}
    <p class="muted">No requests measured yet.</p>
  {:else}
    <table>
      <thead><tr><th>Measure</th><th>Requests</th><th>Median</th><th>95th percentile</th></tr></thead>
      <tbody>
        {#each rows as row, i (i)}
          <tr><td>{row.name.replaceAll('_', ' ')}</td><td>{row.count}</td><td>{seconds(row.p50_seconds)}</td><td>{seconds(row.p95_seconds)}</td></tr>
        {/each}
      </tbody>
    </table>
  {/if}
{/snippet}

{#if $snapshot === null}
  <p class="muted">Reading the model…</p>
{:else if deployment === undefined}
  <section>
    <h1>Model not found</h1>
    <p>This model no longer exists. <a href={href('/models')} use:link>All models</a></p>
  </section>
{:else}
  <section class="detail">
    <p><a href={href('/models')} use:link>Models</a></p>
    <div class="title">
      <h1>{deployment.name}</h1>
      <StateBadge state={deployment.observed_state} suspended={deployment.suspended} />
    </div>
    <ActionBar {deployment} />

    <h2>Instances</h2>
    <div class="instances">
      {#each deployment.instances as instance (instance.index)}
        <div class="instance" data-instance={instance.index}>
          <div class="facts">
            <span>Instance {instance.index}</span>
            <span>Host {instance.host_id ?? 'not placed'}</span>
            <StateBadge state={instance.observed_state} />
            <span class="muted">{engineLoad(instance.index)}</span>
          </div>
          <ActionBar {deployment} instance={instance.index} />
          <button type="button" class="log-button" onclick={() => readLog(instance.index)}>Engine log</button>
        </div>
      {/each}
    </div>

    <h2>Parking</h2>
    <p>{RESIDENCY[deployment.development_controls.residency ?? ''] ?? 'Parking residency is not known yet.'}</p>
    {#if deployment.parked}
      <p class="muted">A parked instance holds {(deployment.parked.bytes / 1024 ** 3).toFixed(1)} GiB.</p>
    {/if}

    <h2>Load</h2>
    {#if load}
      <p>{load.router.in_flight} requests in flight, {load.router.waiting} waiting.</p>
    {:else}
      <p class="muted">No load reading yet.</p>
    {/if}

    <h2>Latency</h2>
    {#if latency}
      {@render seriesTable('Measured by CapyCTL', latency.capyctl)}
      {@render seriesTable('Reported by the engine', latency.engine)}
    {:else}
      <p class="muted">Reading latency…</p>
    {/if}

    <h2>Effective configuration</h2>
    {#if configError}
      <Refusal error={configError} />
    {:else if config}
      <pre class="text">{stringify({ effective: config.effective, hosts: config.hosts })}</pre>
    {:else}
      <p class="muted">Reading the configuration…</p>
    {/if}

    <h2>Engine log</h2>
    {#if logInstance === null}
      <p class="muted">Choose Engine log on an instance to read the end of its log.</p>
    {:else if reading}
      <p class="muted">Reading the log of instance {logInstance}…</p>
    {:else if noEngine}
      <p>Instance {logInstance} has no running engine, so there is no engine log to show.</p>
    {:else if logError}
      <Refusal error={logError} />
    {:else if log}
      <p class="muted">The end of instance {log.instance}'s log on {log.host_id}{log.truncated ? ', shortened' : ''}, with secrets removed.</p>
      <!-- ADR 0040 §14 (plan KTD18): logs render as text, never as HTML. -->
      <pre class="text log">{log.text}</pre>
    {/if}
  </section>
{/if}

<style>
  .title {
    display: flex;
    align-items: center;
    gap: 0.75rem;
    flex-wrap: wrap;
  }
  .detail h2 {
    margin-top: 1.5rem;
    font-size: 1.125rem;
  }
  .detail h3 {
    font-size: 1rem;
  }
  .instance {
    border: 1px solid var(--capyctl-rule);
    border-radius: 8px;
    padding: 0.75rem 1rem;
    margin-bottom: 0.75rem;
    display: flex;
    flex-direction: column;
    gap: 0.5rem;
  }
  .facts {
    display: flex;
    flex-wrap: wrap;
    gap: 0.5rem 1rem;
    align-items: center;
  }
  .log-button {
    align-self: flex-start;
  }
  table {
    border-collapse: collapse;
    width: 100%;
    font-size: 0.875rem;
  }
  th,
  td {
    text-align: left;
    padding: 0.25rem 0.5rem;
    border-bottom: 1px solid var(--capyctl-rule);
  }
  .text {
    background: var(--capyctl-code-bg);
    color: var(--capyctl-code-text);
    padding: 0.75rem 1rem;
    border-radius: 6px;
    overflow: auto;
    max-height: 32rem;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  @media (max-width: 767.98px) {
    .log-button {
      width: 100%;
    }
  }
</style>
