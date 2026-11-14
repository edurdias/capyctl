import { flushSync, mount, unmount } from 'svelte';
import { vi } from 'vitest';
import App from '../src/App.svelte';
import { navigate } from '../src/lib/router';
import type { ActionHint, DeploymentSnapshot, InstanceSnapshot, ObservedState, Snapshot } from '../src/lib/types';
import { cursor, installFetch, json, settle, setVisibility, snapshot, StreamMock, type Call } from './helpers';

export const DEPLOYMENT_ID = '01JBX7Q4M2N8P0R5S7T9V1W3AA';

export function instance(index: number, state: ObservedState, actions: ActionHint[] = []): InstanceSnapshot {
  return {
    index,
    host_id: 'gpu-box',
    generation: '1',
    observed_state: state,
    lifecycle: 'active',
    operator_stopped: false,
    development_controls: { state: 'not_exposed' },
    requests: { cancelling: '0', cancelling_settles: 'stop', settle_when_idle: '0', settle_at_stop: '0' },
    actions,
  };
}

export function deployment(overrides: Partial<DeploymentSnapshot> = {}): DeploymentSnapshot {
  return {
    id: DEPLOYMENT_ID,
    name: 'chat',
    kind: 'model',
    revision: '3',
    generation: '1',
    desired_state: 'ready',
    observed_state: 'ready',
    admission_enabled: true,
    dispatch_enabled: true,
    suspended: false,
    development_controls: { state: 'not_exposed' },
    desired_instances: 1,
    ready_instances: 1,
    conditions: [],
    instances: [instance(0, 'ready')],
    hosts: [],
    timeouts: { request_deadline_ms: 900_000, initialize_ms: 300_000, stop_ms: 60_000 },
    actions: [],
    ...overrides,
  };
}

export function snapshotWith(deployments: DeploymentSnapshot[], sequence = 5, operations: Snapshot['operations'] = []): Snapshot {
  return { ...snapshot(cursor(sequence)), deployments, operations };
}

export const STANDALONE_HOST = {
  host_id: 'gpu-box',
  name: 'gpu-box',
  revoked: false,
  online: true,
  eligible: true,
  session: {
    domains: [
      { domain_id: 'gpu0', kind: 'device', device_id: 'GPU-0', capacity_bytes: 24 * 1024 ** 3, available_bytes: 8 * 1024 ** 3 },
      { domain_id: 'host', kind: 'host', capacity_bytes: 64 * 1024 ** 3, available_bytes: 32 * 1024 ** 3 },
    ],
  },
  binary_version: '1.0.0',
  compatibility: 'supported',
  compatibility_reason: null,
};

// ADR 0035 §8 / SPEC §7.2, T16: flattened domain fields, nullable privacy data.
export const OUTSIDE_GPU_HOLDERS = {
  outside_processes: [
    { pid: 101, name: 'python', bytes: 3 * 1024 ** 3, capyctl_state_dir: null, other_capyctl: false },
    { pid: 102, name: null, bytes: 2 * 1024 ** 3, capyctl_state_dir: null, other_capyctl: false },
    { pid: 103, name: 'capyctl', bytes: 1024 ** 3, capyctl_state_dir: `/home/operator/${'another-installation/'.repeat(20)}`, other_capyctl: true },
    { pid: 104, name: 'capyctl', bytes: 1024 ** 3, capyctl_state_dir: null, other_capyctl: true },
  ],
  outside_processes_remainder: { count: 4, bytes: 1024 ** 3 },
};

export type Route = (call: Call) => Response | Promise<Response> | undefined;

// A role answering the shell's reads: the snapshot (each read takes the next
// value; the last one repeats), an event stream the test writes into, and
// whatever `route` answers. Anything else is held open, as a slow server.
export function role(options: { snapshots: Snapshot[]; route?: Route }) {
  const streams: StreamMock[] = [];
  const snapshots = [...options.snapshots];
  const calls = installFetch((call) => {
    const answered = options.route?.(call);
    if (answered !== undefined) {
      return answered;
    }
    if (call.url === '/management/v1/snapshot') {
      return json(snapshots.length > 1 ? snapshots.shift()! : snapshots[0]);
    }
    if (call.url === '/management/v1/events') {
      const stream = new StreamMock(call.signal);
      if (!call.headers.has('Last-Event-ID')) {
        stream.raw(': history complete\n\n');
        return stream.response;
      }
      streams.push(stream);
      return stream.response;
    }
    if (call.url === '/management/v1/hosts') {
      return json({ api_version: '1', hosts: [STANDALONE_HOST] });
    }
    if (call.url === '/management/v1/engines') {
      return json({ api_version: '1', engines: [] });
    }
    if (call.url.startsWith('/management/v1/metrics/load')) {
      return json({ observed_at_ms: 0, stale_after_ms: 0, deployments: [] });
    }
    if (call.url.startsWith('/management/v1/metrics/latency')) {
      return json({ unit: 'seconds', deployments: [] });
    }
    return new Promise<Response>(() => {});
  });
  const posts = () => calls.filter((call) => call.method === 'POST');
  return { calls, streams, posts };
}

let app: ReturnType<typeof mount> | undefined;

// jsdom has no modal dialogs; these follow the `open` attribute as browsers do.
export function setUp(): void {
  vi.useFakeTimers();
  setVisibility('visible');
  HTMLDialogElement.prototype.showModal = function (this: HTMLDialogElement) {
    this.setAttribute('open', '');
  };
  HTMLDialogElement.prototype.close = function (this: HTMLDialogElement) {
    this.removeAttribute('open');
  };
}

export function tearDown(): void {
  if (app) {
    unmount(app);
    app = undefined;
  }
  vi.useRealTimers();
}

export async function open(path: string): Promise<void> {
  navigate(path, { replace: true });
  app = mount(App, { target: document.body });
  flushSync();
  await settle();
  flushSync();
}

export async function tick(ms = 0): Promise<void> {
  await vi.advanceTimersByTimeAsync(ms);
  flushSync();
}

// The buttons a person can see: a closed dialog's buttons are not shown.
export function buttons(text: string, within: ParentNode = document): HTMLButtonElement[] {
  return Array.from(within.querySelectorAll('button')).filter(
    (b) => b.textContent?.trim() === text && b.closest('dialog:not([open])') === null,
  );
}

export function button(text: string, within: ParentNode = document): HTMLButtonElement {
  const found = buttons(text, within);
  if (found.length !== 1) {
    throw new Error(`expected one "${text}" button, found ${found.length}`);
  }
  return found[0];
}

export function dialog(): HTMLDialogElement {
  const open = document.querySelector<HTMLDialogElement>('dialog[open]');
  if (open === null) {
    throw new Error('no open dialog');
  }
  return open;
}
