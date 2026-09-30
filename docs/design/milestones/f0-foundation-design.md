# F0 — Foundation: Detailed Design

**Status:** Approved direction, September 10, 2026.
**Source:** [`../0000-full-picture.md`](../0000-full-picture.md) and
[`../../SPEC.md`](../../SPEC.md) rev 0.2. This design implements F0 of the delivery
sequence (SPEC §18); the spec is authoritative where summaries differ.
**Requirements covered:** R02, R11, R13 (foundation of R10, R12).
**Tests targeted:** T01–T04, T08, T09, T26, T27 (fixture/simulator tiers only — no GPUs).

## 1. Scope

F0 delivers the contracts and foundation milestone: action-first CLI skeleton, strict
config/default behavior, domain types and state machines, the durable store, the resource
ledger, abstract host/adapter/launcher contracts, and the fake-engine transport harness.
Remote-host and resource-owner contracts are designed from the beginning (SPEC §18 rule)
but exercised only through embedded/simulated participants in F0. Enrollment execution,
router HTTP surface, and real engines are explicitly out of scope (F1/F3).

## 2. Slices

Six slices in build order; each leaves a coherent tested product.

| Slice | Deliverable | Tests |
|---|---|---|
| S1 — Workspace + domain | Cargo workspace — all 11 crates of the full-picture layout: `capyctl-domain`, `capyctl-store`, `capyctl-scheduler`, `capyctl-protocol`, `capyctl-controller`, `capyctl-router`, `capyctl-agent`, `capyctl-adapters`, `capyctl-launchers`, `capyctl-config`, `capyctl-cli` (later slices populate the empty crates); lifecycle state machine; identity types; stale-generation rejection | Transition-table property tests; generation rejection |
| S2 — Config | Strict YAML schema v1; §15.2 no-config matrix; `init`/`validate config` | T02, T03, T04 |
| S3 — Store | SQLite schema; transactional acceptance; migrations; idempotency keys | T08, T09 |
| S4 — Ledger | Physical-domain accounting; exclusive pools; `auto` resolution; admission blocking | T26, T27, and a sub-limit block scenario (retained host-KV owners exceeding `host_kv_limit`) |
| S5 — Contracts + fake engine | Adapter/launcher/agent traits; fake-engine harness | Fake-engine scenario suite |
| S6 — CLI + proto freeze | Full action-first parser; role startup wiring; frozen proto v1 with a wire-level round-trip (AgentControl stream plus report/replay against the fake engine over a real in-process gRPC channel) | T01 plus the round-trip scenario |

## 3. Domain model (`capyctl-domain`)

### 3.1 Identities

| Type | Visibility | Form |
|---|---|---|
| `DeploymentId` | External, durable | ULID — stable across start/park/stop/restart |
| `OperationId` | Internal | Opaque string + deployment reference |
| `Generation` | Internal | Per-deployment monotonic integer, never reset |
| `OwnerAccountId` | Internal | Opaque id; kind: deployment allocation, retained private cache, shared service, persistent namespace |

### 3.2 Lifecycle states and transitions

The ten states of SPEC §6.1 (STOPPED, STARTING, READY, DRAINING, PARKING, PARKED, WAKING,
STOPPING, RECONCILING, FAILED) with the legal-transition table exactly matching §6.1:

```text
STOPPED -> STARTING -> READY
READY -> DRAINING -> PARKING -> PARKED
PARKED -> WAKING -> READY
READY -> DRAINING -> STOPPING -> STOPPED
PARKED -> STOPPING -> STOPPED
Any uncertain state -> RECONCILING -> verified state or FAILED
```

Anything else is a type-level error. RECONCILING is the only entry point for uncertain
states. FAILED never releases reservations at the domain level — the ledger decides
release separately with evidence.

### 3.3 Intent vs observation

Desired state and observed state are distinct values. A stopped-but-enabled (on-demand)
deployment and a suspended deployment are distinct conditions (SPEC §2). Operations record
attempted transitions; observations record reported engine/agent state.

## 4. Durable store (`capyctl-store`)

Embedded transactional SQLite per ADR 0002.

