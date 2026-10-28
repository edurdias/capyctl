// Wire shapes of the management API reads the shell consumes.
// Snapshot: crates/capyctl-store/src/snapshot.rs (`Snapshot` and friends),
// served verbatim by `GET /management/v1/snapshot`
// (crates/capyctl-management/src/lib.rs). Integers the store keeps as i64
// counters are serialized as decimal strings; plain sizes are JSON numbers.

export type ObservedState =
  | 'uncertain'
  | 'starting'
  | 'queued'
  | 'stopping'
  | 'reconciling'
  | 'ready'
  | 'parking'
  | 'parked'
  | 'waking'
  | 'failed'
  | 'stopped';

export type EngineName = 'vllm' | 'sglang' | 'tensorfold' | 'llamacpp' | 'custom';

export interface DevelopmentControls {
  state: 'exposed' | 'not_exposed' | 'unknown';
  engine?: EngineName;
  deep_park?: 'enabled' | 'disabled';
  deep_park_source?: 'host_policy' | 'default';
  enable_sleep_mode?: boolean;
  residency?: 'restart_only' | 'host_backed' | 'deep';
  surface?: string[];
  mitigations?: string[];
  production_safe?: false;
  applies_to?: string;
  unauthenticated_local_surfaces?: {
    surface: string[];
    listener: 'loopback';
    access: 'read_only' | 'inference';
  };
}

export interface LatestOperation {
  id: string;
  kind: string;
  state: string;
  error_code?: string;
  reason?: string;
  hint?: string;
  given_up?: true;
}

export interface CheckpointDigest {
  state: 'pending' | 'recorded' | 'mismatch' | 'unusable';
  host_id: string;
  expected?: string;
  digest?: string;
  weights_bytes?: number;
  state_slot_bytes?: number;
  provisional: boolean;
  diagnostic?: string;
  provenance?: 'measured' | 'fetched' | 'declared_trusted';
}

export interface ModelSourceRecord {
  host_id: string;
  source_key: string;
  state: 'pending' | 'downloading' | 'verified' | 'failed';
  bytes_done: number;
  bytes_total: number;
  reason?: string;
  terminal?: true;
}

export interface LifecycleWindows {
  request_deadline_ms: number;
  initialize_ms: number;
  wake_ms?: number;
  stop_ms: number;
  provenance?: Record<string, 'declared' | 'derived'>;
}

export type StartupProvenance = 'declared' | 'measured' | 'default' | 'resources' | 'request' | 'whole_host';

export interface StartupReservation {
  bytes: number;
  provenance: StartupProvenance;
  device_bytes?: number;
  host_bytes?: number;
  device_provenance?: StartupProvenance;
  host_provenance?: StartupProvenance;
}

export interface StartupStatus extends StartupReservation {
  measured?: {
    host_id: string;
    installation: string;
    domain: string;
    peak_bytes: number;
    measured_at_ms: number;
    tensor_parallel?: number;
  }[];
}

export interface ParkedStatus {
  bytes: number;
  provenance: 'measured' | 'placeholder';
  measured?: { host_id: string; installation: string; domain: string; bytes: number; measured_at_ms: number }[];
  growth?: {
    instance: number;
    generation: number;
    domain: string;
    first_bytes: number;
    last_bytes: number;
    parks: number;
    limit_bytes?: number;
    state: 'within_limit' | 'past_limit' | 'stopped';
    stop_operation_id?: string;
  }[];
}

export interface SwitchStatus {
  switch_id: string;
  role: 'target' | 'victim';
  target: string;
  host?: string | null;
  victims: string[];
  phase: 'planned' | 'admission_closed' | 'released';
  explicit: boolean;
}

export interface ContextFit {
  tokens?: number;
  source: 'declared' | 'host_fixed' | 'fitted' | 'fallback' | 'on_host';
  reason?: string;
  warning?: string;
  running_limit?: number;
  streams?: { count?: number; source: 'declared' | 'default' | 'extra_args' | 'host_fixed'; reason?: string };
}

export interface ParserChoice {
  name?: string;
  source: string;
  reason?: string;
}

