# F2A3 Management and Configuration Implementation Plan

**Goal:** Expose validated deployment configuration, lifecycle operations, snapshots, and resumable events through one authenticated API used by the CLI and future UI.

**Architecture:** Add a focused `mllm-management` Axum crate around the A2d coordinator. Keep host-agent gRPC unchanged; its remote transport remains F3. Typed effective configuration freezes every runtime binding before acceptance; transactional events share the same commits as visible state changes.

**Tech Stack:** Existing Rust 2021, Axum 0.8, reqwest 0.12, serde, rusqlite 0.37, clap 4, Tokio, SHA-256. Add no browser/UI implementation.

**Spec:** [F2 design](../../design/milestones/f2-sglang-design.md), §§3/5/7, Q1/Q9/Q10; depends on [A2d](2026-09-12-f2a2d-coordinator-integration.md).

## Global Constraints

- “The management API is the single semantic boundary for the CLI, future UI, and integrations.”
- “Management and inference credentials have separate authority; neither bypasses host policy.”
- “Status and list requests never activate engines.”
- “Accepted commands persist the operation and affected durable state before returning an ID.”
- “Material configuration changes require an explicit revision-aware operation.”
- “Retention is bounded; expired cursors explicitly require a fresh snapshot rather than silently omitting history.”
- Only host-a for eventual live work; no live work in these tasks. Preserve the unrelated live-interactive test.
- Normal inference remains HTTP. Existing host-agent protobuf field numbers remain unchanged.

---

## 1. Public API contract

Use a separate loopback management listener with an independently generated bearer
credential. The inference listener never mounts these routes. Default management
bind is `127.0.0.1:9091`; inference retains its configured listener. Reject port
collisions at startup. Non-loopback management requires configured TLS; do not
provide an insecure override in F2. No browser CORS or cookie authentication is
enabled by default. Future UI uses this API, not engine-admin passthrough.

JSON responses carry `api_version: "1"`. IDs and cursors are strings. Serialize
revision, generation, byte counts, and millisecond timestamps as decimal strings
to avoid JavaScript integer precision loss. Counts bounded to u32 remain numbers.
Unknown JSON fields are rejected on every command object. Max command body: 1 MiB.

| Method/path | Request | Result |
|---|---|---|
| `POST /management/v1/deployments` | `config` (strict deployment object), `activate` boolean; required `Idempotency-Key` | 202 accepted operation and deployment IDs |
| `PUT /management/v1/deployments/{id}` | `expected_revision`, `config`; required idempotency key | 202 revision operation; reject material mutation while retained runtime exists |
| `POST /management/v1/deployments/{id}/actions` | `expected_revision`, `action` (`start`, `park`, `stop`, `suspend`, `resume`, `undeploy`), `deadline_ms` | 202 accepted/joined operation |
| `POST /management/v1/preinitializations` | `members` ordered array of `{deployment_id,expected_revision,final_state}`, `deadline_ms`; idempotency key | 202 one sequence operation with per-member progress |
| `POST /management/v1/attachments` | `name`, `route`, `endpoint`, `credential_ref`, `resource_bound`, `fingerprint`; idempotency key | 202 attached routing operation, never process adoption |
| `POST /management/v1/attachments/{id}/detach` | `expected_revision`, `deadline_ms`; idempotency key | 202 route closure and mllm-work drain; no external engine control |
| `POST /management/v1/attachments/{id}/reconcile` | `expected_revision`, `action` (`refresh`, `retire`, `reattach`), optional `resource_bound`, `deadline_ms`; idempotency key | 202 verified external-accounting operation; never engine control |
| `PUT /management/v1/hosts/{id}/resource-policy` | `expected_revision`, strict `resource_policy`; idempotency key | 202 durable policy update operation and new policy revision |
| `POST /management/v1/qualification-runs` | `host_id`, `expected_host_revision`, `recipe_digest`, `manifest`, `deadline_ms`, `allow_owned_abort_cleanup`; idempotency key | 202 scoped candidate run and operation IDs |
| `POST /management/v1/qualification-runs/{id}/actions` | `expected_revision`, `action` (`initialize`, `park`, `restore`, `finish`, `abort`, `cleanup`), `deadline_ms`; idempotency key | 202 run-scoped operation; never ordinary warm authority |
| `POST /management/v1/qualification-runs/{id}/inference` | `expected_revision`, bounded OpenAI-compatible `request`; idempotency key | accounted run-only correctness request; no ordinary public route |
| `GET /management/v1/snapshot` | none | coherent full snapshot plus cursor |
| `GET /management/v1/deployments` | `limit` (1–100), opaque `after` | stable-ID ordered page of deployment snapshots |
| `GET /management/v1/deployments/{id}` | none | deployment snapshot and cursor |
| `GET /management/v1/operations/{id}` | none | operation state/progress and cursor |
| `GET /management/v1/events?after={cursor}` | optional `Last-Event-ID`; mismatch with query is invalid | authenticated ordered SSE replay followed by live events |
| `GET /management/v1/config/effective` | none | redacted effective local configuration and provenance |
| `GET /management/v1/inventory` | none | local capabilities, qualification IDs/reasons, observations |
| `GET /management/v1/qualifications/{id}` | none | redacted evidence metadata; no raw environment/secrets |

