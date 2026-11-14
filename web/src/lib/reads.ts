import { ApiError, request } from './api';
import { DEPLOYMENTS } from './setup-endpoints';

// Wire shapes of the management reads the watch views use (SPEC R10, R12,
// R13). Each one names the handler that writes it.

// crates/capyctl-controller/src/agent_sessions.rs `DomainView`; a standalone
// role reports the same fields (crates/capyctl-management/src/hosts.rs).
export interface MemoryDomain {
  domain_id: string;
  kind: string;
  capacity_bytes: number;
  available_bytes: number;
  device_id?: string;
  observed_at_unix_ms?: number;
  observed_at_unix?: number;
}

export interface HostSession {
  online?: boolean;
  unresponsive?: boolean;
  drain_pending?: boolean;
  domains?: MemoryDomain[];
  memory_warnings?: string[];
}

// `GET /management/v1/hosts` (hosts.rs `hosts`, `standalone_hosts`).
export interface HostRow {
  host_id: string;
  name: string;
  revoked: boolean;
  online: boolean;
  eligible: boolean;
  session: HostSession | null;
  binary_version: string | null;
  compatibility: 'supported' | 'upgrade_recommended' | 'upgrade_required' | 'refused' | null;
  compatibility_reason: string | null;
}

// `GET /management/v1/engines` (hosts.rs `engine_row`).
export interface EngineRow {
  host_id: string;
  host: string;
  online: boolean;
  profile: string;
  engine: string;
  version: string;
  custom: boolean;
  retiring: boolean;
  deployments: string[];
}

// `GET /management/v1/metrics/load` (crates/capyctl-router/src/capacity.rs).
export interface LoadInstance {
  index: number;
  host_id: string | null;
  router_in_flight: number;
  sample: {
    fresh: boolean;
    ingress_in_flight: number | null;
    engine: { running: number | null; waiting: number | null; kv_usage_ppm: number | null } | null;
  } | null;
}

export interface LoadDeployment {
  deployment_id: string;
  name: string;
  router: { in_flight: number; in_flight_limit: number; waiting: number; waiting_limit: number };
  instances: LoadInstance[];
}

export interface LoadReport {
  deployments: LoadDeployment[];
}

// `GET /management/v1/metrics/latency` (crates/capyctl-router/src/timing.rs
// `latency_report`): `source` separates CapyCTL's timings from the engine's.
export interface LatencySeries {
  name: string;
  tier: 'router' | 'ingress' | 'engine';
  source: 'capyctl' | 'engine';
  count: number;
  p50_seconds: number | null;
  p95_seconds: number | null;
  // The histogram's sum divided by its count: exact, not estimated inside a
  // bucket as the percentiles are.
  mean_seconds?: number | null;
}

export interface LatencyReport {
  deployments: {
    deployment_id: string;
    instances: { instance: number | null; generation?: number | null; host_id: string | null; engine: string | null; series: LatencySeries[] }[];
  }[];
}

// `GET /management/v1/deployments/{id}/engine-log` (engine_log.rs).
export interface EngineLog {
  instance: number;
  host_id: string;
  truncated: boolean;
  text: string;
}

// `GET /management/v1/deployments/{id}/effective-config`
// (configuration.rs `effective_configuration`).
export interface EffectiveConfig {
  deployment_id: string;
  effective: unknown;
  hosts: { host_id: string; outcome: string; effective: unknown; diagnostic: string | null }[];
}

// SPEC R10, R15: a host's health in one word.
export function hostHealth(host: HostRow): string {
  if (host.revoked) {
    return 'revoked';
  }
  if (!host.online) {
    return 'offline';
  }
  if (host.session?.unresponsive) {
    return 'unresponsive';
  }
  if (host.session?.drain_pending) {
    return 'draining';
  }
  if (host.compatibility === 'upgrade_required' || host.compatibility === 'refused') {
    return 'needs an upgrade';
  }
  return 'online';
}

export async function readHosts(): Promise<HostRow[]> {
  return (await request<{ hosts: HostRow[] }>('/management/v1/hosts')).hosts;
}

export async function readEngines(): Promise<EngineRow[]> {
  return (await request<{ engines: EngineRow[] }>('/management/v1/engines')).engines;
}

export function readLoad(deployment?: string): Promise<LoadReport> {
  const query = deployment === undefined ? '' : `?deployment=${encodeURIComponent(deployment)}`;
  return request<LoadReport>(`/management/v1/metrics/load${query}`);
}

export function readLatency(deployment: string): Promise<LatencyReport> {
  return request<LatencyReport>(`/management/v1/metrics/latency?deployment=${encodeURIComponent(deployment)}`);
}

export function readEffectiveConfig(deployment: string): Promise<EffectiveConfig> {
  return request<EffectiveConfig>(DEPLOYMENTS.effectiveConfig(deployment));
}

export function readEngineLog(deployment: string, instance: number, kib = 64): Promise<EngineLog> {
  return request<EngineLog>(
    `/management/v1/deployments/${encodeURIComponent(deployment)}/engine-log?instance=${instance}&kib=${kib}`,
  );
}

export type RoleKind = 'standalone' | 'server';

// Only the standalone role serves `GET /management/v1/installation`
// (crates/capyctl-management/src/installation.rs, mounted in roles.rs); a
// server answers it `not_found`.
export async function readRoleKind(): Promise<RoleKind> {
  try {
    await request<unknown>('/management/v1/installation');
    return 'standalone';
  } catch (error) {
    if (error instanceof ApiError && error.status === 404) {
      return 'server';
    }
    throw error;
  }
}
