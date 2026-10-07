//! The F0 durable-store schema (design §4), verbatim.
//!
//! Tables not yet exercised by acceptance are created now so the v1
//! migration is the single, forward-only baseline.

/// v1 DDL: deployments, operations, generation_history, owners,
/// reservations, domains, hosts, journal_entries, schema_migrations.
pub const SCHEMA_V1: &str = r#"
CREATE TABLE deployments(
    id TEXT PRIMARY KEY,
    name TEXT UNIQUE NOT NULL,
    kind TEXT NOT NULL,
    route_model_id TEXT,
    desired_state TEXT NOT NULL,
    admission_enabled INTEGER NOT NULL,
    suspended INTEGER NOT NULL,
    current_generation INTEGER NOT NULL,
    schema_version INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

-- revision preconditions are intentionally absent until the
-- revision-aware update design exists (SPEC §19 defers it)
CREATE TABLE operations(
    id TEXT PRIMARY KEY,
    deployment_id TEXT REFERENCES deployments,
    kind TEXT NOT NULL,
    state TEXT NOT NULL,
    error_code TEXT,
    idempotency_key TEXT UNIQUE,
    accepted_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

CREATE TABLE generation_history(
    deployment_id TEXT NOT NULL REFERENCES deployments,
    generation INTEGER NOT NULL,
    started_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    ended_at TEXT,
    outcome TEXT
);

CREATE TABLE owners(
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    deployment_id TEXT NULL REFERENCES deployments
);

CREATE TABLE reservations(
    owner_id TEXT NOT NULL REFERENCES owners,
    domain_id TEXT,
    bytes INTEGER NOT NULL,
    phase TEXT NOT NULL,
    exclusive_devices TEXT -- JSON array
);

CREATE TABLE domains(
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL, -- system | device_memory | filesystem | remote_storage
    observed_bytes INTEGER NULL,
    observed_at TEXT
);

CREATE TABLE hosts(
    id TEXT PRIMARY KEY,
    name TEXT,
    state TEXT
); -- inventory skeleton; F3 enrolls

CREATE TABLE journal_entries(
    id TEXT PRIMARY KEY,
    host_id TEXT,
    operation_id TEXT,
    state TEXT,
    evidence TEXT,
    recorded_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
); -- no inference bodies, ever

CREATE TABLE schema_migrations(version INTEGER PRIMARY KEY);
"#;

/// v2: the observed half of the lifecycle state. v1 shipped before any
/// writer needed it; the controller operation engine records it (T12).
pub const SCHEMA_V2: &str =
    "ALTER TABLE deployments ADD COLUMN observed_state TEXT NOT NULL DEFAULT 'stopped';";

pub const SCHEMA_V3: &str = r#"
ALTER TABLE deployments ADD COLUMN revision INTEGER NOT NULL DEFAULT 1 CHECK(revision >= 1);
CREATE TABLE resource_ledger_meta(
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    epoch INTEGER NOT NULL CHECK(epoch >= 0)
);
INSERT INTO resource_ledger_meta(singleton, epoch) VALUES (1, 0);
CREATE TABLE resource_owners(
    owner_id TEXT PRIMARY KEY REFERENCES deployments(id),
    footprint_json TEXT NOT NULL
);
CREATE TABLE resource_grants(
    id TEXT PRIMARY KEY,
    deployment_id TEXT NOT NULL REFERENCES deployments(id),
    operation_id TEXT NOT NULL REFERENCES operations(id),
    request_json TEXT NOT NULL,
    committed_epoch INTEGER NOT NULL UNIQUE CHECK(committed_epoch > 0)
);
"#;

pub const SCHEMA_V4: &str = r#"
ALTER TABLE deployments ADD COLUMN dispatch_enabled INTEGER NOT NULL DEFAULT 0 CHECK(dispatch_enabled IN (0,1));
CREATE TABLE coordinator_session(
    singleton INTEGER PRIMARY KEY CHECK(singleton=1),
    epoch INTEGER NOT NULL CHECK(epoch>=0),
    session_id TEXT NOT NULL
);
INSERT INTO coordinator_session(singleton,epoch,session_id) VALUES (1,0,'');
CREATE TABLE request_leases(
    id TEXT PRIMARY KEY,
    deployment_id TEXT NOT NULL REFERENCES deployments(id),
    revision INTEGER NOT NULL CHECK(revision>=1),
    generation INTEGER NOT NULL CHECK(generation>=1),
    session_id TEXT NOT NULL,
    disposition TEXT NOT NULL CHECK(disposition IN ('inflight','uncertain'))
);
CREATE INDEX request_leases_deployment ON request_leases(deployment_id);
"#;

pub const SCHEMA_V5: &str = r#"
CREATE TABLE runtime_bindings(
  id TEXT PRIMARY KEY,
  deployment_id TEXT NOT NULL REFERENCES deployments(id),
  revision INTEGER NOT NULL CHECK(revision>0),
  incarnation TEXT NOT NULL UNIQUE,
  ownership TEXT NOT NULL CHECK(ownership IN ('managed','attached')),
  binding_json TEXT NOT NULL,
  identities_json TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('reserved','live','uncertain','released'))
);
CREATE UNIQUE INDEX one_retained_binding ON runtime_bindings(deployment_id)
  WHERE state!='released';
CREATE TABLE endpoint_leases(
  host TEXT NOT NULL,
  port INTEGER NOT NULL CHECK(port BETWEEN 1 AND 65535),
  binding_id TEXT NOT NULL REFERENCES runtime_bindings(id),
  PRIMARY KEY(host,port)
);
CREATE TABLE lifecycle_runs(
  operation_id TEXT PRIMARY KEY REFERENCES operations(id),
  deployment_id TEXT NOT NULL REFERENCES deployments(id),
  revision INTEGER NOT NULL CHECK(revision>0),
  generation INTEGER NOT NULL CHECK(generation>0),
  session_id TEXT NOT NULL,
  action TEXT NOT NULL CHECK(action IN ('activate','park','stop','prepare','reconcile')),
  state TEXT NOT NULL CHECK(state IN ('queued','running','uncertain','succeeded','failed')),
  deadline_ms INTEGER NOT NULL,
  plan_json TEXT NOT NULL
);
CREATE UNIQUE INDEX one_activation ON lifecycle_runs(deployment_id,revision,generation)
  WHERE action='activate' AND state IN ('queued','running','uncertain');
CREATE TABLE lifecycle_claims(
  deployment_id TEXT PRIMARY KEY REFERENCES deployments(id),
  operation_id TEXT NOT NULL REFERENCES lifecycle_runs(operation_id),
  revision INTEGER NOT NULL CHECK(revision>0),
  generation INTEGER NOT NULL CHECK(generation>0)
);
CREATE TABLE lifecycle_steps(
  id TEXT PRIMARY KEY,
  operation_id TEXT NOT NULL REFERENCES lifecycle_runs(operation_id),
  ordinal INTEGER NOT NULL CHECK(ordinal>=0),
  deployment_id TEXT NOT NULL REFERENCES deployments(id),
  binding_id TEXT NOT NULL REFERENCES runtime_bindings(id),
  session_id TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('planned','armed','uncertain','completed','cancelled')),
  step_json TEXT NOT NULL,
  grant_id TEXT UNIQUE REFERENCES resource_grants(id),
  UNIQUE(operation_id,ordinal)
);
CREATE TABLE lifecycle_evidence(
  step_id TEXT PRIMARY KEY REFERENCES lifecycle_steps(id),
  evidence_json TEXT NOT NULL,
  committed_epoch INTEGER NOT NULL CHECK(committed_epoch>=0)
);
"#;

