// ADR 0040 owner decision 2026-11-13 / plan U20, R10, R12, R16, R35–R36.
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { button, deployment, DEPLOYMENT_ID, instance, open, role, setUp, snapshotWith, STANDALONE_HOST, tearDown, tick } from './fixtures';
import { cursor, json, StreamMock } from './helpers';
import type { ObservedState } from '../src/lib/types';

beforeEach(setUp);
afterEach(tearDown);
const GiB = 1024 ** 3;

describe('U20 model action grammar', () => {
  it.each<[ObservedState, string]>([['ready', 'Park'], ['parked', 'Start'], ['stopped', 'Start'], ['failed', 'Start']])('offers only %s’s primary action outside its menu', async (state, label) => {
    role({ snapshots: [snapshotWith([deployment({ observed_state: state, actions: [
      { action: 'start', available: true }, { action: 'park', available: true }, { action: 'stop', available: true }, { action: 'preinitialize', available: true }, { action: 'delete', available: true },
    ] })])] });
    await open('/');
    const card = document.querySelector('[data-model="chat"]')!;
    const outsideMenu = [...card.querySelectorAll('button')].filter((b) => !b.closest('details, dialog'));
    expect(outsideMenu.map((b) => b.textContent?.trim())).toEqual([label]);
    expect(card.querySelector('details')?.open).toBe(false);
    if (state === 'failed') expect(card.querySelector('.why-link')?.textContent).toBe('Why?');
  });

  it('puts the busy pill and spinner on the model card while the command is pending', async () => {
    role({ snapshots: [snapshotWith([deployment({ actions: [{ action: 'park', available: true }] })])] });
    await open('/');
    button('Park').click();
    await tick();
    const card = document.querySelector('[data-model="chat"]')!;
    expect(card.getAttribute('aria-busy')).toBe('true');
    expect(card.querySelector('.badge')?.textContent).toContain('Park…');
    expect(card.querySelector('.spinner')).not.toBeNull();
    expect(button('Park').disabled).toBe(true);
  });
});

