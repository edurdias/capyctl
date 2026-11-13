import type { DeploymentSnapshot, HintedAction, ObservedState } from './types';
import type { EngineRow, LatencySeries, LoadDeployment, MemoryDomain } from './reads';

// ADR 0040, owner decision 2026-11-13: display-only calculations; the role
// remains the authority for actions, reservations and readings.
export function primaryAction(state: ObservedState): HintedAction | null {
  if (state === 'ready') return 'park';
  if (['parked', 'stopped', 'failed'].includes(state)) return 'start';
  return null;
}

export function gib(bytes: number): string { return `${(bytes / 1024 ** 3).toFixed(1)} GiB`; }
export function duration(seconds: number | null | undefined): string {
  if (seconds == null) return '–';
  return seconds >= 1 ? `${seconds.toFixed(1)} s` : `${(seconds * 1000).toFixed(0)} ms`;
}
export function relative(ms: number, now = Date.now()): string {
  const seconds = Math.max(0, Math.floor((now - ms) / 1000));
  return seconds < 60 ? `${seconds}s` : seconds < 3600 ? `${Math.floor(seconds / 60)}m` : seconds < 86400 ? `${Math.floor(seconds / 3600)}h` : `${Math.floor(seconds / 86400)}d`;
}
export function engineName(d: DeploymentSnapshot, engines: EngineRow[] = []): string {
  return engines.find((e) => e.deployments.includes(d.id) || e.deployments.includes(d.name))?.engine
    ?? d.development_controls.engine ?? d.instances.find((i) => i.engine)?.engine ?? '–';
}
export function reservation(d: DeploymentSnapshot): number {
  return d.instances.reduce((sum, i) => sum + (['stopped', 'failed'].includes(i.observed_state) ? 0
    : i.observed_state === 'parked' ? (d.parked?.bytes ?? 0) : (i.startup?.bytes ?? d.startup?.bytes ?? 0)), 0);
}
export interface MemorySegment { id: string; name: string; bytes: number; parked: boolean; colour: number }
export function memorySegments(deployments: DeploymentSnapshot[], host: string, domain: MemoryDomain): MemorySegment[] {
  return deployments.flatMap((d, colour) => [false, true].flatMap((parked) => {
    let bytes = 0;
    for (const i of d.instances) {
      if (i.host_id !== host || ['stopped', 'failed'].includes(i.observed_state) || (i.observed_state === 'parked') !== parked) continue;
      const measured = parked ? d.parked?.measured?.find((m) => m.host_id === host && m.domain === domain.domain_id) : undefined;
      const startup = i.startup ?? d.startup;
      bytes += parked ? measured?.bytes ?? (domain.kind === 'device' ? 0 : d.parked?.bytes ?? 0)
        : domain.kind === 'device' ? startup?.device_bytes ?? startup?.bytes ?? 0
        : domain.kind === 'host' ? startup?.host_bytes ?? (startup?.device_bytes ? 0 : startup?.bytes) ?? 0
        : startup?.bytes ?? 0;
    }
    return bytes > 0 ? [{ id: `${d.id}:${parked ? 'parked' : 'active'}`, name: d.name, bytes, parked, colour: colour % 6 }] : [];
  }));
}
export function kvPercent(load: LoadDeployment | null | undefined): number | null {
  const samples = (load?.instances ?? []).flatMap((i) => i.sample?.fresh && i.sample.engine?.kv_usage_ppm != null ? [i.sample.engine.kv_usage_ppm] : []);
  return samples.length ? samples.reduce((a, b) => a + b, 0) / samples.length / 10000 : null;
}
export function timing(series: LatencySeries[], metric: 'ttft' | 'tpot' | 'request'): LatencySeries | undefined {
  // SPEC §17: use the role's canonical histogram names (timing.rs and
  // protocol/reports.rs). Router TTFT includes queue/activation; the engine
  // inter-token histogram supplies per-stream TPOT. Never average percentiles.
  const names = {
    ttft: ['router_time_to_first_content', 'engine_time_to_first_token'],
    tpot: ['engine_inter_token_latency'],
    request: ['router_total', 'engine_e2e_request_latency'],
  }[metric];
  for (const name of names) {
    const match = series.find((s) => s.name === name && s.source === (name.startsWith('router_') ? 'capyctl' : 'engine'));
    if (match) return match;
  }
  return undefined;
}
export function trend(samples: (number | null)[], value: number | null, capacity = 24): (number | null)[] {
  return [...samples.slice(-(capacity - 1)), value];
}
export function sparkPoints(samples: (number | null)[]): string {
  const values = samples.filter((v): v is number => v !== null);
  const min = Math.min(...values), max = Math.max(...values);
  return samples.flatMap((v, i) => v === null ? [] : [`${i * 100 / Math.max(1, samples.length - 1)},${24 - (v - min) * 20 / (max - min || 1)}`]).join(' ');
}