pub const SCHEMA_V6: &str = r#"
CREATE TABLE deployment_routes(
  route TEXT PRIMARY KEY,
  deployment_id TEXT NOT NULL REFERENCES deployments(id)
);
CREATE TABLE command_receipts(
  principal_id TEXT NOT NULL,
  command_scope TEXT NOT NULL,
  idempotency_key TEXT NOT NULL,
  request_hash TEXT NOT NULL,
  operation_id TEXT NOT NULL REFERENCES operations(id),
  response_json TEXT NOT NULL,
  PRIMARY KEY(principal_id,command_scope,idempotency_key)
);
CREATE TABLE effective_revisions(
  deployment_id TEXT NOT NULL REFERENCES deployments(id),
  revision INTEGER NOT NULL CHECK(revision>0),
  effective_json TEXT NOT NULL,
  fingerprint TEXT NOT NULL,
  PRIMARY KEY(deployment_id,revision)
);
CREATE TABLE host_resource_policies(
  host_id TEXT PRIMARY KEY,
  revision INTEGER NOT NULL CHECK(revision>0),
  policy_json TEXT NOT NULL
);
CREATE TABLE host_qualification_policies(
  host_id TEXT PRIMARY KEY,
  revision INTEGER NOT NULL CHECK(revision>0),
  policy_json TEXT NOT NULL
);
CREATE TABLE qualification_runs(
  id TEXT PRIMARY KEY,
  host_id TEXT NOT NULL REFERENCES host_qualification_policies(host_id),
  deployment_id TEXT NOT NULL REFERENCES deployments(id),
  revision INTEGER NOT NULL CHECK(revision>0),
  binding_id TEXT NOT NULL REFERENCES runtime_bindings(id),
  incarnation TEXT NOT NULL,
  operation_id TEXT NOT NULL REFERENCES operations(id),
  principal_id TEXT NOT NULL,
  recipe_digest TEXT NOT NULL,
  authorization_json TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN
    ('accepted','running','passed','failed','uncertain','aborted','expired')),
  deadline_ms INTEGER NOT NULL,
  requests_used INTEGER NOT NULL DEFAULT 0 CHECK(requests_used>=0),
  cleanup_state TEXT NOT NULL DEFAULT 'retained'
    CHECK(cleanup_state IN ('retained','verified_gone')),
  cleanup_step_id TEXT REFERENCES lifecycle_steps(id),
  CHECK((cleanup_state='retained' AND cleanup_step_id IS NULL) OR
        (cleanup_state='verified_gone' AND cleanup_step_id IS NOT NULL))
);
CREATE TABLE qualifications(
  id TEXT PRIMARY KEY,
  source_run_id TEXT NOT NULL UNIQUE REFERENCES qualification_runs(id),
  recipe_fingerprint TEXT NOT NULL,
  record_json TEXT NOT NULL
);
CREATE INDEX qualifications_recipe ON qualifications(recipe_fingerprint);
CREATE TABLE qualification_evidence_refs(
  id TEXT PRIMARY KEY,
  run_id TEXT NOT NULL REFERENCES qualification_runs(id),
  case_id TEXT NOT NULL,
  evidence_digest TEXT NOT NULL,
  metadata_json TEXT NOT NULL,
  UNIQUE(run_id,case_id,evidence_digest)
);
"#;

pub const SCHEMA_V10: &str = r#"
CREATE TABLE qualification_ready_probes(
  run_id TEXT NOT NULL REFERENCES qualification_runs(id),
  case_id TEXT NOT NULL,
  parent_operation_id TEXT NOT NULL REFERENCES lifecycle_runs(operation_id),
  parent_step_id TEXT NOT NULL UNIQUE REFERENCES lifecycle_steps(id),
  probe_step_id TEXT NOT NULL UNIQUE REFERENCES lifecycle_steps(id),
  linkage_json TEXT NOT NULL CHECK(length(CAST(linkage_json AS BLOB)) <= 1048576),
  PRIMARY KEY(run_id,case_id),
  CHECK(parent_step_id != probe_step_id)
);
CREATE TABLE qualification_request_attempts(
  run_id TEXT NOT NULL REFERENCES qualification_runs(id),
  case_id TEXT NOT NULL,
  item_ordinal INTEGER NOT NULL CHECK(item_ordinal BETWEEN 0 AND 4095),
  subcheck_id TEXT NOT NULL,
  request_operation_id TEXT NOT NULL UNIQUE REFERENCES operations(id),
  lease_id TEXT NOT NULL UNIQUE,
  principal_id TEXT NOT NULL,
  command_scope TEXT NOT NULL,
  idempotency_key TEXT NOT NULL,
  parent_operation_id TEXT REFERENCES operations(id),
  child_step_id TEXT UNIQUE REFERENCES lifecycle_steps(id),
  receipt_json TEXT NOT NULL CHECK(length(CAST(receipt_json AS BLOB)) <= 1048576),
  PRIMARY KEY(run_id,case_id,item_ordinal,subcheck_id),
  FOREIGN KEY(principal_id,command_scope,idempotency_key) REFERENCES command_receipts(principal_id,command_scope,idempotency_key),
  CHECK((parent_operation_id IS NULL) = (child_step_id IS NULL)),
  CHECK(request_operation_id != parent_operation_id)
);
CREATE TABLE qualification_request_results(
  request_operation_id TEXT PRIMARY KEY REFERENCES qualification_request_attempts(request_operation_id),
  evidence_json TEXT NOT NULL CHECK(length(CAST(evidence_json AS BLOB)) <= 1048576),
  committed_epoch INTEGER NOT NULL
);
CREATE TABLE qualification_parked_status(
  parent_step_id TEXT PRIMARY KEY REFERENCES lifecycle_steps(id),
  evidence_json TEXT NOT NULL CHECK(length(CAST(evidence_json AS BLOB)) <= 1048576),
  committed_epoch INTEGER NOT NULL
);
"#;