All mutations require idempotency keys, including action requests. Lifecycle and
configuration acceptance use this envelope:

```json
{"api_version":"1","operation_id":"01OP","deployment_id":"01DEP","joined":false,"revision":"1"}
```

Host operations set `deployment_id:null` and include `host_id`; qualification
operations also include `qualification_run_id`. `revision` names the target's
revision. Run inference returns 202 with this operation envelope, never resends
an uncertain backend request; its bounded result is read through the operation.
Store schema must support host-scoped operations without inventing deployments.

An operation result contains `operation_id`, `deployment_id`, `state`, `action`,
`accepted_at_ms`, `deadline_ms`, ordered `steps`, per-member `results`, and nullable
`error`. Step fields are `id`, `deployment_id`, `action`, `state`, `activation_mode`
(`cold`, `warm`, `none`, `unknown`), `started_at_ms`, `completed_at_ms`, `blocked_by`,
and `evidence_id`. Optional timestamps are null until observed, never zero-duration
success placeholders. Partial preparation failure remains a failed operation with
successful members visible.

Errors have one envelope:

```json
{"api_version":"1","error":{"code":"revision_conflict","message":"Expected revision does not match","retryable":false,"operation_id":null,"details":{"expected":"1","actual":"2"}}}
```

| HTTP | Stable codes |
|---|---|
| 400 | `invalid_config`, `invalid_request`, `invalid_cursor` |
| 401 | `unauthenticated` |
| 403 | `forbidden`, `host_policy_denied` |
| 404 | `not_found` |
| 409 | `revision_conflict`, `idempotency_conflict`, `route_conflict`, `lifecycle_conflict`, `runtime_retained` |
| 410 | `cursor_expired` (includes `resnapshot_required:true`) |
| 413 | `body_too_large` |
| 429 | `queue_full` |
| 503 | `capacity_blocked`, `observation_stale`, `reconciliation_required`, `unsupported_capability` |
| 504 | `deadline_exceeded` |
| 500 | `internal` (redacted; correlation ID only) |

Do not convert an accepted operation's later failure into a second submission.
Waiting is client-side observation of the same operation ID. `resume` removes an
administrative suspension; it does not bypass qualification or host policy.

### Candidate qualification authority

Host policy separately permits qualification runs and experimental controls.
Management authentication alone cannot enable either. Accept only reviewed
candidate manifests whose digest is allowlisted by that policy. Freeze engine,
checkpoint, hardware/environment fingerprints, all phase grants, bounded cases,
request count/body/token limits, deadlines, and cleanup permission before effects.
Run creation allocates its own managed deployment/binding; never adopt a retained
user deployment. Bind authorization to principal, host identity, run ID, immutable
recipe digest, and exact binding incarnation. Persist it; do not accept a client
claim that a recipe is Qualified or deserialize completion evidence from HTTP.

