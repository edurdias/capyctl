import { ApiError, request } from './api';
import { newIdempotencyKey } from './idempotency';
import { lostResponse } from './operate';
import { HOST_ADMIN, INVITATIONS } from './setup-endpoints';

// ADR 0040 §§10, 11: host administration, with the shapes
// `capyctl_agent::host_admin` answers.

// host_admin/engines.rs `detected`.
export interface Candidate {
  engine: string;
  version: string | null;
  custom: boolean;
  entry: string;
  source: string;
}

// host_admin/model_store.rs `list_copies`.
export interface ModelCopy {
  key: string;
  bytes: number;
  referenced: boolean;
}

// host_admin/custom.rs `CustomPreview`: the executable and its argument
// template, the summary `engine add --custom` prints, and the digest the
// confirmation carries.
export interface CustomPreview {
  command: string[];
  summary: string;
  digest: string;
}

export interface CustomRequest {
  operation: 'add' | 'change';
  name: string;
  executable: string;
  manifest: string;
}

// enrollment.rs `invite`: the invitation, which is the join file itself.
export interface Invitation {
  host_name: string;
  expires_unix: number;
  [field: string]: unknown;
}

// A mutation: one request identity, and a request that went out but never
// came back reads as a lost answer (plan KTD16).
async function mutate<T>(path: string, body: unknown): Promise<T> {
  try {
    return await request<T>(path, { method: 'POST', body, headers: { 'Idempotency-Key': newIdempotencyKey() } });
  } catch (error) {
    throw error instanceof ApiError ? error : lostResponse();
  }
}

export async function detectEngines(host: string, paths: string[]): Promise<Candidate[]> {
  return (await request<{ candidates: Candidate[] }>(HOST_ADMIN.detected(host, paths))).candidates;
}

// KTD11: a built-in add takes a detected candidate.
export function addEngine(host: string, candidate: Candidate, name: string): Promise<unknown> {
  return mutate(HOST_ADMIN.addEngine(host), { path: candidate.entry, ...(name ? { name } : {}) });
}

// Answers 202; the engines list then shows the profile `retiring`.
export function removeEngine(host: string, name: string): Promise<unknown> {
  return mutate(HOST_ADMIN.removeEngine(host, name), {});
}

export async function modelCopies(host: string): Promise<ModelCopy[]> {
  return (await request<{ copies: ModelCopy[] }>(HOST_ADMIN.modelStore(host))).copies;
}

export function pruneCopies(host: string): Promise<{ removed: { key: string; bytes: number }[]; removed_bytes: number }> {
  return mutate(HOST_ADMIN.prune(host), { apply: true });
}

export function hostConfig(host: string): Promise<unknown> {
  return request<unknown>(HOST_ADMIN.config(host));
}

export function previewCustom(host: string, custom: CustomRequest): Promise<CustomPreview> {
  return mutate(HOST_ADMIN.customPreview(host), custom);
}

// ADR 0040 §11: the preview's digest and the name the owner typed.
export function confirmCustom(host: string, custom: CustomRequest, digest: string, typedName: string): Promise<unknown> {
  return mutate(HOST_ADMIN.customConfirm(host), { ...custom, digest, confirm_name: typedName });
}

// SPEC §4.1: the lifetime `capyctl invite host` asks for.
const INVITATION_SECONDS = 3600;

export function invite(hostName: string): Promise<Invitation> {
  return mutate(INVITATIONS, { host_name: hostName, lifetime_seconds: INVITATION_SECONDS });
}
