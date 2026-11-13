// ADR 0040 owner decision 2026-11-13 / plan U20: display calculations only.
import { describe, expect, it } from 'vitest';
import { currentSeries, duration, kvPercent, memorySegments, primaryAction, reservation, sparkPoints, timing, trend } from '../src/lib/presentation';
import { deployment, instance } from './fixtures';
import { hostHealth, type HostRow, type LatencyReport, type LatencySeries } from '../src/lib/reads';
const domain = { domain_id: 'unified', kind: 'unified', capacity_bytes: 128, available_bytes: 80 };

describe('U20 memory accounting', () => {
  it('sums per-instance startup and parked reservations, scoped to host', () => {
    const active = deployment({ startup: { bytes: 20, provenance: 'declared' }, instances: [instance(0, 'ready'), instance(1, 'ready'), { ...instance(2, 'ready'), host_id: 'other' }] });
    const parked = deployment({ id: 'parked', name: 'parked', parked: { bytes: 8, provenance: 'placeholder' }, instances: [instance(0, 'parked')] });
    const segments = memorySegments([active, parked], 'gpu-box', domain);
    expect(segments.map((s) => [s.bytes, s.parked])).toEqual([[40, false], [8, true]]);
    expect(segments.reduce((sum, s) => sum + s.bytes, 0)).toBe(48);
    expect(reservation(active)).toBe(60);
    expect(reservation(parked)).toBe(8);
  });
  it('separates active and parked instances of the same model', () => {
    const segments = memorySegments([deployment({ startup: { bytes: 20, provenance: 'declared' }, parked: { bytes: 5, provenance: 'placeholder' }, instances: [instance(0, 'ready'), instance(1, 'parked')] })], 'gpu-box', domain);
    expect(segments.map((s) => [s.bytes, s.parked])).toEqual([[20, false], [5, true]]);
    expect(new Set(segments.map((s) => s.id)).size).toBe(2);
  });
  it('uses split startup budgets on discrete domains without duplicating host bytes', () => {
    const model = deployment({ startup: { bytes: 25, provenance: 'declared', device_bytes: 20, host_bytes: 5 }, instances: [{ ...instance(0, 'ready'), devices: [{ id: 'gpu0' }, { id: 'gpu1' }] }] });
    expect(memorySegments([model], 'gpu-box', { ...domain, domain_id: 'gpu0', device_id: 'gpu0', kind: 'device' })[0].bytes).toBe(10);
    expect(memorySegments([model], 'gpu-box', { ...domain, domain_id: 'gpu1', device_id: 'gpu1', kind: 'device' })[0].bytes).toBe(10);
    expect(memorySegments([model], 'gpu-box', { ...domain, domain_id: 'gpu2', device_id: 'gpu2', kind: 'device' })).toEqual([]);
    expect(memorySegments([model], 'gpu-box', { ...domain, kind: 'host' })[0].bytes).toBe(5);
  });
  it('uses measured parked domain bytes and excludes stopped instances', () => {
    const model = deployment({ parked: { bytes: 10, provenance: 'measured', measured: [{ host_id: 'gpu-box', installation: 'engine', domain: 'gpu0', bytes: 2, measured_at_ms: 1 }] }, instances: [{ ...instance(0, 'parked'), devices: [{ id: 'gpu0' }] }, instance(1, 'stopped')] });
    expect(memorySegments([model], 'gpu-box', { ...domain, domain_id: 'gpu0', kind: 'device' })[0].bytes).toBe(2);
    expect(reservation(deployment({ startup: { bytes: 20, provenance: 'declared' }, instances: [instance(0, 'stopped')] }))).toBe(0);
  });
  // SPEC R10 / ADR 0019: an unsplit parked total cannot identify RAM residence.
  it('does not invent host RAM or GPU residence from an unmeasured parked budget', () => {
    const model = deployment({ parked: { bytes: 10, provenance: 'placeholder' }, instances: [{ ...instance(0, 'parked'), devices: ['gpu0'] }] });
    expect(memorySegments([model], 'gpu-box', { ...domain, kind: 'host' })).toEqual([]);
    expect(memorySegments([model], 'gpu-box', { ...domain, domain_id: 'gpu0', kind: 'device' })).toEqual([]);
  });
});

describe('U20 telemetry presentation', () => {
  // SPEC R10, R13: retain host health and fence historical launch histograms.
  it('keeps an online but silent host unresponsive', () => {
    expect(hostHealth({ online: true, revoked: false, session: { unresponsive: true } } as HostRow)).toBe('unresponsive');
  });
  it('selects the current instance generation, including generation-scoped engine reports', () => {
    const old: LatencySeries = { name: 'engine_inter_token_latency', source: 'engine', tier: 'engine', count: 8, p50_seconds: 9, p95_seconds: 10 };
    const current = { ...old, p50_seconds: 1 };
    const report: LatencyReport = { deployments: [{ deployment_id: 'dep-1', instances: [
      { instance: 0, generation: 1, host_id: 'gpu-box', engine: 'vllm', series: [old] },
      { instance: null, generation: 2, host_id: 'gpu-box', engine: 'vllm', series: [current] },
      { instance: 1, generation: 2, host_id: 'gpu-box', engine: 'vllm', series: [old] },
    ] }] };
    const model = deployment({ id: 'dep-1', instances: [{ ...instance(0, 'ready'), generation: '2' }] });
    expect(currentSeries(report, model)).toEqual([current]);
    expect(currentSeries(report, { ...model, instances: [{ ...instance(0, 'ready'), generation: '3' }] })).toEqual([]);
  });
  it.each([[15.2, '15.2 s'], [0.025, '25 ms'], [null, '–']])('humanises %s seconds', (input, expected) => expect(duration(input as number | null)).toBe(expected));
  it('bounds the client trend to 24 actual reads and does not turn missing values into zero', () => {
    let samples: (number | null)[] = [];
    for (let i = 0; i < 30; i++) samples = trend(samples, i);
    expect(samples).toHaveLength(24);
    expect(samples[0]).toBe(6);
    expect(trend(samples, null).at(-1)).toBeNull();
    expect(sparkPoints([null, null])).toBe('');
    expect(sparkPoints([1, 2])).toBe('0,24 100,4');
  });
  it('keeps source percentiles apart, preferring the router measurement instead of averaging', () => {
    const series: LatencySeries[] = [
      { name: 'engine_time_to_first_token', source: 'engine', tier: 'engine', count: 3, p50_seconds: 2, p95_seconds: 3 },
      { name: 'router_time_to_first_content', source: 'capyctl', tier: 'router', count: 3, p50_seconds: 4, p95_seconds: 5 },
    ];
    expect(timing(series, 'ttft')?.p50_seconds).toBe(4);
    expect(timing(series, 'tpot')).toBeUndefined();
    expect(kvPercent(null)).toBeNull();
  });
  it('has no primary command while a transition is in progress', () => {
    expect(primaryAction('starting')).toBeNull();
    expect(primaryAction('parking')).toBeNull();
  });
});