Only the run action and inference routes can exercise candidate controls. Use the
same coordinator claims, grants, dispatch leases, authentication, and pressure
checks as ordinary work. Run requests cannot use another recipe, host, deployment,
or deadline. Ordinary routing and automatic reclamation remain disabled while
eligibility is Unknown. Candidate completion retains the full conservative grant;
it does not shrink accounting to measured residue or qualify normal warm use.
The existing dispatch transaction receives trusted run scope from its authenticated
handler, rechecks persisted authority, and registers the same durable lease. Never
publish a candidate route or bypass dispatch accounting. Stream results preserve
ordered bounded chunks and terminal metadata in the run response for validation;
do not persist raw inference bodies in durable operation events or logs.

`finish` validates required case coverage and trusted collected evidence for the
exact fingerprints. Only a passing run writes Qualified evidence and derived safe
bounds. Abort, expiry, or failure closes run admission but never erases resources.
Cleanup requires the frozen explicit permission and verified owned identities;
uncertain cleanup retains reservations/endpoints. Failed runs cannot promote.
`cleanup` remains available after finish/abort/expiry only under that original
owned-runtime permission and a separately bounded cleanup deadline; it cannot
re-enable initialization or inference. Missing permission requires explicit owner
direction, not automatic permission extension.
Promotion does not mutate the retained candidate binding; ordinary use requires
verified candidate cleanup and a new binding against the qualified recipe.

### Host policy and attachment retirement

Host resource-policy updates affect only bounded resource controls listed in §2,
not credentials, engine pins, qualification allowlists, or host identity. Validate
physical-domain references and protected headroom before acceptance. Serialize
revision check, policy replacement, ledger epoch increment, operation/receipt, and
event in one immediate transaction, shared with admission. Preserve every existing
reservation and armed grant even if the new ceiling is below their sum. Report
overcommitted state; permit only safe draining/release and reject new increases.
Revalidate every pending step against the latest revision. Never stop engines or
claim memory release as a side effect of changing a ceiling. Restoring a previous
policy also requires the latest revision; stale restoration fails with 409.

Detach atomically checks attachment revision, closes its route and new dispatch,
and fences generation. Wait for confirmed terminal mllm work only. Timeout or
unknown work leaves the operation failed/uncertain and leases retained. Successful
detach tombstones routing ownership but preserves external resource accounting
as a non-reclaimable owner until explicit verified accounting reconciliation.
Never call external drain, park, stop, restart, or cancellation endpoints. Managed
deployment IDs return 409 `lifecycle_conflict`; stale revision returns 409
`revision_conflict`; missing IDs return 404. Retry returns the original operation.

External-accounting reconciliation uses a host-policy-approved read-only collector
for the recorded external identity. Clients cannot submit evidence. Refresh may
retain/increase a conservative bound; any reduction or retirement requires trusted
collector proof covering the complete accounted external runtime and excluding a
replacement under that identity. Endpoint silence, global free-memory changes, or
an operator assertion alone are insufficient. Missing collector support returns
`unsupported_capability`; uncertainty retains charge and endpoint ownership.
Reattach reuses the existing external owner, immutable endpoint/credential/fingerprint,
and charge; it does not allocate a duplicate owner. Require free original route,
settled prior mllm leases, and freshly verified external identity before reopening.
Changed external identity requires separate accounting, not overwriting retained
ownership. Commit evidence, bound/endpoint changes, revision, operation, receipt,
and event in one fenced transaction. No external control or process adoption.
Test retire-with-proof, refusal without proof, reattach without duplicate charging,
replacement identity, route conflict, and rollback. CLI exposes
`reconcile attachment ID --action ACTION --expected-revision REV --wait` through
this endpoint, with optional resource-bound file and normal idempotency handling.

## 2. Configuration contract

Retain schema version 1 and strict duplicate/unknown-key rejection. Expand the
existing allowlists and then deserialize typed inputs with `deny_unknown_fields`.
Missing required nested fields fail before any durable deployment or launch.
Environment values are explicit allowlisted runtime inputs, not arbitrary secret
exports. Profile revision changes never mutate existing effective bindings.

Deployment fields: `schema_version`, `kind: deployment`, `name`, `model`, `routes`
(nonempty unique strings), `runtime_profile`, `runtime_profile_revision`, `recipe`,
`residency` (`warm` or `restart_only`), `recovery` (`reconcile` or explicitly
`cold_restart`), `devices` (device ID plus shared/exclusive), and `resources`
(the five complete phase footprints). `model` is a canonical local checkpoint
identity with explicit content/revision fingerprint, not an arbitrary download URL.