export interface Parsers {
  tool_call: ParserChoice;
  reasoning: ParserChoice;
  family?: string;
}

export interface HostResolutionSnapshot {
  host_id: string;
  outcome: 'resolved' | 'refused';
  diagnostic?: string | null;
  binary_version?: string | null;
  compatibility?: 'supported' | 'upgrade_recommended' | 'upgrade_required' | 'refused';
  compatibility_reason?: string | null;
}

export interface InstanceRequests {
  cancelling: string;
  cancelling_settles: 'idle_signal' | 'stop';
  settle_when_idle: string;
  settle_at_stop: string;
}

export interface InstanceLmcache {
  state: 'on' | 'off';
  reason?: string;
  message?: string;
  server?: 'starting' | 'running' | 'exited' | 'stopped';
  pool_bytes?: number;
  overhead_bytes?: number;
  device_bytes?: number;
  held_kv_bytes?: number;
  charge_bytes?: number;
  measured?: boolean;
}

export interface InstanceTopology {
  requested?: unknown;
  tensor_parallel?: number;
  devices?: string[];
}

export interface GroupMember {
  host: string;
  node_rank: number;
  role: 'head' | 'worker';
  state: 'reserved' | 'dispatching' | 'launched' | 'uncertain' | 'settled' | 'failed';
  processes: number;
  reservation?: { phase: 'cold' | 'ready' | 'parking' | 'parked' | 'wake'; bytes: string } | null;
  residency?: string | null;
  last_error?: string | null;
}

export interface InstanceSnapshot {
  index: number;
  host_id?: string | null;
  devices?: unknown;
  generation?: string | null;
  observed_state: ObservedState;
  revision?: string;
  last_error?: string;
  lifecycle: 'active' | 'retiring';
  operator_stopped: boolean;
  reservation_owner?: string | null;
  development_controls: DevelopmentControls;
  startup?: StartupReservation;
  latest_operation?: LatestOperation;
  // Flattened group status, present on multi-host group instances only.
  engine?: 'vllm' | 'sglang' | 'tensorfold';
  rendezvous?: string;
  peer_transport?: 'unauthenticated';
  plan_generation?: string;
  members?: GroupMember[];
  requests: InstanceRequests;
  lmcache?: InstanceLmcache;
  topology?: InstanceTopology | { tensor_parallel: number; pipeline_parallel: number };
  collectives?: 'loopback';
}

export interface DeploymentSnapshot {
  id: string;
  name: string;
  kind: string;
  legacy_route?: string | null;
  revision: string;
  generation: string;
  desired_state: 'ready' | 'stopped';
  observed_state: ObservedState;
  admission_enabled: boolean;
  dispatch_enabled: boolean;
  suspended: boolean;
  effective_fingerprint?: string | null;
  development_controls: DevelopmentControls;
  operator_action?: string;
  checkpoint_digest?: CheckpointDigest;
  model_sources?: ModelSourceRecord[];
  desired_instances: number;
  ready_instances: number;
  conditions: string[];
  instances: InstanceSnapshot[];
  hosts: HostResolutionSnapshot[];
  timeouts?: LifecycleWindows;
  startup?: StartupStatus;
  parked?: ParkedStatus;
  switch?: SwitchStatus;
  warm?: true;
  activation?: 'explicit';
  latest_operation?: LatestOperation;
  context?: ContextFit;
  parsers?: Parsers;
  api?: 'decisions';
  topology?: unknown;
}

export interface RouteSnapshot {
  deployment_id: string;
  route: string;
}

export interface OperationSnapshot {
  id: string;
  deployment_id?: string | null;
  action: string;
  state: string;
  has_error: boolean;
  error_code?: string;
  accepted_at: string;
}

export interface RunSnapshot {
  operation_id: string;
  deployment_id: string;
  revision: string;
  generation: string;
  action: string;
  state: string;
  deadline_ms: string;
}

export interface StepSnapshot {
  id: string;
  operation_id: string;
  ordinal: string;
  deployment_id: string;
  binding_id: string;
  state: string;
  grant_id?: string | null;
  evidence_epoch?: string | null;
}

