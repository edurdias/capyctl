// ADR 0040 §8 (plan U14, KTD16): lifecycle actions, drain and revoke,
// driven through the mounted App against the management API's shapes.
import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import type { Call } from './helpers';
import { cursor, json, managementError } from './helpers';
import {
  button,
  buttons,
  deployment,
  DEPLOYMENT_ID,
  dialog,
  open,
  role,
  setUp,
  snapshotWith,
  STANDALONE_HOST,
  tearDown,
  tick,
} from './fixtures';

beforeEach(setUp);
afterEach(tearDown);

const ACTIONS = `/management/v1/deployments/${DEPLOYMENT_ID}/actions`;

function body(call: Call): Record<string, unknown> {
  return JSON.parse(call.body ?? 'null') as Record<string, unknown>;
}

function accepted(operation = '01JBX7Q4M2N8P0R5S7T9V1W3OP'): Response {
  return json({ api_version: '1', operation_id: operation, deployment_id: DEPLOYMENT_ID, joined: false, revision: '3' }, 202);
}

describe('lifecycle actions', () => {
  // SPEC R16, R17, F3 (plan KTD16)
  it('offers Make room and start after a start refused capacity_blocked, and sends evict: true', async () => {
    const stopped = deployment({
      observed_state: 'stopped',
      actions: [
        { action: 'start', available: true },
        { action: 'stop', available: false, reason: 'lifecycle_conflict' },
      ],
    });
    const actions: Call[] = [];
    role({
      snapshots: [snapshotWith([stopped])],
      route: (call) => {
        if (call.url !== ACTIONS) {
          return undefined;
        }
        actions.push(call);
        return actions.length === 1 ? managementError(409, 'capacity_blocked') : accepted();
      },
    });
    await open('/');
    expect(buttons('Make room and start')).toHaveLength(0);
    button('Start').click();
    await tick();
    expect(body(actions[0])).not.toHaveProperty('evict');
    expect(document.body.textContent).toContain('There is not enough free memory, GPU or port room on the host.');

    button('Make room and start').click();
    await tick();
    expect(actions).toHaveLength(2);
    expect(body(actions[1])).toMatchObject({ action: 'start', expected_revision: 3, evict: true });
    expect(actions[1].headers.get('Idempotency-Key')).not.toBe(actions[0].headers.get('Idempotency-Key'));
  });

  // Plan U14: an unavailable action shows its reason as visible text.
  it('shows a disabled action with its reason as text', async () => {
    role({
      snapshots: [snapshotWith([deployment({ actions: [{ action: 'start', available: false, reason: 'runtime_retained' }] })])],
    });
    await open('/');
    expect(button('Start').disabled).toBe(true);
    expect(document.querySelector('.why')?.textContent).toBe('The model’s engine is still being cleaned up.');
  });

  // SPEC R16, R18 (plan KTD16; crates/capyctl-cli/src/client/delete.rs)
  it('reads Stop and delete on a ready model, and deletes only after the stop settles on the event stream', async () => {
    const ready = deployment({
      actions: [
        { action: 'stop', available: true },
        { action: 'delete', available: false, reason: 'delete_requires_cleanup' },
      ],
    });
    const stopOp = '01JBX7Q4M2N8P0R5S7T9V1W3ST';
    const stopping = { id: stopOp, deployment_id: DEPLOYMENT_ID, action: 'stop', state: 'running', has_error: false, accepted_at: '1' };
    const actions: Call[] = [];
    const server = role({
      snapshots: [
        snapshotWith([ready], 5),
        // The refresh after the first event: the stop is still running.
        snapshotWith([{ ...ready, observed_state: 'stopping' }], 6, [stopping]),
        // After the next event: it has settled.
        snapshotWith([{ ...ready, observed_state: 'stopped', ready_instances: 0 }], 7, [{ ...stopping, state: 'succeeded' }]),
      ],
      route: (call) => {
        if (call.url !== ACTIONS) {
          return undefined;
        }
        actions.push(call);
        return accepted(body(call).action === 'stop' ? stopOp : undefined);
      },
    });
    await open('/');
    expect(buttons('Delete')).toHaveLength(0);
    button('Stop and delete').click();
    await tick();
    expect(actions).toHaveLength(0);

    button('Stop and delete', dialog()).click();
    await tick();
    expect(actions.map((call) => body(call).action)).toEqual(['stop']);

    server.streams[0].event(cursor(6), 'ordinary_cleanup_accepted', DEPLOYMENT_ID);
    await tick(1000);
    expect(actions.map((call) => body(call).action)).toEqual(['stop']);
    expect(document.body.textContent).toContain('Waiting for the model to stop…');

    server.streams[0].event(cursor(7), 'ordinary_cleanup_completed', DEPLOYMENT_ID);
    await tick(1000);
    expect(actions.map((call) => body(call).action)).toEqual(['stop', 'delete']);
    expect(actions[1].headers.get('Idempotency-Key')).not.toBe(actions[0].headers.get('Idempotency-Key'));
  });

  // SPEC §13 (plan KTD16): a Retry is the same request; a click a new one.
  it('reuses the idempotency key on Retry after deadline_exceeded, and a fresh click uses a new one', async () => {
    const actions: Call[] = [];
    role({
      snapshots: [snapshotWith([deployment({ actions: [{ action: 'park', available: true }] })])],
      route: (call) => {
        if (call.url !== ACTIONS) {
          return undefined;
        }
        actions.push(call);
        return actions.length === 1 ? managementError(504, 'deadline_exceeded', true) : accepted();
      },
    });
    await open('/');
    button('Park').click();
    await tick();
    expect(document.body.textContent).toContain('CapyCTL did not answer in time.');

    button('Retry').click();
    await tick();
    expect(actions[1].headers.get('Idempotency-Key')).toBe(actions[0].headers.get('Idempotency-Key'));
    expect(actions[1].body).toBe(actions[0].body);
    expect(buttons('Retry')).toHaveLength(0);

    button('Park').click();
    await tick();
    expect(actions).toHaveLength(3);
    expect(actions[2].headers.get('Idempotency-Key')).not.toBe(actions[0].headers.get('Idempotency-Key'));
  });

  it('reloads the snapshot after revision_conflict and says the model changed elsewhere', async () => {
    const server = role({
      snapshots: [snapshotWith([deployment({ actions: [{ action: 'park', available: true }] })])],
      route: (call) => (call.url === ACTIONS ? managementError(409, 'revision_conflict') : undefined),
    });
    await open('/');
    const reads = () => server.calls.filter((call) => call.url === '/management/v1/snapshot').length;
    const before = reads();
    button('Park').click();
    await tick();
    expect(reads()).toBe(before + 1);
    expect(document.body.textContent).toContain('This model was changed elsewhere');
    expect(buttons('Retry')).toHaveLength(0);
  });
});