/// SPEC §6.3 requires an administrative stop to suspend automatic activation, and
/// requires an automatic idle stop not to. `suspended` cannot carry that intent: its
/// nine readers all use it to mean "eligible to proceed", so writing it on a stop
/// breaks completion, replay and expiry for the very operation that wrote it. This
/// column carries the intent alone, and nothing else reads it.
pub const SCHEMA_V11: &str =
    "ALTER TABLE deployments ADD COLUMN admin_stopped INTEGER NOT NULL DEFAULT 0;";

/// ADR 0011 decision 5: a deployment's failed attempts are counted against the exact
/// configuration that failed. A new revision is a new configuration and starts fresh,
/// so the key is the fence rather than the deployment alone.
pub const SCHEMA_V12: &str = r#"
CREATE TABLE deployment_attempts(
  deployment_id TEXT NOT NULL REFERENCES deployments(id),
  revision INTEGER NOT NULL CHECK(revision > 0),
  generation INTEGER NOT NULL CHECK(generation > 0),
  attempts INTEGER NOT NULL CHECK(attempts >= 0),
  last_attempt_ms INTEGER NOT NULL CHECK(last_attempt_ms >= 0),
  PRIMARY KEY(deployment_id, revision, generation)
);
"#;

/// ADR 0011: qualification is not a capyctl concept. The tables that held candidate
/// runs, their catalog, evidence, probes, budgets and parked-status records are
/// dropped in foreign-key order. Nothing wrote them outside tests; a v12 store from
/// this branch has no rows in them. State directories older than 2026-09-16 must
/// already be deleted for the resource-policy shape, so no data path is preserved.
///
/// Stored kind strings of the ordinary path were renamed in the same change without
/// a data migration; a v12 state directory that holds lifecycle history is recreated,
/// as the design keeps no compatibility.
pub const SCHEMA_V13: &str = r#"
DROP TABLE qualification_evidence_refs;
DROP TABLE qualification_ready_probes;
DROP TABLE qualification_request_results;
DROP TABLE qualification_request_attempts;
DROP TABLE qualification_case_actions;
DROP TABLE qualification_parked_status;
DROP TABLE candidate_cleanup_actions;
DROP TABLE qualifications;
DROP TABLE qualification_runs;
DROP TABLE host_qualification_policies;
"#;

// Spec §3: the per-launch engine key, encrypted at rest with XChaCha20-Poly1305 under
// the identity key file, with binding id, incarnation and role as associated data so
// a row copied between bindings or roles does not authenticate. Deleted when the
// binding releases.
pub const SCHEMA_V14: &str = r#"
CREATE TABLE engine_secrets(
  binding_id TEXT PRIMARY KEY REFERENCES runtime_bindings(id),
  incarnation TEXT NOT NULL,
  nonce BLOB NOT NULL CHECK(length(nonce)=24),
  ciphertext BLOB NOT NULL
);
"#;

// Spec §4.2 (ordinary launch design): SGLang seals two keys per launch, one
// inference and one admin, so `engine_secrets` carries a role and a binding may
// hold one row per role. Existing rows were vLLM inference keys and migrate to
// that role. The rebuild is foreign-key ordered and preserves nonce/ciphertext
// bytes, but the bytes do not carry over: role joined the sealing AAD in the
// same change, so pre-v15 ciphertexts no longer authenticate and fail to open.
// Affected launches re-seal on their next start. As with SCHEMA_V13, there is
// no compatibility with state written before v15.
pub const SCHEMA_V15: &str = r#"
CREATE TABLE engine_secrets_v15(
  binding_id TEXT NOT NULL REFERENCES runtime_bindings(id),
  role TEXT NOT NULL CHECK(role IN ('inference','admin')),
  incarnation TEXT NOT NULL,
  nonce BLOB NOT NULL CHECK(length(nonce)=24),
  ciphertext BLOB NOT NULL,
  PRIMARY KEY(binding_id, role)
);
INSERT INTO engine_secrets_v15(binding_id,role,incarnation,nonce,ciphertext)
  SELECT binding_id,'inference',incarnation,nonce,ciphertext FROM engine_secrets;
DROP TABLE engine_secrets;
ALTER TABLE engine_secrets_v15 RENAME TO engine_secrets;
"#;

pub const SCHEMA_V9: &str = r#"
CREATE TABLE owned_launch_associations(
  step_id TEXT PRIMARY KEY REFERENCES lifecycle_steps(id),
  binding_id TEXT NOT NULL UNIQUE REFERENCES runtime_bindings(id),
  incarnation TEXT NOT NULL UNIQUE,
  association_json TEXT NOT NULL CHECK(length(CAST(association_json AS BLOB)) <= 1048576)
);
CREATE TABLE candidate_cleanup_actions(
  operation_id TEXT PRIMARY KEY REFERENCES lifecycle_runs(operation_id),
  run_id TEXT NOT NULL REFERENCES qualification_runs(id),
  step_id TEXT NOT NULL UNIQUE REFERENCES lifecycle_steps(id),
  predecessor_cleanup_operation_id TEXT UNIQUE REFERENCES candidate_cleanup_actions(operation_id),
  CHECK(predecessor_cleanup_operation_id IS NULL OR predecessor_cleanup_operation_id != operation_id)
);
CREATE UNIQUE INDEX one_initial_candidate_cleanup ON candidate_cleanup_actions(run_id)
  WHERE predecessor_cleanup_operation_id IS NULL;
"#;

pub const SCHEMA_V8: &str = r#"
CREATE TABLE qualification_case_actions(
  run_id TEXT NOT NULL REFERENCES qualification_runs(id),
  case_id TEXT NOT NULL,
  operation_id TEXT NOT NULL UNIQUE REFERENCES operations(id),
  step_id TEXT NOT NULL UNIQUE REFERENCES lifecycle_steps(id),
  PRIMARY KEY(run_id,case_id)
);
"#;

