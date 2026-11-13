// ADR 0040 §§10, 11 (plan U16): engines, invitations and host
// administration, driven through the mounted App against the management
// API's shapes.
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { Call } from './helpers';
import { json, managementError } from './helpers';
import { button, buttons, open, role, setUp, snapshotWith, STANDALONE_HOST, tearDown, tick } from './fixtures';

beforeEach(setUp);
afterEach(tearDown);

const HOST = `/management/v1/hosts/${STANDALONE_HOST.host_id}`;
const PREVIEW = `${HOST}/custom-engines/preview`;
const CONFIRM = `${HOST}/custom-engines`;

function field(name: string): HTMLInputElement | HTMLTextAreaElement {
  const found = document.querySelector<HTMLInputElement | HTMLTextAreaElement>(`[name="${name}"]`);
  if (found === null) {
    throw new Error(`no field ${name}`);
  }
  return found;
}

async function type(name: string, value: string): Promise<void> {
  const element = field(name);
  element.value = value;
  element.dispatchEvent(new Event('input', { bubbles: true }));
  await tick();
}

function text(): string {
  return document.body.textContent ?? '';
}

function preview(executable: string, digest: string): Response {
  return json({ command: [executable, '--port', '{port}'], summary: `Custom engine my-engine runs ${executable}`, digest });
}

async function fillCustom(): Promise<void> {
  button('Add custom engine…').click();
  await tick();
  await type('custom-name', 'my-engine');
  await type('custom-executable', '/opt/engine/serve');
  await type('custom-manifest', 'api: openai\n');
  button('Preview').click();
  await tick();
}

describe('custom engines', () => {
  // SPEC R25, AE6
  it('keeps Confirm disabled until the typed name matches, and sends the preview digest', async () => {
    const confirms: Call[] = [];
    role({
      snapshots: [snapshotWith([])],
      route: (call) => {
        if (call.url === PREVIEW) {
          return preview('/opt/engine/serve', 'digest-1');
        }
        if (call.url === CONFIRM) {
          confirms.push(call);
          return json({ profile: 'my-engine' });
        }
        return undefined;
      },
    });
    await open('/engines');
    await fillCustom();
    expect(text()).toContain('/opt/engine/serve --port {port}');
    expect(button('Confirm').disabled).toBe(true);
    await type('custom-confirm', 'my-engin');
    expect(button('Confirm').disabled).toBe(true);
    await type('custom-confirm', 'my-engine');
    expect(button('Confirm').disabled).toBe(false);
    button('Confirm').click();
    await tick();
    expect(JSON.parse(confirms[0].body ?? '{}')).toMatchObject({ digest: 'digest-1', confirm_name: 'my-engine', name: 'my-engine' });
    expect(text()).toContain('The engine is added and its installation is pinned.');
  });

  // ADR 0040 §11
  it('re-runs the preview after confirmation_stale and shows the new command', async () => {
    const previews: Call[] = [];
    role({
      snapshots: [snapshotWith([])],
      route: (call) => {
        if (call.url === PREVIEW) {
          previews.push(call);
          return previews.length === 1 ? preview('/opt/engine/serve', 'digest-1') : preview('/opt/engine/serve-v2', 'digest-2');
        }
        return call.url === CONFIRM ? managementError(409, 'confirmation_stale') : undefined;
      },
    });
    await open('/engines');
    await fillCustom();
    await type('custom-confirm', 'my-engine');
    button('Confirm').click();
    await tick();
    expect(previews).toHaveLength(2);
    expect(text()).toContain('/opt/engine/serve-v2 --port {port}');
    expect(text()).toContain('The host’s details changed after you reviewed the change.');
    expect(button('Confirm').disabled).toBe(true);
  });
});