describe('U20 Overview', () => {
  it('shows reservations per model and marks parked memory, without counting stopped models', async () => {
    role({ snapshots: [snapshotWith([
      // SPEC R10 / ADR 0019: device residence needs assignment and domain evidence.
      deployment({ name: 'active', startup: { bytes: 16 * GiB, provenance: 'declared', device_bytes: 12 * GiB, host_bytes: 4 * GiB }, instances: [{ ...instance(0, 'ready'), devices: [{ id: 'GPU-0' }] }] }),
      deployment({ id: 'park', name: 'sleeping', observed_state: 'parked', parked: { bytes: 8 * GiB, provenance: 'measured', measured: [{ host_id: 'gpu-box', installation: 'engine', domain: 'gpu0', bytes: 8 * GiB, measured_at_ms: 1 }] }, instances: [{ ...instance(0, 'parked'), devices: [{ id: 'GPU-0' }] }] }),
      deployment({ id: 'off', name: 'off', observed_state: 'stopped', startup: { bytes: GiB, provenance: 'declared' }, instances: [instance(0, 'stopped')] }),
    ])] });
    await open('/');
    const segments = [...document.querySelectorAll<HTMLElement>('[data-model-segment]')];
    expect(segments.reduce((sum, s) => sum + Number(s.dataset.bytes), 0)).toBe(24 * GiB);
    expect(segments.filter((s) => s.classList.contains('parked')).map((s) => s.dataset.modelSegment)).toEqual(['sleeping']);
    expect(document.querySelector('.status-line')?.textContent).toContain('1 online');
  });

  // Found on a real standalone role, 2026-11-13: two full-sentence warnings
  // were drawn as one chip with no break between them.
  it('lists each host memory warning on its own line under a count', async () => {
    const warnings = [
      'gpu0 memory has 2.9 GiB available, less than its 14.7 GiB managed limit plus its 0.0 GiB free reserve (14.7 GiB): a deployment near the limit cannot be admitted until 11.8 GiB more is free',
      'system memory has 16.5 GiB available, less than its 30.6 GiB managed limit plus its 12.2 GiB free reserve (42.9 GiB): a deployment near the limit cannot be admitted until 26.4 GiB more is free',
    ];
    role({ snapshots: [snapshotWith([deployment()])], route: (call) => (call.url === '/management/v1/hosts'
      ? json({ api_version: '1', hosts: [{ ...STANDALONE_HOST, session: { ...STANDALONE_HOST.session, memory_warnings: warnings } }] })
      : undefined) });
    await open('/');
    const block = document.querySelector('.host .warnings')!;
    expect(block.querySelector('summary')?.textContent).toContain('2 memory warnings');
    expect([...block.querySelectorAll('li')].map((li) => li.textContent)).toEqual(warnings);
    expect(document.querySelector('.host .chip.warning')).toBeNull();
  });
  // SPEC R10, R15: silence and old readings must not promise currently free RAM.
  it('labels an unresponsive host and its retained memory reading', async () => {
    role({ snapshots: [snapshotWith([])], route: (call) => call.url === '/management/v1/hosts'
      ? json({ hosts: [{ ...STANDALONE_HOST, session: { ...STANDALONE_HOST.session, unresponsive: true,
        domains: STANDALONE_HOST.session.domains.map((domain) => ({ ...domain, observed_at_unix_ms: Date.now() - 60_000 })) } }] })
      : undefined });
    await open('/');
    const host = document.querySelector('.host')!;
    expect(host.querySelector('header .chip')?.textContent).toContain('unresponsive');
    expect(host.textContent).toContain('available at last reading · 1m ago');
    expect(host.textContent).not.toContain('GiB free');
  });

  it('fills recent activity from retained history on first load and deduplicates live events', async () => {
    let history: StreamMock | undefined;
    const server = role({ snapshots: [snapshotWith([deployment()])], route: (call) => {
      if (call.url === '/management/v1/events' && !call.headers.has('Last-Event-ID')) {
        history = new StreamMock(call.signal);
        return history.response;
      }
      return undefined;
    } });
    await open('/');
    history!.event(cursor(4), 'parked_committed', DEPLOYMENT_ID);
    history!.event(cursor(5), 'ready_committed', DEPLOYMENT_ID);
    await tick();
    expect(document.querySelector('.activity .events')?.textContent).toContain('Parked');
    expect(document.querySelector('.activity .events')?.textContent).toContain('Ready');
    expect(document.querySelector('.activity time')?.textContent).toContain('ago');
    server.streams[0].event(cursor(5), 'ready_committed', DEPLOYMENT_ID);
    await tick();
    expect(document.querySelectorAll('.activity .events li')).toHaveLength(2);
  });

  it('renders download progress from model_sources', async () => {
    role({ snapshots: [snapshotWith([deployment({ observed_state: 'starting', model_sources: [{ host_id: STANDALONE_HOST.host_id, source_key: 'weights', state: 'downloading', bytes_done: 4 * GiB, bytes_total: 10 * GiB }] })])] });
    await open('/');
    expect(document.querySelector('progress')?.value).toBe(4 * GiB);
    expect(document.querySelector('.download')?.textContent).toContain('40%');
  });
});

describe('U20 model detail', () => {
  it('shows the failure reason, hint, instance error, conditions and auto-loads the engine log', async () => {
    const server = role({ snapshots: [snapshotWith([deployment({ observed_state: 'failed', latest_operation: { id: 'op', kind: 'start', state: 'failed', error_code: 'engine_exited', reason: 'Engine exited with code 1', hint: 'Check the checkpoint path.' }, conditions: ['checkpoint_missing'], instances: [{ ...instance(0, 'failed'), last_error: 'Weights could not be opened' }] })])], route: (call) => call.url.includes('/engine-log?') ? json({ instance: 0, host_id: 'gpu-box', truncated: false, text: 'ERROR checkpoint missing' }) : undefined });
    await open(`/models/${DEPLOYMENT_ID}`);
    const why = document.querySelector('[aria-label="Why this model failed"]')!;
    expect(why.textContent).toContain('Engine exited with code 1');
    expect(why.textContent).toContain('Check the checkpoint path.');
    expect(why.textContent).toContain('Weights could not be opened');
    expect(why.textContent).toContain('checkpoint missing');
    expect(why.querySelector('pre')?.textContent).toBe('ERROR checkpoint missing');
    expect(server.calls.filter((c) => c.url.includes('/engine-log?'))).toHaveLength(1);
  });

  it('humanises latency, preserves sources, samples trends and avoids duplicate single-instance actions', async () => {
    const server = role({ snapshots: [snapshotWith([deployment({ actions: [{ action: 'park', available: true }] })])], route: (call) => call.url.includes('/metrics/latency?') ? json({ deployments: [{ deployment_id: DEPLOYMENT_ID, instances: [{ instance: 0, generation: 1, host_id: 'gpu-box', engine: 'vllm', series: [
      { name: 'router_time_to_first_content', tier: 'router', source: 'capyctl', count: 12, p50_seconds: 15.2, p95_seconds: 18.3 },
      { name: 'engine_inter_token_latency', tier: 'engine', source: 'engine', count: 12, p50_seconds: 0.025, p95_seconds: 0.04 },
    ] }] }] }) : undefined });
    await open(`/models/${DEPLOYMENT_ID}`);
    expect(document.querySelector('.metrics')?.textContent).toContain('15.2 s');
    expect(document.querySelector('.metrics')?.textContent).toContain('25 ms');
    expect(document.querySelector('.metrics')?.textContent).toContain('capyctl · router');
    expect(document.querySelector('.metrics')?.textContent).toContain('engine · engine');
    // The same derived per-stream rate the model card shows: 1 / 25 ms.
    const rate = Array.from(document.querySelectorAll('.metrics .metric')).find((m) => m.querySelector('.eyebrow')?.textContent === 'Tokens/s per stream');
    expect(rate?.querySelector('.numeral')?.textContent).toBe('40.0');
    expect(document.querySelectorAll('.actions')).toHaveLength(1);
    expect(document.querySelectorAll('.metrics svg[aria-label*="1 readings"]')).toHaveLength(5);
    await tick(5000);
    expect(server.calls.filter((c) => c.url.includes('/metrics/latency?'))).toHaveLength(2);
    expect(document.querySelector('.metrics svg[aria-label^="TTFT p50"]')?.getAttribute('aria-label')).toContain('2 readings');
  });
});