pub const SCHEMA_V7: &str = r#"
CREATE TABLE event_meta(
  singleton INTEGER PRIMARY KEY CHECK(singleton=1),
  incarnation TEXT NOT NULL,
  retained_after INTEGER NOT NULL CHECK(retained_after>=0)
);
CREATE TABLE management_events(
  sequence INTEGER PRIMARY KEY AUTOINCREMENT,
  recorded_at_ms INTEGER NOT NULL,
  kind TEXT NOT NULL,
  deployment_id TEXT,
  operation_id TEXT,
  payload_json TEXT NOT NULL
);
"#;

/// v16: additive resource namespaces preserve immutable legacy receipts and keys.
pub const SCHEMA_V16: &str = r#"
CREATE TABLE host_resource_namespaces(
  host_id TEXT PRIMARY KEY,
  policy_key TEXT NOT NULL UNIQUE,
  kind TEXT NOT NULL CHECK(kind IN ('embedded','remote'))
);
CREATE UNIQUE INDEX one_embedded_resource_namespace ON host_resource_namespaces(kind) WHERE kind='embedded';
CREATE TABLE host_resource_keys(
  host_id TEXT NOT NULL REFERENCES host_resource_namespaces(host_id),
  kind TEXT NOT NULL CHECK(kind IN ('domain','device')),
  local_id TEXT NOT NULL,
  ledger_key TEXT NOT NULL,
  PRIMARY KEY(host_id,kind,local_id),
  UNIQUE(kind,ledger_key)
);
"#;

/// SPEC §4.1: immutable redemption binding and renewable, revocable host identity.
pub const SCHEMA_V18: &str = r#"
CREATE TABLE remote_binding_ingress (
 binding_id TEXT PRIMARY KEY REFERENCES runtime_bindings(id),
 host_id TEXT NOT NULL REFERENCES enrolled_hosts(host_id), endpoint TEXT NOT NULL
);
CREATE TABLE approved_host_publications (
 host_id TEXT PRIMARY KEY REFERENCES enrolled_hosts(host_id),
 config_json TEXT NOT NULL, boot_id TEXT NOT NULL, fingerprint TEXT NOT NULL,
 received_at_ms INTEGER NOT NULL CHECK(received_at_ms>=0)
);
CREATE TABLE managed_configuration_sources (
 deployment_id TEXT NOT NULL, revision INTEGER NOT NULL, config_json TEXT NOT NULL,
 PRIMARY KEY(deployment_id,revision),
 FOREIGN KEY(deployment_id,revision) REFERENCES effective_revisions(deployment_id,revision)
);
"#;

pub const SCHEMA_V17: &str = r#"
CREATE TABLE host_invitations (
 digest TEXT PRIMARY KEY, host_name TEXT NOT NULL, expires_unix INTEGER NOT NULL
);
CREATE TABLE enrolled_hosts (
 host_id TEXT PRIMARY KEY, host_name TEXT NOT NULL UNIQUE, key_digest TEXT NOT NULL,
 revoked INTEGER NOT NULL DEFAULT 0 CHECK(revoked IN (0,1))
);
CREATE TABLE host_certificates (
 fingerprint TEXT PRIMARY KEY, host_id TEXT NOT NULL REFERENCES enrolled_hosts(host_id),
 certificate_pem TEXT NOT NULL, expires_unix INTEGER NOT NULL
);
CREATE TABLE host_enrollment_transactions (
 invitation_digest TEXT PRIMARY KEY REFERENCES host_invitations(digest),
 transaction_id TEXT NOT NULL UNIQUE, host_name TEXT NOT NULL, key_digest TEXT NOT NULL,
 csr_digest TEXT NOT NULL, fingerprint TEXT NOT NULL REFERENCES host_certificates(fingerprint)
);
CREATE TABLE host_certificate_renewals (
 host_id TEXT NOT NULL REFERENCES enrolled_hosts(host_id), transaction_id TEXT NOT NULL,
 csr_digest TEXT NOT NULL, fingerprint TEXT NOT NULL REFERENCES host_certificates(fingerprint),
 PRIMARY KEY(host_id,transaction_id)
);
"#;

/// v19: pre-E1 state carried into the ADR 0014 shape (owner decision
/// 2026-09-22). One row per effective revision the upgrade found in the old
/// shape: migrated with the fingerprint it carried before, so ownership and
/// adoption keep the identity the running engine was launched under, or refused
/// with the operator diagnostic while everything it owns stays retained. The
/// legacy bytes are kept verbatim; nothing is deleted. The data step is
/// `ordinary_lifecycle::legacy_engine_config::migrate`, in the same transaction.
pub const SCHEMA_V19: &str = r#"
CREATE TABLE engine_config_migrations (
 deployment_id TEXT NOT NULL, revision INTEGER NOT NULL,
 outcome TEXT NOT NULL CHECK(outcome IN ('migrated','refused')),
 legacy_fingerprint TEXT NOT NULL,
 fingerprint TEXT,
 legacy_command_fingerprint TEXT,
 legacy_effective_json TEXT NOT NULL,
 legacy_source_json TEXT,
 diagnostic TEXT NOT NULL,
 PRIMARY KEY(deployment_id,revision),
 FOREIGN KEY(deployment_id,revision) REFERENCES effective_revisions(deployment_id,revision),
 CHECK((outcome='migrated') = (fingerprint IS NOT NULL))
);
CREATE TABLE host_publication_migrations (
 host_id TEXT PRIMARY KEY REFERENCES enrolled_hosts(host_id),
 legacy_fingerprint TEXT NOT NULL, fingerprint TEXT NOT NULL,
 legacy_config_json TEXT NOT NULL
);
"#;