describe('host administration', () => {
  // SPEC R27, ADR 0040 §10 (U16)
  it('lists model copies and reloads after pruning only unused copies', async () => {
    const used = { key: 'models/chat', bytes: 4 * 1024 ** 3, referenced: true };
    const unused = { key: 'models/old', bytes: 2 * 1024 ** 3, referenced: false };
    const prunes: Call[] = [];
    let lists = 0;
    role({
      snapshots: [snapshotWith([])],
      route: (call) => {
        if (call.url === `${HOST}/model-store`) {
          lists += 1;
          return json({ copies: prunes.length ? [used] : [used, unused] });
        }
        if (call.url === `${HOST}/model-store/prune`) {
          prunes.push(call);
          return json({ removed: [{ key: unused.key, bytes: unused.bytes }], removed_bytes: unused.bytes });
        }
        return undefined;
      },
    });
    await open(`/hosts/${STANDALONE_HOST.host_id}`);
    expect(text()).toContain('models/chat');
    expect(text()).toContain('4.0 GiB');
    expect(text()).toContain('models/old');
    expect(text()).toContain('2.0 GiB (unused)');
    expect(button('Remove unused copies').disabled).toBe(false);
    button('Remove unused copies').click();
    await tick();
    expect(prunes).toHaveLength(1);
    expect(prunes[0].method).toBe('POST');
    expect(JSON.parse(prunes[0].body ?? '{}')).toEqual({ apply: true });
    expect(lists).toBe(2);
    expect(text()).toContain('Removed 1 copy, 2.0 GiB.');
    expect(text()).toContain('models/chat');
    expect(text()).not.toContain('models/old');
    expect(button('Remove unused copies').disabled).toBe(true);
  });

  // SPEC R27, ADR 0040 §10 (U16)
  it('shows an empty model store without offering pruning', async () => {
    const api = role({
      snapshots: [snapshotWith([])],
      route: (call) => call.url === `${HOST}/model-store` ? json({ copies: [] }) : undefined,
    });
    await open(`/hosts/${STANDALONE_HOST.host_id}`);
    expect(text()).toContain('No model has been downloaded to this host.');
    expect(buttons('Remove unused copies')).toHaveLength(0);
    expect(api.posts()).toHaveLength(0);
  });

  // SPEC R24, ADR 0040 §10 (U16)
  it('preserves an in-use engine and its model usage when removal is refused', async () => {
    const removals: Call[] = [];
    const engine = { host_id: STANDALONE_HOST.host_id, host: STANDALONE_HOST.name, online: true, profile: 'sglang-main', engine: 'sglang', version: '0.5.22', custom: false, retiring: false, deployments: ['chat', 'coder'] };
    role({
      snapshots: [snapshotWith([])],
      route: (call) => {
        if (call.url === '/management/v1/engines') {
          return json({ engines: [engine] });
        }
        if (call.url === `${HOST}/engines/sglang-main/remove`) {
          removals.push(call);
          return managementError(409, 'profile_in_use');
        }
        return undefined;
      },
    });
    await open('/engines');
    document.querySelector('details.overflow')?.setAttribute('open', '');
    button('Remove').click();
    await tick();
    expect(removals).toHaveLength(1);
    expect(removals[0].method).toBe('POST');
    expect(JSON.parse(removals[0].body ?? '{}')).toEqual({});
    expect(document.querySelector('[role="alert"]')).not.toBeNull();
    const row = document.querySelector('tbody tr')!;
    expect(row.textContent).toContain('sglang-main');
    expect(row.textContent).toContain('chat, coder');
    expect(row.textContent).toContain('2 models');
    expect(row.textContent).toContain('available');
    expect(text()).not.toContain('being removed');
    expect(button('Remove').disabled).toBe(false);
  });

  // SPEC R24, ADR 0040 §10 (U16): a 202 is not completed removal.
  it('follows accepted removal through retiring until the engine disappears', async () => {
    const removals: Call[] = [];
    const engine = { host_id: STANDALONE_HOST.host_id, host: STANDALONE_HOST.name, online: true, profile: 'sglang-main', engine: 'sglang', version: '0.5.22', custom: false, retiring: false, deployments: [] };
    let gone = false;
    role({
      snapshots: [snapshotWith([])],
      route: (call) => {
        if (call.url === '/management/v1/engines') {
          return json({ engines: gone ? [] : [{ ...engine, retiring: removals.length > 0 }] });
        }
        if (call.url === `${HOST}/engines/sglang-main/remove`) {
          removals.push(call);
          return json({ retiring: true }, 202);
        }
        return undefined;
      },
    });
    await open('/engines');
    document.querySelector('details.overflow')?.setAttribute('open', '');
    button('Remove').click();
    await tick();
    expect(removals).toHaveLength(1);
    expect(text()).toContain('sglang-main');
    expect(text()).toContain('being removed');
    expect(buttons('Remove')).toHaveLength(0);
    expect(document.querySelector('[aria-label="Actions for sglang-main"]')).toBeNull();
    gone = true;
    await tick(5000);
    expect(text()).not.toContain('sglang-main');
    expect(text()).not.toContain('being removed');
    expect(text()).toContain('No engines are installed on this host.');
    expect(removals).toHaveLength(1);
  });

  // ADR 0040 §10
  it('shows the update instruction when the host refuses host_capability_missing:host_admin', async () => {
    role({
      snapshots: [snapshotWith([])],
      route: (call) => (call.url.startsWith(`${HOST}/engines/detected`) ? managementError(409, 'host_capability_missing:host_admin') : undefined),
    });
    await open('/engines');
    button('Detect engines').click();
    await tick();
    button('Detect').click();
    await tick();
    expect(text()).toContain('Update CapyCTL on the host and restart it, then try again.');
  });

  // KTD11 (plan U8): a candidate found under an extra directory is added
  // with those directories, so the host does not ask for a confirmation.
  it('adds a detected engine with the extra directories it was found under', async () => {
    const adds: Call[] = [];
    role({
      snapshots: [snapshotWith([])],
      route: (call) => {
        if (call.url.startsWith(`${HOST}/engines/detected`)) {
          return json({ candidates: [{ engine: 'vllm', version: '0.32.0', custom: false, entry: '/opt/vllm/bin/vllm', source: 'path' }] });
        }
        if (call.method === 'POST' && call.url === `${HOST}/engines`) {
          adds.push(call);
          return json({ profile: 'vllm' });
        }
        return undefined;
      },
    });
    await open('/engines');
    button('Detect engines').click();
    await tick();
    await type('search-paths', '/opt/vllm, /srv/engines');
    button('Detect').click();
    await tick();
    button('Add').click();
    await tick();
    expect(adds).toHaveLength(1);
    expect(JSON.parse(adds[0].body ?? '{}')).toEqual({ path: '/opt/vllm/bin/vllm', paths: ['/opt/vllm', '/srv/engines'] });
  });
});