describe('U20 phone navigation', () => {
  it('provides Overview, Models, Playground and More, keeping metric labels separate from values', async () => {
    role({ snapshots: [snapshotWith([deployment()])] });
    await open('/');
    const nav = document.querySelector('nav[aria-label="Phone navigation"]')!;
    expect([...nav.querySelectorAll('a')].map((a) => a.getAttribute('href'))).toEqual(['/ui/', '/ui/models', '/ui/playground']);
    const more = nav.querySelector('button')!;
    expect(more.textContent).toContain('More');
    more.click();
    await tick();
    expect(more.getAttribute('aria-expanded')).toBe('true');
    const label = document.querySelector('[data-model="chat"] .reserved .eyebrow')!;
    expect(label.nextElementSibling?.tagName).toBe('STRONG');
    expect(label.parentElement?.classList.contains('fact')).toBe(true);
  });
});

describe('U20 Engines', () => {
  it('shows the per-host table and model usage, with removal in a menu', async () => {
    role({ snapshots: [snapshotWith([])], route: (call) => call.url === '/management/v1/engines' ? json({ engines: [{ host_id: 'gpu-box', host: 'gpu-box', online: true, profile: 'sglang-main', engine: 'sglang', version: '0.5.22', custom: false, retiring: false, deployments: ['chat', 'coder'] }] }) : undefined });
    await open('/engines');
    const table = document.querySelector('table')!;
    expect(table.textContent).toContain('sglang-main');
    expect(table.textContent).toContain('2 models');
    expect(table.textContent).toContain('chat, coder');
    expect(table.querySelector('details')?.open).toBe(false);
    table.querySelector('summary')!.click();
    await tick();
    expect(table.querySelector('details')?.open).toBe(true);
    expect(button('Remove', table).disabled).toBe(false);
  });

  it('keeps detection behind a button and custom preview behind a dialog', async () => {
    const server = role({ snapshots: [snapshotWith([])] });
    await open('/engines');
    expect(document.querySelector('[name="search-paths"]')).toBeNull();
    expect(server.calls.some((c) => c.url.includes('/engines/detected'))).toBe(false);
    button('Detect engines').click();
    await tick();
    expect(document.querySelector('[name="search-paths"]')).not.toBeNull();
    const custom = document.querySelector('dialog')!;
    expect(custom.open).toBe(false);
    button('Add custom engine…').click();
    await tick();
    expect(custom.open).toBe(true);
    expect(button('Preview', custom).disabled).toBe(true);
  });
});

// U20 / R10: the displayed rate is explicitly a per-stream estimate, not
// a fabricated role-side token counter.
it('derives per-stream tok/s from the served inter-token p50 and renders fresh KV and queue readings', async () => {
  role({ snapshots: [snapshotWith([deployment()])], route: (call) => {
    if (call.url.startsWith('/management/v1/metrics/load')) return json({ deployments: [{ deployment_id: DEPLOYMENT_ID, router: { in_flight: 3, waiting: 2 }, instances: [{ index: 0, sample: { fresh: true, engine: { kv_usage_ppm: 250000 } } }] }] });
    if (call.url.startsWith('/management/v1/metrics/latency')) return json({ deployments: [{ deployment_id: DEPLOYMENT_ID, instances: [{ instance: 0, generation: 1, host_id: 'gpu-box', engine: 'vllm', series: [{ name: 'engine_inter_token_latency', tier: 'engine', source: 'engine', count: 5, p50_seconds: 0.025, p95_seconds: 0.04 }] }] }] });
    return undefined;
  } });
  await open('/');
  const card = document.querySelector('[data-model="chat"]')!;
  expect(card.textContent).toContain('tok/s / stream derived');
  expect(card.querySelectorAll('.metric strong')[1].textContent).toBe('40.0');
  expect(card.textContent).toContain('3 in flight · 2 waiting');
  expect(card.querySelector('.kv > span')?.textContent).toBe('KV');
  expect(card.querySelector('.kv > strong')?.textContent).toBe('25.0%');
});