export interface ClaimSnapshot {
  deployment_id: string;
  operation_id: string;
  revision: string;
  generation: string;
}

export interface BindingSnapshot {
  id: string;
  deployment_id: string;
  revision: string;
  incarnation: string;
  ownership: string;
  state: string;
  recorded_identities: { role: string; pid: number; boot_id: string; start_ticks: string }[];
}

export interface ReservationSnapshot {
  owner_id: string;
  phase: string;
  allocations: { domain: string; bytes: string; host_kv_bytes: string }[];
  devices: { device: string; shared: boolean }[];
}

export interface LegacyReservationSnapshot {
  owner_id: string;
  domain_id?: string | null;
  bytes: string;
  phase: string;
  exclusive_devices: string[];
}

export interface GrantSnapshot {
  id: string;
  deployment_id: string;
  operation_id: string;
  committed_epoch: string;
}

export interface HostPolicyRevision {
  host_id: string;
  revision: string;
}

export interface DomainObservation {
  domain_id: string;
  kind: string;
  observed_bytes?: string | null;
  observed_at?: string | null;
}

export interface Snapshot {
  api_version: string;
  scope: string;
  // `<database incarnation ULID>:<sequence>`; the event stream resumes after it.
  cursor: string;
  ledger_epoch: string;
  session_epoch: string;
  deployments: DeploymentSnapshot[];
  routes: RouteSnapshot[];
  operations: OperationSnapshot[];
  runs: RunSnapshot[];
  steps: StepSnapshot[];
  claims: ClaimSnapshot[];
  bindings: BindingSnapshot[];
  reservations: ReservationSnapshot[];
  legacy_reservations: LegacyReservationSnapshot[];
  grants: GrantSnapshot[];
  host_policy_revisions: HostPolicyRevision[];
  observations: DomainObservation[];
}

// Event kinds the stream carries: `EventMetadata::kind()` in
// crates/capyctl-store/src/events.rs, projected by
// crates/capyctl-management/src/events.rs.
export const EVENT_KINDS = [
  'initialize_accepted',
  'initialize_armed',
  'owned_launch_associated',
  'ready_committed',
  'initialize_uncertain',
  'initialize_expired_unarmed',
  'initialize_failed_released',
  'ordinary_cleanup_accepted',
  'ordinary_cleanup_armed',
  'ordinary_cleanup_completed',
  'ordinary_cleanup_expired_unarmed',
  'ordinary_unarmed_stop_accepted',
  'ordinary_unarmed_stop_completed',
  'park_accepted',
  'park_armed',
  'parked_committed',
  'park_refused',
  'park_uncertain',
  'restore_accepted',
  'restore_armed',
  'restored_committed',
  'restore_refused',
  'restore_uncertain',
  'residency_cancelled',
  'switch_planned',
  'switch_admission_closed',
  'switch_released',
  'switch_completed',
  'switch_failed',
  'managed_configuration_accepted',
  'deployment_deleted',
  'coordinator_session_started',
  'host_resource_policy_bootstrapped',
  'host_resource_policy_updated',
  'host_resource_policy_migrated',
  'installation_drift_flagged',
  'host_revoked',
  'host_recovery_invited',
  'host_recovered',
  'host_drain_intent_expired',
  'request_cancellation_acknowledged',
  'lmcache_off',
  'lmcache_server_exited',
  'topology_auto_changed',
  'instance_restarted_for_held_requests',
] as const;

export type EventKind = (typeof EVENT_KINDS)[number];

// One SSE frame's `data`: every integer is a decimal string.
export interface ManagementEventData {
  api_version: string;
  recorded_at_ms: string;
  deployment_id: string | null;
  operation_id: string | null;
  payload: Record<string, unknown>;
}

export interface ManagementEvent {
  cursor: string;
  kind: EventKind;
  data: ManagementEventData;
}

// The terminal frame the stream sends before closing after headers
// (crates/capyctl-management/src/events.rs `Failure::terminal`).
export const STREAM_ERROR_EVENT = 'management_stream_error';

export interface StreamErrorData {
  api_version: string;
  error: { code: string; resnapshot_required: boolean; retryable: boolean };
}