describe('host actions', () => {
  const HOST = STANDALONE_HOST.host_id;

  function hostRole(kind: 'standalone' | 'server') {
    const posts: Call[] = [];
    role({
      snapshots: [snapshotWith([])],
      route: (call) => {
        if (call.url === '/management/v1/installation') {
          return kind === 'standalone' ? json({ api_version: '1', installation: null, installations: [] }) : managementError(404, 'not_found');
        }
        if (call.method === 'POST') {
          posts.push(call);
          if (call.url.endsWith('/drain')) {
            return json({ api_version: '1', host: HOST, host_state: 'online', stops: 'issued', operations: [], refused: [] }, 202);
          }
          return json({ api_version: '1', host_id: HOST, name: HOST, revoked: true, newly_revoked: true });
        }
        return undefined;
      },
    });
    return posts;
  }

  // SPEC R18, R19
  it('does nothing on Drain or Revoke until confirmed', async () => {
    const posts = hostRole('server');
    await open(`/hosts/${HOST}`);
    await tick();

    button('Drain').click();
    await tick();
    button('Cancel', dialog()).click();
    await tick();
    button('Revoke').click();
    await tick();
    button('Cancel', dialog()).click();
    await tick();
    expect(posts).toHaveLength(0);

    button('Drain').click();
    await tick();
    button('Drain', dialog()).click();
    await tick();
    expect(posts.map((call) => call.url)).toEqual([`/management/v1/hosts/${HOST}/drain`]);
    expect(posts[0].headers.get('Idempotency-Key')).toMatch(/^[0-9a-f]{32}$/);
    expect(body(posts[0]).deadline_ms).toEqual(expect.any(Number));

    button('Revoke').click();
    await tick();
    button('Revoke', dialog()).click();
    await tick();
    expect(posts.map((call) => call.url)).toEqual([`/management/v1/hosts/${HOST}/drain`, `/management/v1/hosts/${HOST}/revoke`]);
  });

  it('hides Revoke on a standalone role', async () => {
    hostRole('standalone');
    await open(`/hosts/${HOST}`);
    await tick();
    expect(buttons('Drain')).toHaveLength(1);
    expect(buttons('Revoke')).toHaveLength(0);
  });
});
