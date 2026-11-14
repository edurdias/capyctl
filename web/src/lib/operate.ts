import { ApiError, request } from './api';
import { newIdempotencyKey } from './idempotency';
import { snapshot } from './state';
import type { DeploymentSnapshot, HintedAction, Snapshot } from './types';

// ADR 0040 (plan KTD16): actions follow the CLI's request contract
// (crates/capyctl-cli/src/client.rs `action_on`) and never retry by
// themselves. An attempt is one request identity: a Retry resends the same
// key with the same body, deadline included; a new click makes a new attempt.

export interface ActionBody {
  action: HintedAction;
  expected_revision: number;
  // Absolute, in Unix milliseconds.
  deadline_ms: number;
  evict?: true;
}

export interface Attempt {
  path: string;
  body: unknown;
  key: string;
}

// The window the CLI gives each action when the deployment declares none
// (client.rs `LEGACY_WINDOW_MS`) and a drain's bound (drain.rs
// `DRAIN_WINDOW_MS`).
const LEGACY_WINDOW_MS = 900_000;
const DRAIN_WINDOW_MS = 900_000;

// What an action needs from a deployment. A model just added has no snapshot
// entry yet, only its id and revision, and takes the CLI's default windows.
export type ActionTarget = Pick<DeploymentSnapshot, 'id' | 'revision'> & Partial<Pick<DeploymentSnapshot, 'timeouts'>>;

// ADR 0014 amendment A1, as client.rs `window` computes it: a start takes the
// Initialize window, a preinitialize the request deadline, anything else the
// Stop window.
function windowMs(deployment: ActionTarget, action: HintedAction): number {
  const windows = deployment.timeouts;
  if (action === 'preinitialize') {
    return windows?.request_deadline_ms ?? LEGACY_WINDOW_MS;
  }
  if (action === 'start') {
    return windows?.initialize_ms ?? LEGACY_WINDOW_MS;
  }
  return windows?.stop_ms ?? LEGACY_WINDOW_MS;
}

function actionBody(deployment: ActionTarget, action: HintedAction, evict: boolean): ActionBody {
  const body: ActionBody = {
    action,
    expected_revision: Number(deployment.revision),
    deadline_ms: Date.now() + windowMs(deployment, action),
  };
  // Owner decision 2026-09-23: only a start evicts, and only when asked.
  if (evict && action === 'start') {
    body.evict = true;
  }
  return body;
}

const DEPLOYMENTS = '/management/v1/deployments';

export function deploymentAttempt(deployment: ActionTarget, action: HintedAction, evict = false): Attempt {
  return {
    path: `${DEPLOYMENTS}/${encodeURIComponent(deployment.id)}/actions`,
    body: actionBody(deployment, action, evict),
    key: newIdempotencyKey(),
  };
}

export function instanceAttempt(
  deployment: DeploymentSnapshot,
  index: number,
  action: 'start' | 'stop',
  evict = false,
): Attempt {
  return {
    path: `${DEPLOYMENTS}/${encodeURIComponent(deployment.id)}/instances/${index}/actions`,
    body: actionBody(deployment, action, evict),
    key: newIdempotencyKey(),
  };
}

// SPEC §4.3: drain a host by id or name, bounded as `capyctl drain host` is.
export function drainAttempt(host: string): Attempt {
  return {
    path: `/management/v1/hosts/${encodeURIComponent(host)}/drain`,
    body: { deadline_ms: Date.now() + DRAIN_WINDOW_MS },
    key: newIdempotencyKey(),
  };
}

export interface Receipt {
  operation_id?: string | null;
  deployment_id?: string;
}

// The answer to a request that went out but never came back: the action
// may or may not have gone through, so only a Retry with the same key is
// safe (SPEC §13).
export function lostResponse(): ApiError {
  return new ApiError({
    status: 0,
    code: null,
    message: 'CapyCTL’s answer did not arrive, so the action may or may not have gone through. Retry to find out; a retry never repeats it.',
    retryable: true,
    operationId: null,
    details: {},
    envelope: 'none',
  });
}