```sql
deployments(id TEXT PRIMARY KEY, name TEXT UNIQUE NOT NULL, kind TEXT NOT NULL,
            route_model_id TEXT, desired_state TEXT NOT NULL,
            admission_enabled INTEGER NOT NULL, suspended INTEGER NOT NULL,
            current_generation INTEGER NOT NULL,
            schema_version INTEGER NOT NULL, created_at, updated_at)
operations(id TEXT PRIMARY KEY, deployment_id TEXT REFERENCES deployments,
           kind TEXT NOT NULL, state TEXT NOT NULL, error_code TEXT,
           idempotency_key TEXT UNIQUE,
           accepted_at, updated_at)
                                                   -- revision preconditions are
                                                   -- intentionally absent until the
                                                   -- revision-aware update design
                                                   -- exists (SPEC §19 defers it)
generation_history(deployment_id, generation, started_at, ended_at, outcome)
owners(id TEXT PRIMARY KEY, kind TEXT NOT NULL, deployment_id TEXT NULL)
reservations(owner_id TEXT REFERENCES owners, domain_id TEXT,
             bytes INTEGER NOT NULL, phase TEXT NOT NULL,
             exclusive_devices TEXT)          -- JSON array
domains(id TEXT PRIMARY KEY, kind TEXT NOT NULL,   -- system | device_memory |
             observed_bytes INTEGER NULL,         --   filesystem | remote_storage
             observed_at)
hosts(id TEXT PRIMARY KEY, name TEXT, state TEXT) -- inventory skeleton; F3 enrolls
journal_entries(id, host_id, operation_id, state, evidence, recorded_at)
                                                   -- no inference bodies, ever
schema_migrations(version INTEGER PRIMARY KEY)
```

Acceptance flow (T08/T09): `deploy model` opens one transaction inserting the deployment,
its initial operation, and initial reservation intent; commit; return the ID. The
`idempotency_key` unique index makes a retry after a lost response resolve to the
existing deployment. Idempotency keys are derived, not minted as random client state:
the CLI computes `SHA-256(server context id, deployment name, canonical manifest bytes)`
as the key, so a retry after a lost response reuses the same key without client-side
persistence. The same key arriving with different content is rejected as a conflict
error (`idempotency_conflict`), never silently resolved to the existing deployment.
Intentionally re-deploying identical content as a new deployment requires a distinct
`name` (names are unique), which yields a distinct key. Status queries are read-only and
never activate anything.

Migrations are forward-only, applied at startup inside a transaction, and versioned in
`schema_migrations`. Backups: `sqlite3 .backup`-style consistent file copy documented at F0.
Store files, WAL/journal sidecars, and generated state directories are created owner-only
(0700 directories, 0600 files), matching the config creation posture; a permissions test
runs alongside T04.

## 5. Resource ledger (`capyctl-scheduler`)

### 5.1 Domains and charging

Each allocation maps to exactly one physical domain: `system` (unified CPU/GPU on
unified-memory hosts; host RAM only on discrete-GPU hosts), `device_memory` per device,
`filesystem(pool)`, `remote_storage(store_id)`.

1. Every allocation belongs to one `OwnerAccountId`; shared-service physical bytes are
   charged once to the service owner; client quotas partition usable capacity only.
2. Domain totals are the union of uniquely owned allocations, never a naive sum of labels.
3. Category sub-limits (`host_kv_limit`, `parked_limit`) are checked against their own
   owner subsets; they are sub-limits, not additional capacity.
4. Exclusive device sets conflict on overlap; disjoint device pools still share `system`.
5. Uncertain owners stay charged until a verified-release ledger event; expiration, loss
   of connectivity, or FAILED never implies release.

### 5.2 Admission check

```text
charged(D)        = Σ bytes of all owners (including C's current reservations)
transition_delta  = if C has a parked reservation on D:
                      activation_peak − parked_budget   # replace, don't stack
                    else activation_peak
pass iff charged(D) + transition_delta ≤ managed_limit(D)
      AND observed_free(D) ≥ free_reserve(D)
      AND category sub-limits hold for their subsets
      AND device-set exclusivity holds
```