describe('U20 current versus retained measurements', () => {
  function retainedLatency() {
    return json({ deployments: [{ deployment_id: DEPLOYMENT_ID, instances: [{ instance: 0, generation: 1, host_id: 'gpu-box', engine: 'vllm', series: [
      { name: 'router_time_to_first_content', tier: 'router', source: 'capyctl', count: 12, p50_seconds: 0.21, p95_seconds: 0.53 },
      { name: 'engine_inter_token_latency', tier: 'engine', source: 'engine', count: 12, p50_seconds: 0.0295, p95_seconds: 0.0385 },
    ] }] }] });
  }
  it.each<ObservedState>(['parked', 'stopped', 'failed', 'starting'])('does not present retained latency or throughput as current for a %s model', async (state) => {
    role({ snapshots: [snapshotWith([deployment({ observed_state: state })])], route: (call) => call.url.startsWith('/management/v1/metrics/latency') ? retainedLatency() : undefined });
    await open('/');
    const card = document.querySelector('[data-model="chat"]')!;
    expect([...card.querySelectorAll('.metric strong')].map((value) => value.textContent)).toEqual(['–', '–']);
  });
  it.each<ObservedState>(['failed', 'stopped'])('does not call retained %s-model measurements live', async (state) => {
    // SPEC R13: an older launch remains useful only as explicitly previous readings.
    role({ snapshots: [snapshotWith([deployment({ observed_state: state, instances: [{ ...instance(0, state), generation: '2' }] })])], route: (call) => call.url.startsWith('/management/v1/metrics/latency') ? retainedLatency() : undefined });
    await open(`/models/${DEPLOYMENT_ID}`);
    expect([...document.querySelectorAll('.metrics .numeral')].map((value) => value.textContent)).toEqual(['210 ms', '530 ms', '30 ms', '33.9', '12']);
    expect(document.querySelector('.section-heading')?.textContent).toContain('Previous readings');
    expect([...document.querySelectorAll('.metrics article small')].every((caption) => caption.textContent?.startsWith('Previous · '))).toBe(true);
    expect(document.querySelector('.measurements summary')?.textContent).toContain('Previous');
  });
  // SPEC R13: a ready model must not inherit the previous launch's percentiles.
  it('excludes older-generation measurements from live detail readings', async () => {
    role({ snapshots: [snapshotWith([deployment({ instances: [{ ...instance(0, 'ready'), generation: '2' }] })])], route: (call) => call.url.startsWith('/management/v1/metrics/latency') ? retainedLatency() : undefined });
    await open(`/models/${DEPLOYMENT_ID}`);
    expect(document.querySelector('.metrics')?.textContent).not.toContain('210 ms');
    expect(document.querySelector('.metrics')?.textContent).not.toContain('30 ms');
    expect(document.querySelector('.measurements')).toBeNull();
  });
  it.each<ObservedState>(['failed', 'stopped'])('omits empty tiles when a %s model has no retained measurements', async (state) => {
    role({ snapshots: [snapshotWith([deployment({ observed_state: state })])] });
    await open(`/models/${DEPLOYMENT_ID}`);
    expect(document.querySelector('.metrics')).toBeNull();
    expect(document.body.textContent).toContain('Readings appear while the model is ready.');
  });

  it('labels sidebar counts and renders every status separator with spaces', async () => {
    role({ snapshots: [snapshotWith([deployment(), deployment({ id: 'off', observed_state: 'stopped' }), deployment({ id: 'bad', observed_state: 'failed' })])] });
    await open('/');
    expect(document.querySelector('#sidebar a[href="/ui/models"] small')?.textContent).toBe('1/3 ready');
    expect(document.querySelector('#sidebar a[href="/ui/hosts"] small')?.textContent).toBe('1 online');
    const status = document.querySelector('.status-line')!.textContent!;
    expect(status).toContain('1 stopped · 1 failed · ');
    expect(status.split(' · ')).toHaveLength(8);
  });
});