/// v20 (ADR 0014 §7, WE3): the checkpoint digest of each deployment revision.
/// `pending` until a host holding the checkpoint measures it; `recorded` with
/// the digest and the weights bytes the manifest supplies; `mismatch` when the
/// measured digest is not the declared expectation; `unusable` when a derived
/// memory request cannot be resolved with the measured weights. `provisional`
/// marks a revision frozen before its weights were known, whose activation
/// waits for the digest. Revisions accepted before v20 have no row: their
/// digest is measured and recorded on first placement. Additive; idempotent so
/// a store rolled back to an earlier version can reapply it.
pub const SCHEMA_V20: &str = r#"
CREATE TABLE IF NOT EXISTS checkpoint_digests (
 deployment_id TEXT NOT NULL, revision INTEGER NOT NULL,
 state TEXT NOT NULL CHECK(state IN ('pending','recorded','mismatch','unusable')),
 host_id TEXT NOT NULL,
 expected TEXT,
 digest TEXT,
 weights_bytes INTEGER CHECK(weights_bytes IS NULL OR weights_bytes>=0),
 provisional INTEGER NOT NULL CHECK(provisional IN (0,1)),
 diagnostic TEXT,
 updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms>=0),
 PRIMARY KEY(deployment_id,revision),
 FOREIGN KEY(deployment_id,revision) REFERENCES effective_revisions(deployment_id,revision),
 CHECK((state='pending') = (digest IS NULL)),
 CHECK((state IN ('recorded','mismatch')) = (weights_bytes IS NOT NULL))
);
"#;

/// v21 (SPEC §3, Phase B follow-up): endpoint port leases are per host. A lease
/// was keyed `(127.0.0.1, port)` for the whole installation, so two hosts could
/// not both lease the same port of their own ranges although every engine binds
/// its own host's loopback. The key now names the host the deployment revision
/// is placed on (`$.host.name` of its effective revision: the enrolled host id,
/// or the embedded host's name). Existing leases keep their binding, address
/// and port and take their binding's host; a binding whose revision cannot be
/// read keeps the lease under the empty host key rather than losing it.
/// Rebuilt in place; idempotent so a store rolled back can reapply it.
pub const SCHEMA_V21: &str = r#"
DROP TABLE IF EXISTS endpoint_leases_by_host;
CREATE TABLE endpoint_leases_by_host(
  host_id TEXT NOT NULL,
  host TEXT NOT NULL,
  port INTEGER NOT NULL CHECK(port BETWEEN 1 AND 65535),
  binding_id TEXT NOT NULL REFERENCES runtime_bindings(id),
  PRIMARY KEY(host_id,host,port)
);
INSERT INTO endpoint_leases_by_host(host_id,host,port,binding_id)
  SELECT COALESCE((SELECT json_extract(e.effective_json,'$.host.name')
                     FROM runtime_bindings b JOIN effective_revisions e
                       ON e.deployment_id=b.deployment_id AND e.revision=b.revision
                    WHERE b.id=l.binding_id),''),
         l.host, l.port, l.binding_id
    FROM endpoint_leases l;
DROP TABLE endpoint_leases;
ALTER TABLE endpoint_leases_by_host RENAME TO endpoint_leases;
CREATE INDEX IF NOT EXISTS endpoint_leases_binding ON endpoint_leases(binding_id);
"#;

/// v22 (ADR 0013 §5, owner decision P1): deployment instances. A deployment
/// declares `instances: N`; each instance is one engine group with its own
/// placement, generation, binding, lifecycle claim, activation, request leases
/// and resource owner.
///
/// - `deployment_revision_instances`: the declared count and normalized
///   placement constraints of each accepted revision.
/// - `host_effective_revisions`: the revision resolved against each host that
///   was checked (ADR 0013 §3; SPEC §8: one installation name does not prove one
///   build). The canonical `effective_revisions` row stays the first resolving
///   host's.
/// - `deployment_instances`: dense indices `0..N-1`, stable across restarts;
///   the placed host and devices (NULL until placed), the generation its last
///   activation drew from the deployment's one counter, and the operator's
///   per-instance stop (owner decision Q7). `retiring` marks a surplus row a
///   count decrease drains before it is removed.
///
/// The tables are created here; `crate::instances::migrate` then adds
/// `instance_index` to bindings, runs, claims, request leases, attempts and
/// resource owners, re-keys their one-per-deployment rules per instance and
/// backfills every existing deployment as instance 0, in the same transaction.
/// Additive and idempotent, so a store rolled back to an earlier version can
/// reapply it.
pub const SCHEMA_V22: &str = r#"
CREATE TABLE IF NOT EXISTS deployment_revision_instances(
  deployment_id TEXT NOT NULL,
  revision INTEGER NOT NULL CHECK(revision>0),
  instances INTEGER NOT NULL CHECK(instances BETWEEN 1 AND 64),
  placement_json TEXT NOT NULL CHECK(json_valid(placement_json)),
  PRIMARY KEY(deployment_id,revision),
  FOREIGN KEY(deployment_id,revision) REFERENCES effective_revisions(deployment_id,revision)
);
CREATE TABLE IF NOT EXISTS host_effective_revisions(
  deployment_id TEXT NOT NULL,
  revision INTEGER NOT NULL CHECK(revision>0),
  host_id TEXT NOT NULL,
  outcome TEXT NOT NULL CHECK(outcome IN ('resolved','refused')),
  effective_json TEXT,
  fingerprint TEXT,
  diagnostic TEXT,
  PRIMARY KEY(deployment_id,revision,host_id),
  FOREIGN KEY(deployment_id,revision) REFERENCES effective_revisions(deployment_id,revision),
  CHECK((outcome='resolved') = (effective_json IS NOT NULL AND fingerprint IS NOT NULL))
);
CREATE TABLE IF NOT EXISTS deployment_instances(
  deployment_id TEXT NOT NULL REFERENCES deployments(id),
  instance_index INTEGER NOT NULL CHECK(instance_index BETWEEN 0 AND 63),
  host_id TEXT,
  device_json TEXT CHECK(device_json IS NULL OR json_valid(device_json)),
  generation INTEGER CHECK(generation IS NULL OR generation>=1),
  state TEXT NOT NULL DEFAULT 'active' CHECK(state IN ('active','retiring')),
  operator_stopped INTEGER NOT NULL DEFAULT 0 CHECK(operator_stopped IN (0,1)),
  placed_at TEXT,
  PRIMARY KEY(deployment_id,instance_index)
);
"#;

