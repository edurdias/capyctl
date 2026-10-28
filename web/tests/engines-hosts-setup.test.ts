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
    expect(text()).toContain('The engine changed on the host after you reviewed it.');
    expect(button('Confirm').disabled).toBe(true);
  });
});

describe('host administration', () => {
  // ADR 0040 §10
  it('shows the update instruction when the host refuses host_capability_missing:host_admin', async () => {
    role({
      snapshots: [snapshotWith([])],
      route: (call) => (call.url.startsWith(`${HOST}/engines/detected`) ? managementError(409, 'host_capability_missing:host_admin') : undefined),
    });
    await open('/engines');
    button('Detect').click();
    await tick();
    expect(text()).toContain('Update CapyCTL on the host and restart it, then try again.');
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
