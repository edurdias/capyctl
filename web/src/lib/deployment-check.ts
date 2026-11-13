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
  estimated: boolean | null;
  // C1: preserve the same limit and reason a start refusal names.
  limit: Record<string, unknown> | string | null;
  reason: string | null;
}

// C1: the verdict is top-level, not a nested `fit`. Its config/YAML belong
// to the first fitting host (or the first host if none fits).
export interface CheckResult extends Fit {
  api_version: string;
  config: Record<string, unknown> | null;
  yaml: string | null;
  errors: DraftError[];
  hosts: (Fit & { host_id: string; estimated: boolean; config?: Record<string, unknown> })[];
}

export function checkDeployment(yaml: string, signal: AbortSignal): Promise<CheckResult> {
  return request<CheckResult>(DEPLOYMENTS.check, { method: 'POST', body: { yaml }, signal });
}

export function checkErrors(error: ApiError | null): DraftError[] {
  if (error?.code !== 'invalid_config' || typeof error.body !== 'object' || error.body === null || !('errors' in error.body)) {
    return [];
  }
  return Array.isArray(error.body.errors)
    ? error.body.errors.filter((entry): entry is DraftError => typeof entry === 'object' && entry !== null && typeof entry.message === 'string')
    : [];
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