/// v23 (ADR 0013 §4, §6, §7; unit I2): the instance is the unit of runtime.
///
/// Every runtime fact a single-instance deployment kept on its `deployments`
/// row moves onto its instance row: the revision its current incarnation was
/// admitted against, its generation (drawn from the deployment's one counter,
/// so a deployment and a generation still identify exactly one incarnation),
/// and its desired, observed, admission and dispatch state. The `deployments`
/// row keeps the declaration (revision, counter, suspension, operator stop)
/// and carries the aggregate of its instances, which triggers maintain for
/// managed deployments so every existing reader keeps working.
///
/// - `pending_start_until_ms`: a start the scheduler must still place (an
///   explicit start whose instance did not fit, or a restart after a
///   non-count revision, owner decision Q8), retried until this deadline.
/// - `last_error`: the closed placement or start diagnostic status shows.
/// - `host_effective_revisions.source_json`: the deployment document scoped
///   to that host, which a remote launch on it is rendered from.
/// - `instance_runtime`: one row per instance with the columns the lifecycle
///   fences read, named as the `deployments` columns they replace.
///
/// The columns are added, the view and triggers created and instance 0's
/// state copied from its deployment by the data step
/// (`crate::instances::migrate_v23`), which checks what already exists so it
/// is idempotent; this batch only removes the v22 trigger that inferred an
/// instance's host from the canonical revision, because placement now records
/// the host explicitly.
pub const SCHEMA_V23: &str = r#"
DROP TRIGGER IF EXISTS instance_activation_generation;
"#;

/// v24 (SPEC §4.3, owner decision 4 of 2026-09-22): the durable marker of a host
/// drain. One row per Stop the drain issued; the drain is pending while any of
/// those operations is not terminal, and the host takes no new placements while
/// it is. Additive; idempotent so a store rolled back can reapply it.
pub const SCHEMA_V24: &str = r#"
CREATE TABLE IF NOT EXISTS host_drains(
  host_id TEXT NOT NULL,
  operation_id TEXT NOT NULL REFERENCES operations(id),
  recorded_at_ms INTEGER NOT NULL CHECK(recorded_at_ms>=0),
  PRIMARY KEY(host_id,operation_id)
);
"#;

/// v25 (SPEC §§3.1, 7.3; per-launch host claims): the hosts whose latest
/// authenticated publication says their agent journal keeps one claim per
/// launch. A row is replaced or removed with each publication; a host without
/// one holds one launch claim at a time, so placement keeps refusing a second
/// launch there. Additive; idempotent so a store rolled back can reapply it.
pub const SCHEMA_V25: &str = r#"
CREATE TABLE IF NOT EXISTS host_launch_claims(
  host_id TEXT PRIMARY KEY REFERENCES enrolled_hosts(host_id),
  mode TEXT NOT NULL CHECK(mode IN ('per_launch')),
  recorded_at_ms INTEGER NOT NULL CHECK(recorded_at_ms>=0)
);
"#;

/// v26 (ADR 0013 §4, owner decision P1; per-instance host fencing): a host may
/// also advertise `per_instance`, a per-launch journal (v5) that fences each
/// instance of a deployment by its own generation, so two instances of one
/// deployment may share it. SQLite cannot widen a CHECK in place, so the table
/// is rebuilt with every row carried across unchanged. Rerunnable.
pub const SCHEMA_V26: &str = r#"
CREATE TABLE IF NOT EXISTS host_launch_claims(
  host_id TEXT PRIMARY KEY REFERENCES enrolled_hosts(host_id),
  mode TEXT NOT NULL,
  recorded_at_ms INTEGER NOT NULL CHECK(recorded_at_ms>=0)
);
DROP TABLE IF EXISTS host_launch_claims_v26;
CREATE TABLE host_launch_claims_v26(
  host_id TEXT PRIMARY KEY REFERENCES enrolled_hosts(host_id),
  mode TEXT NOT NULL CHECK(mode IN ('per_launch','per_instance')),
  recorded_at_ms INTEGER NOT NULL CHECK(recorded_at_ms>=0)
);
INSERT INTO host_launch_claims_v26(host_id,mode,recorded_at_ms)
  SELECT host_id,mode,recorded_at_ms FROM host_launch_claims
   WHERE mode IN ('per_launch','per_instance');
DROP TABLE host_launch_claims;
ALTER TABLE host_launch_claims_v26 RENAME TO host_launch_claims;
"#;

/// Owner decision 2026-09-23: startup peaks measured on a first run, per
/// revision, host and engine installation. Later starts of the revision on
/// that host reserve the largest one recorded instead of the placeholder.
pub const SCHEMA_V27: &str = r#"
CREATE TABLE IF NOT EXISTS startup_measurements(
  deployment_id TEXT NOT NULL,
  revision INTEGER NOT NULL CHECK(revision>=1),
  host_id TEXT NOT NULL,
  installation TEXT NOT NULL,
  peak_bytes INTEGER NOT NULL CHECK(peak_bytes>0),
  step_id TEXT NOT NULL,
  measured_at_ms INTEGER NOT NULL CHECK(measured_at_ms>=0),
  PRIMARY KEY(deployment_id,revision,host_id,installation)
);
"#;

/// W10 gaps (owner decisions 2026-09-23). `dispatch_closures` records why an
/// instance incarnation's dispatch gate is closed, one row per reason, so a
/// failed switch reopens only a gate it alone closed (SPEC §§10, 13.2).
/// `active_switches` is the switch in progress, for status. The warm-residency
/// flag (SPEC §6.5, ADR 0013 amendment) is added by `switch_state::migrate`,
/// which checks for the column so a reapplied migration changes nothing.
pub const SCHEMA_V28: &str = r#"
CREATE TABLE IF NOT EXISTS dispatch_closures(
  deployment_id TEXT NOT NULL,
  instance_index INTEGER NOT NULL CHECK(instance_index>=0),
  generation INTEGER NOT NULL,
  reason TEXT NOT NULL CHECK(reason IN ('switch','host_session','engine_exit')),
  PRIMARY KEY(deployment_id,instance_index,generation,reason)
);
CREATE TABLE IF NOT EXISTS active_switches(
  switch_id TEXT PRIMARY KEY,
  target_deployment TEXT NOT NULL,
  host_id TEXT,
  victims_json TEXT NOT NULL CHECK(json_valid(victims_json)),
  phase TEXT NOT NULL CHECK(phase IN ('planned','admission_closed','released')),
  evicting INTEGER NOT NULL DEFAULT 0 CHECK(evicting IN (0,1))
);
"#;

/// Owner decision 2026-09-23 (solo first start): the weights a host sized by
/// a stat walk while a revision's checkpoint digest is still pending, so a
/// first start's startup estimate is known before the full digest.
pub const SCHEMA_V29: &str = r#"
CREATE TABLE IF NOT EXISTS checkpoint_sizes(
  deployment_id TEXT NOT NULL,
  revision INTEGER NOT NULL CHECK(revision>=1),
  host_id TEXT NOT NULL,
  weights_bytes INTEGER NOT NULL CHECK(weights_bytes>=0),
  sized_at_ms INTEGER NOT NULL CHECK(sized_at_ms>=0),
  PRIMARY KEY(deployment_id,revision)
);
"#;

