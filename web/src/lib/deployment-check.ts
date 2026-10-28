import { ApiError, request } from './api';
import { lostResponse, type Receipt } from './operate';
import { DEPLOYMENTS } from './setup-endpoints';
import type { DraftError } from './deployment-draft';

// ADR 0040 §9: `POST /management/v1/deployments/check` answers the
// normalized config, the emitted YAML, the parser's errors and a fit verdict.
// It stores nothing.
export type Verdict = 'fits' | 'needs_room' | 'does_not_fit' | 'unknown';

export interface Fit {
  verdict: Verdict;
  // Built on file sizes rather than a measurement.
  estimated: boolean;
  host_id?: string;
  // `does_not_fit`: the limit, worded as the start refusal names it.
  limit?: string;
  // `unknown`: host_offline, no_observations or weights_unknown.
  reason?: string;
}

export interface CheckResult {
  config: Record<string, unknown> | null;
  yaml: string | null;
  errors: DraftError[];
  fit: Fit;
}

export function checkDeployment(yaml: string, signal: AbortSignal): Promise<CheckResult> {
  return request<CheckResult>(DEPLOYMENTS.check, { method: 'POST', body: { yaml }, signal });
}

async function command(path: string, method: 'POST' | 'PUT', body: unknown, key: string): Promise<Receipt & { revision?: string }> {
  try {
    return await request(path, { method, body, headers: { 'Idempotency-Key': key } });
  } catch (error) {
    throw error instanceof ApiError ? error : lostResponse();
  }
}

// SPEC §14: the model is added stopped (`activate: false`), with the config
// the check returned pinned.
export function createDeployment(config: Record<string, unknown>, key: string) {
  return command(DEPLOYMENTS.create, 'POST', { config, activate: false }, key);
}

// SPEC §14: a revision-aware replace; a stale revision is refused
// `revision_conflict`.
export function replaceDeployment(id: string, config: Record<string, unknown>, expectedRevision: number, key: string) {
  return command(DEPLOYMENTS.replace(id), 'PUT', { config, expected_revision: expectedRevision }, key);
}
