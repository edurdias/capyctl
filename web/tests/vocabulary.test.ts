// ADR 0040 (plan KTD15, U12): refusal and state wording.
import { describe, expect, it } from 'vitest';
import { parseError } from '../src/lib/api';
import type { ObservedState } from '../src/lib/types';
import { describeRefusal, MANAGEMENT_REFUSALS, ROUTER_REFUSALS, STATE_WORDS, stateWord } from '../src/lib/vocabulary';

// Every code the management envelope carries
// (crates/capyctl-management/src), plus KTD5's origin_refused.
const MANAGEMENT_CODES = [
  'unauthenticated',
  'origin_refused',
  'invalid_request',
  'unsupported_media_type',
  'identity_conflict',
  'host_not_revoked',
  'queue_full',
  'not_found',
  'method_not_allowed',
  'internal',
  'invalid_cursor',
  'cursor_expired',
  'invalid_config',
  'body_too_large',
  'deadline_exceeded',
  'unsupported_capability',
  'revision_conflict',
  'idempotency_conflict',
  'route_conflict',
  'runtime_retained',
  'delete_requires_cleanup',
  'reconciliation_required',
  'checkpoint_digest_pending',
  'checkpoint_mismatch',
  'checkpoint_unusable',
  'model_source_pending',
  'model_source_failed',
  'host_policy_denied',
  'capacity_blocked',
  'host_ineligible',
  'profile_not_published',
  'startup_requires_empty_host',
  'switch_failed',
  'still_stopping',
  'lifecycle_conflict',
  'host_not_found',
  'forbidden',
  'observation_stale',
  'snapshot_unavailable',
];

// Every code the router envelope carries (crates/capyctl-router/src/chat.rs).
const ROUTER_CODES = [
  'unauthorized',
  'request_too_large',
  'invalid_request',
  'internal',
  'unknown_model',
  'queue_full',
  'unsupported_parameter',
  'cache_salt_unsupported',
  'api_mismatch',
  'unsupported',
  'insufficient_resources',
  'conflict',
  'deployment_stopped',
  'deployment_inactive',
  'activation_uncertain',
  'activation_failed',
  'unavailable',
  'shutting_down',
  'engine_rejected',
  'engine_error',
];

// SPEC R15: internal terms never reach the owner.
const INTERNAL_TERMS = /\b(lease|leases|epoch|epochs|reservation|reservations|fencing|fenced)\b/i;

describe('refusal vocabulary', () => {
  it.each(MANAGEMENT_CODES)('maps management code %s to a reason and an action', (code) => {
    const text = describeRefusal(parseError(409, { api_version: '1', error: { code, message: 'server text', retryable: false, operation_id: null, details: {} } }));
    expect(text.reason).not.toBe('');
    expect(text.reason).not.toBe('server text');
    expect(text.action).toBeTruthy();
    expect(text.details).toBe('server text');
  });

  it.each(ROUTER_CODES)('maps router code %s to a reason and an action', (code) => {
    const text = describeRefusal(parseError(503, { code, message: 'server text', retryable: true }));
    expect(text.reason).not.toBe('');
    expect(text.reason).not.toBe('server text');
    expect(text.action).toBeTruthy();
  });

  it('lists exactly the codes the server sends', () => {
    expect(Object.keys(MANAGEMENT_REFUSALS).sort()).toEqual([...MANAGEMENT_CODES].sort());
    expect(Object.keys(ROUTER_REFUSALS).sort()).toEqual([...ROUTER_CODES].sort());
  });

  it('falls back to the server message for an unknown code in either envelope', () => {
    const management = describeRefusal(parseError(409, { api_version: '1', error: { code: 'brand_new', message: 'The server explains.', retryable: false, operation_id: null, details: {} } }));
    expect(management).toEqual({ reason: 'The server explains.', action: null, details: 'The server explains.' });
    const router = describeRefusal(parseError(400, { code: 'brand_new', message: 'The router explains.' }));
    expect(router).toEqual({ reason: 'The router explains.', action: null, details: 'The router explains.' });
  });

  it('keeps internal terms out of every reason and action', () => {
    for (const refusal of [...Object.values(MANAGEMENT_REFUSALS), ...Object.values(ROUTER_REFUSALS)]) {
      expect(refusal.reason).not.toMatch(INTERNAL_TERMS);
      expect(refusal.action).not.toMatch(INTERNAL_TERMS);
    }
  });
});

describe('state words', () => {
  it('uses the guide words for every snapshot state', () => {
    const expected: Record<ObservedState, string> = {
      ready: 'ready',
      parked: 'parked',
      stopped: 'stopped',
      failed: 'failed',
      starting: 'starting',
      waking: 'starting',
      queued: 'starting',
      parking: 'parking',
      stopping: 'stopping',
      uncertain: 'checking',
      reconciling: 'checking',
    };
    expect(STATE_WORDS).toEqual(expected);
  });

  it('reads a suspended deployment as stopped', () => {
    expect(stateWord('ready', true)).toBe('stopped');
  });
});