/// v30 (SPEC §4.3, owner decision 4; router review item 14): the drain intent.
/// A v24 marker names the Stops a drain issued, so it cannot exist before the
/// first of them, and an instance placed on the host in between survived the
/// drain. An intent row needs no operation: it is written in the same
/// transaction that enumerates the host's instances, before any Stop, and the
/// host takes no new placements while an intent is open (`completed_at_ms`
/// null) or any marked Stop is unsettled. Additive; idempotent so a store
/// rolled back can reapply it.
pub const SCHEMA_V30: &str = r#"
CREATE TABLE IF NOT EXISTS host_drain_intents(
  host_id TEXT NOT NULL CHECK(length(host_id)>0),
  drain_key TEXT NOT NULL CHECK(length(drain_key)>0),
  recorded_at_ms INTEGER NOT NULL CHECK(recorded_at_ms>=0),
  completed_at_ms INTEGER CHECK(completed_at_ms IS NULL OR completed_at_ms>=0),
  PRIMARY KEY(host_id,drain_key)
);
"#;

/// v31 (SPEC §4.3): the deadline a host drain intent was opened with. A drain
/// whose request never completed (the server stopped between opening the
/// intent and recording its Stops) otherwise held its host out of placement
/// forever; with its deadline, an intent past it with no Stop of the host
/// still open is completed and journaled (`Store::expire_host_drain_intents`).
/// A v30 intent has no row here and is given the drain window from its
/// recording. Additive; idempotent so a store rolled back can reapply it.
pub const SCHEMA_V31: &str = r#"
CREATE TABLE IF NOT EXISTS host_drain_intent_deadlines(
  host_id TEXT NOT NULL CHECK(length(host_id)>0),
  drain_key TEXT NOT NULL CHECK(length(drain_key)>0),
  deadline_ms INTEGER NOT NULL CHECK(deadline_ms>=0),
  PRIMARY KEY(host_id,drain_key)
);
"#;

/// v32 (ADR 0016, owner decision 2026-09-24): recovery of a revoked host
/// under its same identity. A recovery invitation names the one enrolled host
/// it may re-enroll; a revoked certificate is revoked by its own fingerprint,
/// so recovery issues a new certificate while every older one stays refused
/// for ever. Certificates of hosts revoked before v32 are carried in as
/// revoked. Additive; idempotent so a store rolled back can reapply it.
pub const SCHEMA_V32: &str = r#"
CREATE TABLE IF NOT EXISTS host_recovery_invitations(
  digest TEXT PRIMARY KEY REFERENCES host_invitations(digest),
  host_id TEXT NOT NULL REFERENCES enrolled_hosts(host_id)
);
CREATE TABLE IF NOT EXISTS revoked_host_certificates(
  fingerprint TEXT PRIMARY KEY REFERENCES host_certificates(fingerprint),
  revoked_at_unix INTEGER NOT NULL CHECK(revoked_at_unix>=0)
);
INSERT OR IGNORE INTO revoked_host_certificates(fingerprint,revoked_at_unix)
  SELECT c.fingerprint,0 FROM host_certificates c JOIN enrolled_hosts h ON h.host_id=c.host_id
  WHERE h.revoked=1;
"#;

/// v33 (ADR 0008): the materialization state of each revision's declared
/// remote model source on each host. Rows of a deleted deployment stay (a
/// delete never removes a copy); they stop counting as references.
pub const SCHEMA_V33: &str = r#"
CREATE TABLE IF NOT EXISTS model_sources(
  deployment_id TEXT NOT NULL CHECK(length(deployment_id)>0),
  revision INTEGER NOT NULL CHECK(revision>0),
  host_id TEXT NOT NULL CHECK(length(host_id)>0),
  source_key TEXT NOT NULL CHECK(source_key LIKE 'sources/%'),
  state TEXT NOT NULL CHECK(state IN ('pending','downloading','verified','failed')),
  bytes_done INTEGER NOT NULL CHECK(bytes_done>=0),
  bytes_total INTEGER NOT NULL CHECK(bytes_total>=0),
  reason TEXT CHECK((state='failed')=(reason IS NOT NULL)),
  terminal INTEGER NOT NULL CHECK(terminal IN (0,1)),
  updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms>=0),
  PRIMARY KEY(deployment_id,revision,host_id)
);
"#;

/// v34 (ADR 0017): the release version and post-baseline capabilities each
/// enrolled host declared on its latest control session, and the version
/// skew policy's verdict on it (`supported`, `upgrade_recommended`,
/// `upgrade_required` or `refused`). Replaced on every connect; evidence for
/// status, never an authority. Additive; idempotent.
pub const SCHEMA_V34: &str = r#"
CREATE TABLE IF NOT EXISTS host_versions(
  host_id TEXT PRIMARY KEY REFERENCES enrolled_hosts(host_id),
  binary_version TEXT NOT NULL CHECK(length(binary_version)<=128),
  compatibility TEXT NOT NULL CHECK(compatibility IN ('supported','upgrade_recommended','upgrade_required','refused')),
  reason TEXT NOT NULL CHECK(length(reason)<=512),
  capabilities_json TEXT NOT NULL CHECK(json_valid(capabilities_json) AND json_type(capabilities_json)='array'),
  recorded_at_ms INTEGER NOT NULL CHECK(recorded_at_ms>=0)
);
"#;

/// v35 (ADR 0018 §4): profile retirements. A row holds (host, profile) out of
/// placement from the transaction that wrote it; `confirmed` means the server
/// found no instance of the profile left on the host. Its stops are recorded
/// so progress is judged on their evidence. Deleted when cancelled, refused,
/// ended unconfirmed, expired, or when the host's re-publication without the
/// profile is accepted.
pub const SCHEMA_V35: &str = r#"
CREATE TABLE IF NOT EXISTS profile_retirements(
  host_id TEXT NOT NULL CHECK(length(host_id) BETWEEN 1 AND 128),
  profile TEXT NOT NULL CHECK(length(profile) BETWEEN 1 AND 64),
  retire_key TEXT NOT NULL CHECK(length(retire_key) BETWEEN 1 AND 128),
  state TEXT NOT NULL CHECK(state IN ('retiring','confirmed')),
  recorded_at_ms INTEGER NOT NULL CHECK(recorded_at_ms>=0),
  deadline_ms INTEGER NOT NULL CHECK(deadline_ms>=recorded_at_ms),
  PRIMARY KEY(host_id, profile)
);
CREATE TABLE IF NOT EXISTS profile_retirement_stops(
  host_id TEXT NOT NULL,
  profile TEXT NOT NULL,
  operation_id TEXT NOT NULL REFERENCES operations(id),
  PRIMARY KEY(host_id, profile, operation_id),
  FOREIGN KEY(host_id, profile) REFERENCES profile_retirements(host_id, profile) ON DELETE CASCADE
);
"#;