Block reasons are structured codes: `insufficient_resources`, `unreconciled_ownership`,
`device_conflict`, `category_limit`, `unknown_topology`, `no_safe_estimate`,
`stale_observation`.

Observation freshness: `observed_free(D)` comes from the newest recorded domain
observation. If that observation is older than the observation TTL (v1 constant:
60 seconds, versioned alongside the auto-resolution policy), the admission check fails
with `stale_observation` instead of passing — stale data never authorizes capacity.
F0's fake inventory stamps synthetic observations so the TTL path is exercised without
hardware; F3 supplies live per-host observations under the same rule.

Transition-peak validation: when replacing a parked reservation, `activation_peak` must
cover the retained parked footprint at every instant of the wake transition (SPEC §7.3
requires validating the transition's true peak). Admission rejects the transition when it
does not, and the S4 slice tests include a restore-peak case.

### 5.3 `auto` resolution (v1 constants, locked)

Per `system` domain:

```text
managed_limit = min(75% × observed_system_memory, observed_system_memory − 8 GiB)
free_reserve  = max(8 GiB, 10% × observed_system_memory)
```

Resolved values are persisted with provenance (policy version, observed inputs,
resolution timestamp). Resolution happens per admission; numeric default changes never
apply retroactively to pinned deployments (T39). Device domains (`device_memory`) are
never auto-resolved — omission is not unlimited VRAM (SPEC §16 note).

## 6. Configuration (`capyctl-config`)

Schema v1 for kinds `server`, `host`, `deployment`, `standalone` with the field set of the
SPEC §16 sketches (any renaming documented in the schema fixture tests). Validation
rejects unknown capyctl fields, duplicate mapping keys, invalid units, unsatisfied required
fields, unsupported role/adapter combinations, conflicting reserved arguments, invalid
cache references, and contradictory standalone/remote connections (SPEC §15.3).

No-config behavior matrix (SPEC §15.2):

| Situation | Behavior |
|---|---|
| No `--config`; implicit role config exists | Load and validate; invalid content is an error, not a reset trigger |
| No implicit server config | Atomically generate safe per-user config/state + protected credentials; local authenticated listeners; print locations, not secrets |
| No implicit standalone config | Generate local server + embedded host defaults; no enrollment, no engine execution |
| Host enrolled | Load saved config and identity; never re-enroll |
| Host not enrolled, no config | Generate offline host template, explain join step, exit |
| Explicit config path missing/invalid | Fail; generation only via explicit `init`/enrollment |
| Existing identity missing/mismatched | Fail for recovery; never overwrite trust |

Creation is atomic (temp file + rename) and owner-protected; concurrent starts cannot
clobber (T04). `capyctl init server|host` and `validate config --file` are implemented in
F0. Secrets use file references; never logged.

## 7. CLI (`capyctl-cli`)

Full SPEC §14 grammar parsed and dispatched. In F0, `start server|host|standalone` runs
against embedded contracts: standalone uses in-process calls (SPEC §3.2); `start host`
runs the agent loop against the local store without remote enrollment. `deploy model`,
`status deployment`, and lifecycle actions write through the store and ledger with fake
participants where engines would run.

Output contract: stdout carries deployment ID / result; diagnostics to stderr; `--output
json` for machine mode; structured error codes; stable exit codes:

| Exit | Meaning |
|---|---|
| 0 | Success |
| 2 | Invalid configuration |
| 3 | Unauthorized (profile/operation) |
| 4 | Insufficient resources |
| 5 | Unsupported capability |
| 6 | Unreconciled ownership |
| 7 | Device conflict (exclusive device overlap) |
| 8 | Category sub-limit exceeded |
| 10 | Activation timeout |
| 11 | Host topology unknown |
| 12 | No safe resource estimate |

List/status commands never activate models as a side effect (T01, T08).

## 8. Transport contract freeze (`capyctl-protocol`)

Package `capyctl.management.v1`. Full skeleton with frozen field numbers is in
[Appendix A](#appendix-a--frozen-proto-skeleton). Services:

- `Bootstrap`: server-authenticated invitation enrollment exchange (exercised in F3;
  schema frozen now so F3 adds no breaks).
- `AgentControl`: agent-initiated bidirectional stream — server→agent commands
  (`LaunchMember`, `OpenIngressGate`, `Inspect`, `GroupControl`, `TerminateMember`,
  `CancelWork`), agent→server reports (`Connect`, `ReportInventory`,
  `ReportOperationResult`).

All commands carry identity through the embedded Envelope — `host_id`, `deployment_id`,
`generation`, `operation_id`, `deadline`, `expected_state`, and `profile_fingerprint`
(SPEC §13.1); `LaunchMember` additionally carries the permitted resource plan.
At-least-once delivery replays known results; the agent rejects stale generations and
incompatible protocol versions.

Deadline semantics under clock skew: `deadline_unix_ms` is compared against the receiver's
clock with a versioned skew tolerance (v1 constant: 30 seconds). An agent treats a command
as expired only when `now > deadline + tolerance`; a server treats a report arriving after
its own deadline the same way. Skew beyond tolerance is an explicit, named agent report
condition (`clock_skew_exceeded`) carried in `ReportOperationResult` — never silent
rejection. The F0 harness exercises both directions (agent clock ahead/behind) so the
tolerance path is tested before the field freeze ships.

## 9. Fake engine and contracts (`capyctl-adapters`, `tests/harness`)

Adapter trait = SPEC §8.3 operation set: `inspect`, `render_plan`, `check_readiness`,
`prepare_park`, `park`, `restore`, `reload_weights`, `observe_work`, `cancel_work`.
Launcher trait owns
spawn/terminate/exit reporting with PID + start-identity handles; PID reuse is detected
(SPEC §13.2).

Fake-engine simulator capabilities:

- Configurable slow startup (readiness after N ms; liveness before readiness is not
  readiness).
- Sleep semantics: level 1 retains a CPU weight backup; level 2 discards weights and KV
  while retaining some buffers; `reload_weights` collective invoked exactly once through
  the designated lead.
- Crash injection at any phase (startup, ready, parking, restore).
- Ambiguous outcomes: effect applied but acknowledgement lost (reconcile, don't blindly
  repeat).
- Retained memory/cache reporting per level and per allocation class.
- Cancellation: explicit acknowledgement semantics; otherwise report uncertainty.

The fake adapter sits beside future vLLM/SGLang adapters behind the same trait; the
conformance suite runs against `fake` first, giving F1/F2 their regression base without
GPUs.

Security gate (carried from SPEC §9.1): deep-park and collective-control operations
(level-2 park semantics, `reload_weights`) are experimental and denied by default; they
are enabled only by explicit host policy opt-in (T21). The conformance suite covers
default-denial behavior from F0 onward, so the gate is regression-tested before any real
adapter lands.

## 10. Toolchain

Stable Rust. `tokio` async runtime; `tonic` + `prost` for gRPC; `rusqlite` with explicit
transactions for predictable transactional semantics (wrapped in a small async facade);
`serde` with a strict YAML loader built on `saphyr-parser`/`yaml-rust2` feeding serde
types — duplicate-key detection is required and `serde_yaml` is archived (it resolves
duplicate keys last-wins); `clap` for the CLI grammar; `ulid` for
deployment IDs. Dev/test: `cargo test`, `proptest` for transition-table properties,
`tempfile` for per-test state dirs. Exact crate versions are pinned in the implementation
plan.

## 11. F0 exit gate

State, allocation, idempotency, and bootstrap/default tests pass without GPUs (SPEC §18):
T01–T04, T08, T09, T26, T27 green; fake-engine scenario suite green; proto compiles,
is version-checked, and passes a wire-level AgentControl round-trip over a real gRPC
channel against the fake engine — the freeze is declared only after stream evidence, so
F3 adds no compatibility shims; `capyctl start standalone` boots an embedded server + host
against the store with a fake engine deployment completing the full lifecycle STOPPED →
STARTING → READY → DRAINING → PARKING → PARKED → WAKING → READY and STOPPED.

## Appendix A — Frozen proto skeleton

```protobuf
syntax = "proto3";
package capyctl.management.v1;

// Common envelope for every command and report.
message Envelope {
  string host_id = 1;
  string deployment_id = 2;     // empty for host-scoped operations
  int64  generation = 3;
  string operation_id = 4;
  int64  deadline_unix_ms = 5;
  string protocol_version = 6;  // "1"
  string expected_state = 7;    // expected observed-state precondition
  string profile_fingerprint = 8; // expected runtime build fingerprint
}

// --- Bootstrap (server-authenticated enrollment; exercised in F3) ---
service Bootstrap {
  rpc Enroll(EnrollRequest) returns (EnrollResponse);
}
message EnrollRequest {
  string invitation_id = 1;
  string invitation_secret = 2;   // single-use; server verifies + burns
  string host_name = 3;
  bytes  client_public_key = 4;   // host-generated key retained locally
}
message EnrollResponse {
  string host_id = 1;
  string server_certificate = 2;  // CA/trust material for pinning
  int64  lease_expires_unix = 3;
}

// --- Agent control (agent-initiated bidirectional stream) ---
service AgentControl {
  rpc Session(stream AgentToServer) returns (stream ServerToAgent);
}

message AgentToServer {
  oneof msg {
    Connect connect = 1;
    ReportInventory report_inventory = 2;
    ReportOperationResult report_operation_result = 3;
  }
}
message Connect {
  string host_id = 1;
  string protocol_version = 2;
  bytes  journal_resume_token = 3;   // bounded operation history
  Envelope envelope = 4;
}
message ReportInventory {
  repeated DomainObservation domains = 1;
  repeated RuntimeProfileStatus profiles = 2;
  Envelope envelope = 3;
}
message DomainObservation {
  string domain_id = 1;        // stable local identity
  string kind = 2;             // system | device_memory | filesystem | remote_storage
  int64  observed_bytes = 3;   // -1 = unknown (closes admission upstream)
  int64  observed_at_unix = 4;
}
message RuntimeProfileStatus {
  string name = 1;
  string build_fingerprint = 2;
  string eligibility = 3;      // unknown | qualified | unsupported | disabled
  string reason = 4;
}
message ReportOperationResult {
  string operation_id = 1;
  string state = 2;            // accepted | rejected | applied | failed | ambiguous
  string error_code = 3;
  string evidence_json = 4;    // observations, exit status, released bytes
  Envelope envelope = 5;
}

message ServerToAgent {
  oneof msg {
    LaunchMember launch_member = 1;
    OpenIngressGate open_ingress_gate = 2;
    Inspect inspect = 3;
    GroupControl group_control = 4;
    TerminateMember terminate_member = 5;
    CancelWork cancel_work = 6;
  }
}
message LaunchMember {
  string profile_name = 1;
  string profile_fingerprint = 2;
  ResourcePlan plan = 3;
  string rendered_command_json = 4;   // full argv + env, secrets redacted upstream
  string role = 5;                    // head | worker | ingress
  string member_id = 6;
  Envelope envelope = 7;              // appended pre-freeze; additive, never renumbered
}
message OpenIngressGate {
  string member_id = 1;
  int64  generation = 2;
  string router_identity_fingerprint = 3;
  Envelope envelope = 4;
}
message Inspect {
  string member_id = 1;              // empty = whole host
  Envelope envelope = 2;
}
message GroupControl {
  string deployment_id = 1;
  int64  generation = 2;
  string action = 3;                 // prepare_park | park | restore | reload_weights
  string lead_member_id = 4;         // collective invoked once through the lead
  Envelope envelope = 5;
}
message TerminateMember {
  string member_id = 1;
  string owned_handle = 2;           // PID + start identity / container id
  int32  grace_period_seconds = 3;
  Envelope envelope = 4;
}
message CancelWork {
  string member_id = 1;
  string request_ref = 2;            // backend-issued request identity
  bool   require_acknowledgement = 3;
  Envelope envelope = 4;
}
message ResourcePlan {
  repeated DomainAllocation allocations = 1;
  repeated string exclusive_devices = 2;
}
message DomainAllocation {
  string domain_id = 1;
  int64  bytes = 2;
  string phase = 3;                  // activation | ready | parked
}
```

Field numbers are frozen; additive evolution only (new oneof arms or appended fields),
never renumbering.