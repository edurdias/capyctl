/// <reference types="node" />
// ADR 0040 (plan KTD15, U12): refusal and state wording.
import { readdirSync, readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';
import { describe, expect, it } from 'vitest';
import { parseError } from '../src/lib/api';
import type { ObservedState } from '../src/lib/types';
import { describeRefusal, MANAGEMENT_REFUSALS, ROUTER_REFUSALS, STATE_WORDS, stateWord } from '../src/lib/vocabulary';

// Read Rust directly: these files are outside Vite's web-root allowlist.
function rustSources(directory: URL): string[] {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const path = new URL(entry.name + (entry.isDirectory() ? '/' : ''), directory);
    if (entry.isDirectory()) return rustSources(path);
    return entry.isFile() && entry.name.endsWith('.rs') ? [readFileSync(path, 'utf8')] : [];
  });
}

const sourceRoot = pathToFileURL(`${process.cwd()}/../crates/`);
const agentSourceDirectory = new URL('capyctl-agent/src/', sourceRoot);
const managementSources = [
  ...rustSources(new URL('capyctl-management/src/', sourceRoot)),
  ...rustSources(new URL('host_admin/', agentSourceDirectory)),
  ...readdirSync(agentSourceDirectory, { withFileTypes: true })
    .filter((entry) => entry.isFile() && /^host_admin.*\.rs$/.test(entry.name))
    .map((entry) => readFileSync(new URL(entry.name, agentSourceDirectory), 'utf8')),
];

function managementCodes(sources: string[]): string[] {
  const codes = new Set<string>();
  const patterns = [
    /StatusCode::[A-Z_]+\s*,\s*"([a-z][a-z_:]+)"/g,
    /(?:"code"|\bcode)\s*:\s*"([a-z][a-z_:]+)"/g,
    /\b(?:error|refusal)\(\s*"([a-z][a-z_:]+)"/g,
    // Host-admin wire error constants; unrelated string constants do not match.
    /\bconst\s+(?:CONFIRMATION_\w+|DEADLINE_EXCEEDED|HOST_ADMIN_BUSY|REPLY_TOO_LARGE)\s*:\s*&str\s*=\s*"([a-z][a-z_:]+)"/g,
    // The host-admin closed-code conversion passes through these wire codes.
    /"([a-z][a-z_:]+)"\s*=>\s*"\1"/g,
  ];
  for (const source of sources) {
    const production = source.split('#[cfg(test)]')[0];
    for (const pattern of patterns) {
      for (const match of production.matchAll(pattern)) codes.add(match[1]);
    }
  }
  return [...codes].sort();
}

// These codes arrive through typed controller failures, not literal management
// constructors. Keep this small boundary explicit rather than duplicating all codes.
const MANAGEMENT_CODES = [...new Set([
  ...managementCodes(managementSources),
  'host_capability_missing:host_admin',
  'host_upgrade_required',
  'outcome_unknown',
])].sort();

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

  it('covers the Rust management and host-admin error constructors', () => {
    expect(managementSources.length).toBeGreaterThan(0);
    expect(MANAGEMENT_REFUSALS).toHaveProperty('shutting_down');
    expect(MANAGEMENT_CODES).toContain('profile_exists');
    for (const code of MANAGEMENT_CODES) expect(MANAGEMENT_REFUSALS).toHaveProperty(code);
    expect(Object.keys(ROUTER_REFUSALS).sort()).toEqual([...ROUTER_CODES].sort());
  });

  it('extracts error codes without treating unrelated Rust strings as refusals', () => {
    expect(managementCodes([`
      let engine = "not_a_refusal";
      let details = json!({"name": "another_unrelated_value"});
      error(StatusCode::CONFLICT, "profile_in_use", false);
      error("engine_unsupported", "explanation");
      Failure { code: "observation_stale".into(), message: "details".into() };
      json!({"code": "invalid_config"});
      pub const CONFIRMATION_STALE: &str = "confirmation_stale";
      pub const OTHER: &str = "not_an_error";
      #[cfg(test)]
      fn test_only() { error("test_only_code", "details"); }
    `])).toEqual([
      'confirmation_stale', 'engine_unsupported', 'invalid_config',
      'observation_stale', 'profile_in_use',
    ]);
  });

  it.each([
    'shutting_down', 'profile_in_use', 'profile_exists', 'publish_rejected',
    'confirmation_required', 'custom_manifest_invalid', 'engine_unsupported',
    'host_admin_busy', 'reply_too_large', 'host_upgrade_required',
    'observation_stale', 'outcome_unknown', 'confirmation_stale',
  ])('provides plain reasons and actions for host-admin code %s', (code) => {
    expect(MANAGEMENT_REFUSALS[code]?.reason).toBeTruthy();
    expect(MANAGEMENT_REFUSALS[code]?.action).toBeTruthy();
  });

  it('does not promise that retrying a timed-out host change is safe', () => {
    const { reason, action } = MANAGEMENT_REFUSALS.deadline_exceeded;
    expect(reason).toContain('did not answer');
    expect(action).toContain('may already have happened');
    expect(action).toContain('Check the current state');
    expect(action).not.toMatch(/does not repeat|safe.*retry|retry safely/i);
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