/// v36 (ADR 0018 §4, §5; review decision I2/I3): the profiles standalone's
/// embedded host publishes now. The embedded host is not enrolled, so it has
/// no approved publication; this row plays that part for placement: a
/// profile it no longer lists takes no new instance, as on a server.
pub const SCHEMA_V36: &str = r#"
CREATE TABLE IF NOT EXISTS embedded_host_publications(
  host_id TEXT PRIMARY KEY CHECK(length(host_id) BETWEEN 1 AND 128),
  profiles_json TEXT NOT NULL CHECK(json_valid(profiles_json) AND json_type(profiles_json)='array'),
  recorded_at_ms INTEGER NOT NULL CHECK(recorded_at_ms>=0)
);
"#;

/// v37 (ADR 0019, discrete GPU design §7): capyctl picks the GPU on a multi-GPU
/// host. A deployment that pins no device is resolved once per GPU of such a
/// host; each resolution is kept here, beside the host's own row (which stays
/// the lowest-index GPU's, so every reader of `host_effective_revisions` keeps
/// its meaning). `deployment_instances.device` records the GPU an instance was
/// placed on, the preference a stopped instance keeps (ADR 0013 §4); it is
/// added by the data step (`crate::instances::migrate_v37`), which checks
/// what exists so the step is idempotent. Additive and forward-only.
pub const SCHEMA_V37: &str = r#"
CREATE TABLE IF NOT EXISTS host_device_effective_revisions(
  deployment_id TEXT NOT NULL,
  revision INTEGER NOT NULL CHECK(revision>0),
  host_id TEXT NOT NULL,
  device TEXT NOT NULL CHECK(length(device) BETWEEN 1 AND 128),
  effective_json TEXT NOT NULL,
  fingerprint TEXT NOT NULL,
  source_json TEXT NOT NULL CHECK(json_valid(source_json)),
  PRIMARY KEY(deployment_id,revision,host_id,device),
  FOREIGN KEY(deployment_id,revision,host_id) REFERENCES host_effective_revisions(deployment_id,revision,host_id)
);
"#;

/// v38 (SPEC §10, amended 2026-10-01): a request whose client hung up. The
/// router closed the engine connection; the lease stays `inflight` (so every
/// drain still waits for it) until the engine reports quiescence after
/// `cancelled_at_ms`. Additive and forward-only.
pub const SCHEMA_V38: &str = r#"
CREATE TABLE IF NOT EXISTS request_lease_cancellations(
  lease_id TEXT PRIMARY KEY REFERENCES request_leases(id) ON DELETE CASCADE,
  cancelled_at_ms INTEGER NOT NULL CHECK(cancelled_at_ms>=0)
);
"#;

/// v39 (ADR 0014 amendment A13): the memory a parked engine was sampled
/// holding, per revision, host, engine installation and memory domain. A park
/// of the revision there is charged the largest one recorded instead of the
/// parked placeholder (never less). Additive and forward-only.
pub const SCHEMA_V39: &str = r#"
CREATE TABLE IF NOT EXISTS parked_measurements(
  deployment_id TEXT NOT NULL,
  revision INTEGER NOT NULL CHECK(revision>=1),
  host_id TEXT NOT NULL,
  installation TEXT NOT NULL,
  domain TEXT NOT NULL,
  bytes INTEGER NOT NULL CHECK(bytes>0),
  step_id TEXT NOT NULL,
  measured_at_ms INTEGER NOT NULL CHECK(measured_at_ms>=0),
  PRIMARY KEY(deployment_id,revision,host_id,installation,domain)
);
"#;

/// v40 (ADR 0014 amendment A16): the hybrid state slot a host measured from
/// the checkpoint's `config.json` beside the weights, recorded with the
/// digest. Absent for any other model, and on rows recorded before v40. The
/// column is added by `checkpoint_digests::migrate_v40` only when missing, so
/// a store rolled back to an earlier version can reapply it.
pub const SCHEMA_V40: &str = "-- checkpoint_digests.state_slot_bytes (migrate_v40)";

/// v41 (ADR 0028 §5, §6): group plans and members, per-member resource owners,
/// group endpoint leases, rendezvous port exclusions and per-host checkpoint
/// digests. The steps are in `groups::migrate_v41`, which is idempotent.
pub const SCHEMA_V41: &str = "-- group plans (groups::migrate_v41)";

/// v42 (ADR 0028 §8, §11): each group member's Launch handle, recorded with
/// its dispatch fence so a stop reaches it from any later session, and the
/// rank a failed group failed at. The columns are added by
/// `groups::migrate_v42` only when missing.
pub const SCHEMA_V42: &str = "-- group member launch handles (groups::migrate_v42)";

/// v43 (ADR 0028 §12): each group plan's wake canary reference, recorded at
/// its first readiness, and the closed code a failed group names when it is
/// not a member's own failure (`group_wake_mismatch`). The columns are added
/// by `groups::migrate_v43` only when missing.
pub const SCHEMA_V43: &str = "-- group canary references (groups::migrate_v43)";

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn schema_v1_applies_cleanly() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        let tables: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
                .unwrap();
            stmt.query_map([], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        for expected in [
            "deployments",
            "domains",
            "generation_history",
            "hosts",
            "journal_entries",
            "operations",
            "owners",
            "reservations",
            "schema_migrations",
        ] {
            assert!(
                tables.iter().any(|t| t == expected),
                "missing table {expected}"
            );
        }
    }

    #[test]
    fn operations_has_no_revision_precondition_column() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_V1).unwrap();
        let cols: Vec<String> = {
            let mut stmt = conn.prepare("PRAGMA table_info(operations)").unwrap();
            stmt.query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert!(!cols.iter().any(|c| c.contains("revision")));
    }
}