export async function send(attempt: Attempt): Promise<Receipt> {
  try {
    return await request<Receipt>(attempt.path, {
      method: 'POST',
      body: attempt.body,
      headers: { 'Idempotency-Key': attempt.key },
    });
  } catch (error) {
    if (error instanceof ApiError) {
      throw error;
    }
    throw lostResponse();
  }
}

// Whether a Retry with the same attempt is offered: the answer was lost, ran
// out of time, or the server said trying again may help.
export function retryable(error: ApiError): boolean {
  return error.status === 0 || error.code === 'deadline_exceeded' || error.retryable;
}

// SPEC §§4.1, 6.4: revoking is idempotent on the server (an already revoked
// host answers as a success), and the route takes an empty body
// (crates/capyctl-management/src/enrollment.rs `revoke`).
export async function revokeHost(host: string): Promise<void> {
  try {
    await request<unknown>(`/management/v1/hosts/${encodeURIComponent(host)}/revoke`, { method: 'POST' });
  } catch (error) {
    throw error instanceof ApiError ? error : lostResponse();
  }
}

const OPEN: Record<string, true> = { pending: true, running: true };
// client.rs `WAIT_MARGIN_MS`: allow cleanup evidence to arrive after Stop's
// own deadline, but never leave Stop and delete waiting indefinitely.
const WAIT_MARGIN_MS = 10_000;

// crates/capyctl-cli/src/client/delete.rs: the stop has settled once the
// deployment has no open operation and the stop itself (when one was
// accepted) is recorded as finished, or the deployment reads stopped.
function stopSettled(value: Snapshot, deploymentId: string, operationId: string | null): boolean {
  const operations = value.operations.filter((op) => op.deployment_id === deploymentId);
  if (operations.some((op) => OPEN[op.state])) {
    return false;
  }
  const deployment = value.deployments.find((d) => d.id === deploymentId);
  if (deployment === undefined || deployment.observed_state === 'stopped') {
    return true;
  }
  return operationId === null || operations.some((op) => op.id === operationId);
}

// SPEC §6.3, plan KTD16: wait for the live snapshot to show cleanup settled,
// bounded exactly as delete.rs does. Closing the screen abandons only this
// wait, not the accepted Stop; ordinary Delete must not follow an abort.
export function waitForStop(deploymentId: string, operationId: string | null, deadlineMs: number, signal: AbortSignal): Promise<void> {
  if (signal.aborted) {
    return Promise.reject(new DOMException('The wait was abandoned.', 'AbortError'));
  }
  return new Promise((resolve, reject) => {
    let unsubscribe: (() => void) | null = null;
    let finished = false;
    const finish = (done: () => void) => {
      if (finished) return;
      finished = true;
      clearTimeout(timer);
      signal.removeEventListener('abort', onAbort);
      unsubscribe?.();
      unsubscribe = null;
      done();
    };
    const onAbort = () => finish(() => reject(new DOMException('The wait was abandoned.', 'AbortError')));
    const remaining = Math.max(deadlineMs - Date.now() + WAIT_MARGIN_MS, WAIT_MARGIN_MS);
    const timer = setTimeout(() => finish(() => reject(new ApiError({
      status: 409,
      code: 'delete_requires_cleanup',
      message: 'Cleanup is still pending. The model has not been deleted.',
      retryable: false,
      operationId,
      details: {},
      envelope: 'none',
    }))), remaining);
    signal.addEventListener('abort', onAbort);
    unsubscribe = snapshot.subscribe((value) => {
      if (!finished && value !== null && stopSettled(value, deploymentId, operationId)) {
        // The subscription may still be starting; let it return first. An
        // abort before this continuation wins over the observed settlement.
        queueMicrotask(() => signal.aborted ? onAbort() : finish(resolve));
      }
    });
  });
}
