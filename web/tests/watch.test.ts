// ADR 0040 (plan U13): the watch views (Overview, model detail, Activity),
// driven through the mounted App against the management API's shapes.
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { json, managementError } from './helpers';
import { button, deployment, DEPLOYMENT_ID, instance, open, role, setUp, snapshotWith, tearDown, tick } from './fixtures';

beforeEach(setUp);
afterEach(tearDown);

function badgeOf(name: string): string | undefined {
  return document.querySelector(`[data-model="${name}"] .badge`)?.textContent?.trim();
}

describe('Overview', () => {
  // SPEC R10, R15 (plan KTD15)
  it('reads a waking model as starting and a reconciling one as checking', async () => {
    role({
      snapshots: [
        snapshotWith([
          deployment({ id: '01JBX7Q4M2N8P0R5S7T9V1W3AB', name: 'waking-model', observed_state: 'waking' }),
          deployment({ id: '01JBX7Q4M2N8P0R5S7T9V1W3AC', name: 'reconciling-model', observed_state: 'reconciling' }),
        ]),
      ],
    });
    await open('/');
    expect(badgeOf('waking-model')).toBe('starting');
    expect(badgeOf('reconciling-model')).toBe('checking');
  });

  it('offers Add model when no model is added yet', async () => {
    role({ snapshots: [snapshotWith([])] });
    await open('/');
    expect(document.body.textContent).toContain('No models are added yet.');
    const add = Array.from(document.querySelectorAll('a')).find((a) => a.textContent === 'Add model');
    expect(add?.getAttribute('href')).toBe('/ui/models/new');
  });

  it('shows each host with its GPU memory and host RAM in use', async () => {
    role({ snapshots: [snapshotWith([])] });
    await open('/');
    const meters = Array.from(document.querySelectorAll('[role="meter"]'), (m) => [m.getAttribute('aria-label'), m.getAttribute('aria-valuetext')]);
    expect(meters).toEqual([
      ['GPU GPU-0', '16.0 of 24.0 GiB'],
      ['Host RAM', '32.0 of 64.0 GiB'],
    ]);
  });
});

describe('model detail', () => {
  const LOG = `/management/v1/deployments/${DEPLOYMENT_ID}/engine-log?instance=0&kib=64`;

  // ADR 0040 §14 (plan KTD18): an engine log renders as text.
  it('renders an engine log containing <script> as literal text', async () => {
    const text = 'INFO ready\n<script>window.injected = true</script>\n<img src=x onerror="window.injected = true">';
    role({
      snapshots: [snapshotWith([deployment()])],
      route: (call) =>
        call.url === LOG
          ? json({ api_version: '1', deployment_id: DEPLOYMENT_ID, instance: 0, host_id: 'gpu-box', incarnation: '1', kib: 64, truncated: false, text, read_at_ms: 0 })
          : undefined,
    });
    await open(`/models/${DEPLOYMENT_ID}`);
    button('Engine log').click();
    await tick();
    const pre = document.querySelector('pre.log');
    expect(pre?.textContent).toBe(text);
    expect(pre?.querySelector('script, img')).toBeNull();
    expect('injected' in window).toBe(false);
  });

  // SPEC §13.3: a stopped instance has no running launch to read.
  it('shows "no running engine" for a 404 not_found engine log', async () => {
    role({
      snapshots: [snapshotWith([deployment({ observed_state: 'stopped', instances: [instance(0, 'stopped')] })])],
      route: (call) => (call.url === LOG ? managementError(404, 'not_found') : undefined),
    });
    await open(`/models/${DEPLOYMENT_ID}`);
    button('Engine log').click();
    await tick();
    expect(document.body.textContent).toContain('has no running engine');
    expect(document.querySelector('[role="alert"]')).toBeNull();
  });
});
