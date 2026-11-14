<script lang="ts">
  import { href, link } from '../lib/router';
  import { hostHealth, type HostRow } from '../lib/reads';
  import { snapshot } from '../lib/state';
  import { gib, memorySegments, relative } from '../lib/presentation';
  let { host }: { host: HostRow } = $props();
  const domains = $derived((host.session?.domains ?? []).filter((d) => d.capacity_bytes > 0 && d.available_bytes >= 0));
  const warnings = $derived(host.session?.memory_warnings ?? []);
</script>

<article class="host panel" data-host={host.host_id}>
  <header><h3><a href={href(`/hosts/${encodeURIComponent(host.host_id)}`)} use:link>{host.name}</a></h3><span class="chip" class:online={hostHealth(host) === 'online'}><span aria-hidden="true">●</span>{hostHealth(host)}</span></header>
  {#each domains as domain (domain.domain_id)}
    {@const segments = memorySegments($snapshot?.deployments ?? [], host.host_id, domain)}
    {@const used = domain.capacity_bytes - domain.available_bytes}
    {@const reserved = segments.reduce((sum, segment) => sum + segment.bytes, 0)}
    {@const scale = Math.max(domain.capacity_bytes, reserved)}
    {@const other = Math.max(0, used - reserved)}
    <div class="memory">
      <!-- SPEC R10, R15: retained observations never promise currently free memory. -->
      <div class="fact"><span class="muted">{domain.kind === 'device' ? `GPU ${domain.device_id ?? domain.domain_id}` : domain.kind === 'unified' ? 'Unified memory' : 'Host RAM'}</span><span>{gib(domain.available_bytes)} available at last reading{domain.observed_at_unix_ms != null ? ` · ${relative(domain.observed_at_unix_ms)} ago` : domain.observed_at_unix != null ? ` · ${relative(domain.observed_at_unix * 1000)} ago` : ''}</span></div>
      <div class="bar" role="meter" aria-label={domain.kind === 'device' ? `GPU ${domain.device_id ?? domain.domain_id}` : domain.kind === 'unified' ? 'Unified memory' : 'Host RAM'} aria-valuemin={0} aria-valuemax={domain.capacity_bytes} aria-valuenow={used} aria-valuetext={`${(used / 1024 ** 3).toFixed(1)} of ${(domain.capacity_bytes / 1024 ** 3).toFixed(1)} GiB`}>
        {#each segments as segment (segment.id)}<span class="segment" class:parked={segment.parked} data-model-segment={segment.name} data-bytes={segment.bytes} style:width={`${segment.bytes / scale * 100}%`} style:--segment-colour={`var(--memory-${segment.colour})`} title={`${segment.name}: ${gib(segment.bytes)}${segment.parked ? ' parked' : ''}`}></span>{/each}
        <span class="other" style:width={`${Math.min(other, Math.max(0, scale - reserved)) / scale * 100}%`}></span>
      </div>
      <div class="legend">
        {#each segments as segment (segment.id)}<span><i class:parked={segment.parked} style:--segment-colour={`var(--memory-${segment.colour})`}></i>{segment.name} <strong>{gib(segment.bytes)}</strong>{segment.parked ? ' · parked' : ''}</span>{/each}
        {#if other > 0}<span><i class="other"></i>Other use <strong>{gib(other)}</strong></span>{/if}
        <span class="muted">{gib(domain.capacity_bytes)} total</span>
      </div>
      <!-- ADR 0035 §8 / SPEC §7.2: diagnostic holders, not reclaimable memory. -->
      {#if domain.outside_processes?.length}
        <div class="outside-processes">
          <span class="muted">Other use processes</span>
          <ul>
            {#each domain.outside_processes as process (process.pid)}
              <li>{process.name ?? 'process'} · PID {process.pid} · {gib(process.bytes)}{process.other_capyctl ? ` · another CapyCTL (${process.capyctl_state_dir ?? 'unknown'})` : ''}</li>
            {/each}
            {#if domain.outside_processes_remainder}
              <li>and {domain.outside_processes_remainder.count} more · {gib(domain.outside_processes_remainder.bytes)}</li>
            {/if}
          </ul>
        </div>
      {/if}
      {#if reserved > domain.capacity_bytes}<p class="warning">Models need more memory than this {domain.kind === 'device' ? 'GPU' : 'host'} has.</p>{/if}
    </div>
  {/each}
  {#if warnings.length > 0}
    <!-- `session.memory_warnings` are full sentences, so they get their own
         wrapped lines under a count, not chips. -->
    <details class="warnings"><summary><span aria-hidden="true">▲</span>{warnings.length === 1 ? '1 memory warning' : `${warnings.length} memory warnings`}</summary>
      <ul>{#each warnings as warning (warning)}<li>{warning}</li>{/each}</ul>
    </details>
  {/if}
</article>
<style>
  header { display: flex; justify-content: space-between; align-items: center; gap: 1rem; margin-bottom: 1.25rem; }
  h3 { margin: 0; } h3 a { color: var(--capyctl-text); text-decoration: none; }
  .online { color: var(--state-ready); }
  .memory + .memory { margin-top: 1.25rem; }
  .fact { font-size: 0.8125rem; }
  .bar { display: flex; height: 10px; border-radius: 3px; overflow: hidden; background: var(--capyctl-rule); margin: 0.625rem 0; }
  .segment, i { background: var(--segment-colour); }
  .parked { background: repeating-linear-gradient(135deg, transparent 0 3px, var(--capyctl-bg) 3px 5px), var(--segment-colour); }
  .other { background: var(--capyctl-muted); opacity: 0.35; }
  .legend { display: flex; flex-wrap: wrap; gap: 0.5rem 1rem; font-size: 0.6875rem; color: var(--capyctl-muted); }
  .legend span { display: inline-flex; align-items: center; gap: 0.375rem; }
  i { display: inline-block; width: 8px; height: 8px; border-radius: 2px; }
  strong { font-weight: 500; color: var(--capyctl-text); }
  .warning { color: var(--state-changing); margin-top: 0.75rem; }
  .warnings { margin-top: 1rem; border-left: 3px solid var(--state-changing); padding: 0 0 0 0.75rem; font-size: 0.8125rem; }
  .warnings summary { color: var(--state-changing); padding: 0.25rem 0; display: flex; gap: 0.5rem; align-items: center; }
  .warnings ul { margin: 0.25rem 0 0; padding-left: 1rem; color: var(--capyctl-muted); }
  .warnings li { overflow-wrap: anywhere; margin: 0.25rem 0; }
  .outside-processes { min-width: 0; max-width: 100%; margin-top: 0.5rem; font-size: 0.75rem; }
  .outside-processes ul { margin: 0.25rem 0 0; padding-left: 1rem; }
  .outside-processes li { white-space: normal; overflow-wrap: anywhere; }
</style>