Profile fields: `engine` (`vllm`, `sglang`, `fake`), `revision`, absolute
`executable`, exact `build_fingerprint`, `qualification_id`, approved `args`,
allowlisted `env`, `security` (`experimental_controls` boolean and secret references),
and `log_policy` (`max_file_bytes`, `retained_files`). Effective recipe fingerprints
include hardware/environment and generated mllm-owned behavioral launch settings.
Qualification identity covers engine build, checkpoint, hardware/environment,
device topology, parallelism, allocator/memory/KV settings, approved behavioral
arguments, and security/logging policy. Binding identity separately covers endpoint,
served-name value, credential reference, and incarnation. Do not hash secret values.
Changing only binding-specific values preserves qualification; changing authentication
mode or other behavioral/security settings invalidates affected evidence. Test candidate
promotion, verified cleanup, and ordinary deployment on a different port/served name
against the same qualification identity.

Host fields extend current `resource_policy`: physical domains with explicit
`managed_limit`, `free_reserve`, optional `host_kv_limit`, `parked_limit`;
`max_parked`, observation TTL, device sharing policy, endpoint port range,
and named runtime profiles. All byte/duration inputs use separate parsers: memory
accepts B/KiB/MiB/GiB/TiB; time accepts ms/s/m/h. Reject fractional bytes after
conversion, negative/nonfinite values, overflow, and a time unit in a byte field.
Do not use floating-point arithmetic to convert resource quantities.

Default bounds: 64 pending requests per deployment, 256 total, 64 MiB total queued
body bytes, request waiting deadline 600 s, non-resetting admission window 2 s,
observation TTL 2 s, 4096 planner states, max 16 parked runtimes. These are tunable
bounded product defaults, not live performance promises. A deployment may select
a shorter deadline, never silently exceed host safety limits.

For a fresh unconfigured host, derive the managed ceiling only after observing
physical capacity. Default protected headroom is `max(16 GiB, ceil(capacity/5))`;
managed limit is capacity minus that headroom. Display derivation as provenance.
Unknown capacity leaves launches disabled. Unified CPU/GPU memory is one domain.

## Execution order with coordinator integration