describe('invitations', () => {
  const INVITATION = {
    version: 1,
    server_address: 'https://server.example:7443',
    control_address: 'https://server.example:7444',
    server_ca: 'CA',
    invitation_id: 'inv-1',
    invitation_secret: 'secret-1',
    host_name: 'gpu-box-2',
    expires_unix: 1793150000,
  };

  // SPEC R26, F4 (plan KTD16): plain HTTP on a non-loopback address has no
  // Clipboard API, so Copy selects the command.
  it('selects the join command when the clipboard is unavailable', async () => {
    vi.stubGlobal('isSecureContext', false);
    vi.stubGlobal('navigator', { ...navigator, clipboard: undefined });
    role({
      snapshots: [snapshotWith([])],
      route: (call) => {
        if (call.url === '/management/v1/installation') {
          return managementError(404, 'not_found');
        }
        return call.url === '/management/v1/host-invitations' ? json(INVITATION, 201) : undefined;
      },
    });
    await open('/hosts');
    await type('invite-host', 'gpu-box-2');
    button('Create invitation').click();
    await tick();
    button('Copy').click();
    await tick();
    const selected = window.getSelection()?.toString() ?? '';
    expect(selected).toContain('capyctl join host --join-file capyctl-host.join');
    expect(selected).toContain('"invitation_secret":"secret-1"');
    expect(text()).toContain('The command is selected');
  });

  // SPEC R26
  it('shows no invitation controls on a standalone role', async () => {
    role({
      snapshots: [snapshotWith([])],
      route: (call) => (call.url === '/management/v1/installation' ? json({ api_version: '1' }) : undefined),
    });
    await open('/hosts');
    await tick();
    expect(buttons('Create invitation')).toHaveLength(0);
    expect(document.querySelector('[name="invite-host"]')).toBeNull();
  });
});