Follow [A2d cross-plan execution order](2026-09-12-f2a2d-coordinator-integration.md#execution-order-across-a2d-and-a3).
After A2d Tasks 1–3, implement configuration foundations, V6 policy/run persistence,
and V7 event-writer foundations needed by atomic arm checks. Parent A3 tasks stay
open until all acceptance tests and integration work pass. Emit events in the same
transaction from each management writer's introduction. Complete remaining A3 work
alongside A2d dependencies; perform production wiring once, jointly with A2d Task 10.
No unvalidated constructor, temporary policy bypass, or second lifecycle authority.

## 3. Tasks

### Task 1: Typed effective configuration and launch ownership

**Files:** Modify `crates/mllm-config/src/schema.rs`, `strict_yaml.rs`, `lib.rs`;
create `src/effective.rs`, `tests/effective.rs`, `tests/fixtures/f2-deployment.yaml`;
modify adapter argument validators and `crates/mllm-cli/src/roles.rs`.

**Interfaces:** `resolve_effective(deployment: &serde_json::Value,
host: &serde_json::Value) -> Result<EffectiveDeployment, ConfigError>`;
`EffectiveDeployment` owns normalized immutable deployment/profile/recipe inputs.
Use A1 types internally and a versioned serializable DTO at persistence boundaries.

- [ ] Add the pure checked parser test before implementation:

```rust
#[test]
fn byte_parser_rejects_time_and_overflow() {
    use mllm_config::effective::parse_bytes;
    assert_eq!(parse_bytes("1.5KiB").unwrap(), 1536);
    assert!(parse_bytes("1ms").is_err());
    assert!(parse_bytes("0.1B").is_err());
    assert!(parse_bytes("9223372036854775808B").is_err());
}
```

- [ ] Run `cargo test -p mllm-config --test effective`; expect missing parser.
  Implement `parse_bytes(&str) -> Result<i64, ConfigError>` by splitting the known
  suffix, parsing the decimal numerator and power-of-ten denominator as checked
  i128, multiplying by the binary unit, requiring zero remainder, and checked
  conversion to nonnegative i64. Implement separate duration conversion to ms.
- [ ] Expand strict nested allowlists; validate every required typed field,
  checkpoint identity, phase recipe via A1, selected device/domain references,
  bounded queues, and profile qualification. Reject unknown profile/revision and
  unsupported effective combinations; no current environment fallback.
  Resolve candidate manifests through a separate typed input that requires the
  persisted run authorization; never add a public `skip_qualification` field to
  ordinary deployments. Candidate recipes remain Unknown in capability output.
- [ ] Reserve mllm-owned flags by normalized option name, including `--x=value`,
  aliases, duplicate values, and underscore/hyphen spellings supported by the
  selected engine. Own host/port/model/served-name/device/parallelism, memory/KV,
  authentication, memory-saver, and logging flags. Deny overrides before spawn.
  Engine-specific allowlists apply to remaining approved flags; no blanket pass-through.
- [ ] Add fixtures for all required fields, unknown nested keys, duplicate YAML
  keys, wrong scalar types, fingerprint drift, reserved-argument overrides, and
  profile-edit isolation. Run config and adapter argument tests to GREEN.
- [ ] Commit task files: `feat: resolve strict immutable deployment configuration`.

### Task 2: Revision-aware submission, unique routes, and idempotency

**Files:** Store `schema.rs`, `migrations.rs`, new `management.rs`, private
`management/tests.rs`, controller acceptance facade.
**Interfaces:** Submission takes validated effective input, authenticated principal
ID, idempotency key, and expected revision. Produces A2d `AcceptedRun` plus deployment
ID/revision, never a live process before durable acceptance.

- [ ] Add schema V6 and migration assertions, then run store management tests RED:

```sql
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
CREATE TABLE qualification_runs(
  id TEXT PRIMARY KEY,
  host_id TEXT NOT NULL,
  deployment_id TEXT NOT NULL REFERENCES deployments(id),
  revision INTEGER NOT NULL CHECK(revision>0),
  principal_id TEXT NOT NULL,
  recipe_digest TEXT NOT NULL,
  authorization_json TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('accepted','running','passed','failed','uncertain')),
  deadline_ms INTEGER NOT NULL
);
```

- [ ] Canonicalize effective semantic input before SHA-256 hashing. Same principal,
  scope, key, and body returns the original operation; altered body conflicts.
  Scope includes method and target. Retain receipts with operations; tombstone
  undeployed identities rather than silently reusing a historical key.
- [ ] Transactionally allocate deployment ID, unique routes, revision snapshot,
  operation, receipt, and event. Existing duplicate legacy routes fail migration
  reconciliation with IDs reported; do not choose whichever row sorts last.
- [ ] Permit configuration replacement only after owned runtime cleanup, with
  no retained endpoint/request leases. Retained runtime returns `runtime_retained`;
  user explicitly stops, then updates, then starts. Increment revision only for
  successful replacement. This is revision-aware update, not hidden hot mutation.
- [ ] In the same task add host-policy and candidate-run store methods. Test
  policy-update versus arm races, stale restoration, retained overcommit,
  candidate digest/host mismatch, expiry, idempotent run creation, failed promotion,
  and uncertain cleanup retaining every grant. Persist run evidence references
  and versioned bounded authorization JSON; secrets never enter these payloads.
- [ ] Test two connections racing route creation, identical retried submission,
  body mismatch, stale revision, retained runtime update, and failed transaction
  rollback. Run to GREEN; commit `feat: accept revision-aware idempotent commands`.

### Task 3: Atomic snapshots and bounded resumable events

**Files:** Store new `events.rs`, `tests/events.rs`; modify A2 lifecycle and
management transaction writers; new `crates/mllm-management/src/snapshot.rs`.
**Interfaces:** `snapshot()` returns a versioned state DTO and high-water cursor
from one read transaction; `events_after(cursor, limit)` returns an ordered page
or explicit expiry. Cursor format is `database_incarnation:sequence`.

- [ ] Add V7 DDL and a snapshot/event-gap race test:

```sql
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
```

- [ ] Initialize incarnation from ULID inside migration transaction. Append a
  redacted event in every visible state transaction, including session reset,
  claim/arm/completion, resource changes, admission changes, and operation failure.
  Use DB events as source; notifications only wake readers.
- [ ] Retain at most 100,000 events and 64 MiB payload bytes, max age 7 days,
  payload max 16 KiB. Prune transactionally and advance `retained_after` to the
  last deleted sequence. A cursor below that floor or from another incarnation
  returns 410; future sequence returns 400. Snapshot cursor is the global high
  water, even when rows were pruned. Restore a backup under a new incarnation
  to invalidate cursors whose old sequence could now be reused.
- [ ] Snapshot fields: deployment/revision/generation/routes; engine/checkpoint/
  recipe fingerprints; desired/observed/readiness; verified runtime identities;
  phase reservations/provenance; observations separately; host ceiling/headroom;
  operation/steps/blocked owners; capability and security status; supported actions
  with reasons; expected activation mode. Never expose bearer tokens, argv secrets,
  prompt bodies, or raw credential files. Compute hints from the same snapshot,
  but revalidate all mutation authority server-side.
- [ ] SSE event has `id` cursor, `event` kind, and versioned JSON data. Subscribe,
  query durable events after cursor, send ordered bounded batches, then wait for
  notification and query again. Slow clients get explicit disconnect/resnapshot
  behavior, never silent dropped events. Heartbeats carry no cursor advancement.
- [ ] Test write between snapshot and subscription, reconnect without mutation,
  pruning races, backup incarnation, auth expiry, event/state rollback, redaction,
  and slow reader. Run store event tests to GREEN; commit
  `feat: expose coherent snapshots and bounded event replay`.

### Task 4: Authenticated management handlers and attachment boundary

**Files:** New `crates/mllm-management/Cargo.toml`, `src/lib.rs`, `src/auth.rs`,
`src/handlers.rs`, `src/error.rs`, `tests/api.rs`; root workspace member; CLI roles.
**Interfaces:** `serve_management(coordinator: Arc<Coordinator>, credentials:
ManagementCredentials) -> axum::Router`. Credentials resolve protected references;
the router never parses a bearer secret from effective/public status data.

- [ ] Test unauthorized and inference-key requests receive 401/403 before a
  controller call. Test status/list/snapshot leave fake-driver invocation count
  zero. Run `cargo test -p mllm-management --test api`; expect RED.
- [ ] Mount exactly §1 routes, strict bounded JSON extractors, error mapping,
  versioned response DTOs, and command acceptance. Generate random distinct keys
  with an OS-backed random source; refuse startup with missing or insecure
  existing credentials. Never use the F1 hardcoded fallback. Use constant-time
  secret comparison and redact authorization at the tracing boundary.
- [ ] Attachment accepts only explicit local HTTP(S) endpoints allowed by host
  policy; deny redirects, credentials embedded in URLs, metadata addresses,
  arbitrary remote hosts, and path-based control tunneling. Probe only the
  approved external endpoint with its host-configured attachment credential reference.
  Bind each such reference to its permitted external endpoint identity; reject
  management keys and managed-runtime credentials before any request. Normalize
  resolved host/port and reject managed endpoint leases and managed reserved port
  ranges, including aliases. Pin allowed resolved addresses for subsequent connections
  so validation cannot drift through DNS changes. Validate attachment ownership
  atomically with endpoint registration; no managed endpoint may later be leased
  to an attachment. Test substitutions/aliases fail before probing or forwarding.
  Probe only the
  inference identity under a fixed conservative bound. Attached resources stay
  charged; no launch/park/stop/restart ownership. Detach disables route and drains
  mllm work but never stops the external server or claims its memory was freed.
- [ ] Undeploy closes routes and fences generation immediately, then performs
  authorized managed cleanup. Tombstone deployment after verified cleanup;
  preserve history and receipts. Never delete checkpoint/user cache files.
- [ ] Add HTTP tests for all three new boundaries: detach never invokes any
  external control; policy changes preserve grants and invalidate pending admission;
  candidate controls work only under exact run scope. Assert ordinary warm requests
  stay blocked before promotion, and management/inference credentials cannot
  fabricate evidence, substitute a recipe, or extend run authority.
- [ ] Test action retry, revision conflict, qualified/disabled capability reasons,
  attached routing and forbidden controls, separate listeners, body bounds,
  missing credentials, and no secret reflection. Run to GREEN; commit
  `feat: serve authenticated local management operations`.

### Task 5: CLI uses the API and exposes structured progress

**Files:** CLI `grammar.rs`, new `client.rs`, `output.rs`, `roles.rs`, `main.rs`,
`tests/grammar.rs`, new `tests/management.rs`.
**Interfaces:** CLI client takes management endpoint and protected credential
reference. It sends the same API DTOs; no direct store writes or alternate lifecycle.

- [ ] Extend grammar fixtures with these exact invocations:

```bash
mllm deploy model --file deployment.yaml --activate --wait
mllm inspect deployment MODEL --effective
mllm start deployment MODEL --expected-revision 1 --wait
mllm preinitialize deployment MODEL_A MODEL_B --final-state parked --wait
mllm status deployment MODEL --watch
mllm inspect config --effective
mllm list deployments --output json
mllm detach deployment ATTACHMENT --expected-revision 1 --wait
mllm update host host-a --resource-policy policy.yaml --expected-revision 1 --wait
```

- [ ] Add `update deployment ID --file FILE --expected-revision REV`,
  `attach deployment --file FILE`, `inspect operation ID`, and `watch events
  --after CURSOR`. All mutations accept `--idempotency-key`; generate one per
  invocation and reuse on transport retries. Expose it with operation ID so a
  user can repeat an uncertain submission. Never generate a new key on retry.
- [ ] Map HTTP errors to structured CLI codes; print operation ID immediately
  before waiting. Ctrl-C stops observation only. Watch reconnect resumes cursor;
  410 fetches a fresh snapshot and clearly reports the gap. Exit nonzero on
  operation failure or wait timeout without cancelling accepted work.
- [ ] Record request timing as admission/queue start, activation interval, backend
  dispatch, first token, backend terminal, and delivery end. Describe overlapping
  activation/queue intervals explicitly; do not add them as disjoint durations.
  Only metadata/timing is recorded by default.
- [ ] Exercise CLI against the real local management listener with fake engines:
  create/retry/start/park/prepare/stop/update/undeploy, attach/detach, conflict,
  reconnect, and failed preparation. No Spark or GPU. Run:

```bash
cargo test -p mllm-cli --test grammar --test errors --test management --lib
cargo test -p mllm-management
cargo test -p mllm-config -p mllm-store
git diff --check
```

- [ ] Commit `feat: drive management lifecycle through the product CLI`.

### Task 6: Bound logs and complete secure composition

**Files:** Launcher `exec.rs`, new `logs.rs`, `tests/logs.rs`; CLI roles;
`docs/runbooks/f2-local-management.md`.

- [ ] Add a child-process fixture writing more than its allowed log budget.
  Assert retained files/bytes stay bounded and credentials are not persisted.
- [ ] Pipe stdout/stderr through bounded rotating files, default 8 MiB each,
  four retained files per deployment. Do not let a slow log reader block the
  lifecycle indefinitely or accumulate an unbounded channel. Redact known secrets
  before persistence; prevent engine startup from printing credentials by launch
  configuration. Log filtering is defense in depth, not permission to put secrets
  into argv or broad environment dumps.
- [ ] Complete A2d production composition with validated A3 bindings and distinct
  API authorities. Document local commands, cursor expiry, warm/stop semantics,
  update preconditions, attachment limits, and unsupported capabilities.
- [ ] Run named local tests and Clippy for changed library/binary targets, excluding
  the unrelated live-interactive target. Commit `feat: bound runtime logs and finalize local management wiring`.

## 4. Review boundary

This plan freezes the F2 user-management HTTP contract without changing F3 agent
transport or implementing a UI. Review with A2d before execution, particularly
event atomicity, credential delivery, attachment accounting, and revision fencing.
No implementation or passing product tests are claimed by this document.
