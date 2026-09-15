# Management reads and owned command submission

This crate implements authenticated `GET /management/v1/snapshot` and optional
durable SSE at `GET /management/v1/events` through `read_only_router`.
The response retains `scope: "durable_store_foundation"`: it is historical
durable state, not complete A3 status, fresh ownership proof or readiness.
Narrow read constructors expose only those routes. Optional constructors compose
stopped configuration, candidate creation, and owned Start/Stop submission.
No constructor starts a listener or exposes inference routes.

`lifecycle_router` adds `POST /management/v1/deployments/{id}/actions` to the
configuration and candidate boundaries. Construct `OwnedActionSource` from a
`SharedConfigurationSource` and the existing worker's `CoordinatorCommands`;
construction rejects different owned Store/session authorities. Keep the
`OwnedCoordinator` owner outside the router for its full application lifetime.
The source reuses the configured principal and worker clock and never creates
a session, driver, database connection, or runtime adapter.

Actions require one `Idempotency-Key` header and exactly `expected_revision`,
`action`, and `deadline_ms`. Revisions and absolute deadlines are positive JSON
integers. `start` and `stop` use existing qualified Fake lifecycle authority;
`park`, `suspend`, `resume`, and `undeploy` remain unsupported. Other actions,
extra fields, duplicate fields, noncanonical ULIDs, and query strings are invalid.
Stop resolves generation inside its acceptance transaction after exact history
lookup. No new cleanup authority or support for attached/native targets is added.

Successful submission returns HTTP 202 with only `api_version`, `operation_id`,
`deployment_id`, `joined`, and decimal-string `revision`. This proves committed
acceptance; clients observe lifecycle completion separately. Exact historical
retries retain the original revision/deadline identity after cleanup, replacement,
or worker shutdown, with a current session. Changed bodies under the same action
scope/key conflict. A closed worker admits no fresh Start or Stop.

All mutation routes share two command slots, a 1 MiB body limit, a five-second
body deadline, and a fifteen-second response deadline. Cancellation or response
timeout retains the slot until started blocking work exits. Retry with the same
key after an ambiguous response. Narrower constructors acquire no lifecycle
authority implicitly.

The trusted service must supply independently generated management and inference
credentials through `ManagementCredentials::from_trusted_resolver`. Both must
be 32–256 ASCII bearer-token characters; equal credentials are refused. The
constructor validates syntax and separation, NOT entropy or provenance. Never
resolve credentials from public deployment configuration or HTTP inputs.
`ManagementCredentials::from_protected_files` resolves two explicit service file
paths without creation, rotation, repair, or fallback. Each file contains one
token with at most one trailing LF (257 bytes maximum). Paths must be absolute
and canonical; protected ancestors must be root/service owned, and files must
be service-owned, single-link regular files with mode 0600. Descriptor/path
identities and permissions are rechecked after both reads. Existing F0 credential
files are not repurposed by this loader. It checks provenance, not entropy.
The credential object retains only a
SHA-256 management-token digest, compared with `subtle` constant-time equality;
it implements neither Debug nor Serialize. Missing, duplicate and malformed
Authorization fields and inference credentials fail before provider access.

`snapshot_router` remains snapshot-only (events return 404). `read_only_router`
adds SSE using a provider implementing both `SnapshotSource` and `EventSource`;
`StoreSnapshotSource` implements both. Either router must be mounted exclusively
on a separate management listener.
Production composition must enforce loopback or configured TLS, distinct ports,
protected credential resolution, transport limits and shutdown. This crate does
not bind a listener or provide an insecure remote-listener override. No CORS or
cookie authentication is enabled.

`StoreSnapshotSource` owns an already-open Store; requests do not open databases,
run migrations or begin coordinator sessions. Snapshots use Store's bounded
coherent read transaction. Event replay uses `Store::events_after`, which prunes
retention in an immediate transaction: this is lifecycle-read-only, not a
read-only SQLite connection. Each router admits at most two blocking reads shared
by snapshots and events, including
cancelled requests whose workers are still running; excess requests get versioned
429 `queue_full`. Instantiate one router for the service, not one per request.
Provider errors are mapped to fixed versioned errors without source diagnostics.
No raw credentials or errors are serialized. Responses disable caching.

## Event protocol and bounds

The optional `after` query parameter and single `Last-Event-ID` header use
`database_incarnation:sequence`. If both occur, their decoded values must match
exactly. Extra query keys, duplicates, malformed percent encoding, non-ULID
incarnations, negative/overflow sequences, and cursors over 46 bytes are rejected
before provider access. Authentication runs before all provider work.
Preflight replay happens before headers: expired or foreign-incarnation cursors
return versioned 410 `cursor_expired` with `resnapshot_required: true`; malformed
or future cursors return 400 `invalid_cursor`. Corrupt payloads and provider
failures return sanitized 500 `internal`. No cursor means retained history.

Each SSE frame has `id: <cursor>`, `event: <validated Store kind>`, and JSON `data`
with `api_version: "1"`, `recorded_at_ms`, nullable deployment/operation IDs, and
`payload`. Every integer in this projection is a decimal string, including
nested revisions, generations, session/ledger/committed epochs and timestamps.
The payload is a closed schema matching existing typed Store metadata. Unknown
kinds/fields, duplicate JSON keys, invalid identities, mismatched transitions,
missing fields and malformed/oversized payloads fail closed. No arbitrary
payload, diagnostic, path, credential or prompt field is forwarded.

Default limits (per router): 16 streams, 2 queued/running blocking reads, 64
events and 256 KiB serialized SSE bytes per page, 8 outgoing frames, 5-second
send wait, 250-ms durable poll, 15-second heartbeat and 300-second total lifetime
(including preflight). Each raw payload is capped at 16 KiB. Only one read per
stream is outstanding. `read_only_router_with_event_options` accepts bounded
service tuning through `EventStreamOptions`; construction rejects zero values
and values beyond the documented maxima (poll maximum 15 seconds, heartbeat
maximum 60 seconds). No detached read queue grows behind cancellation.

Durable polling resumes after the last enqueued event, never a partial page's
high-water cursor. The client reconnects from its last received `id`, so buffered
or unsent frames can be replayed. A heartbeat is only `: heartbeat`, with no ID.
After-header expiry/failure emits `management_stream_error` with versioned JSON
and no ID when the bounded channel can accept it, then closes. Slow clients
disconnect when sending times out; no success or advanced ID is invented.
Client drop, timeout and lifetime expiry stop polling and release stream capacity;
already accepted blocking reads retain their global permits until completion.
The lifetime forces reconnection authentication, but fixed credential digests do
not provide live rotation, revocation or a separate authentication expiry claim.
Transport buffering and shutdown remain the listener owner's responsibility.

Existing event coverage includes managed configuration acceptance, candidate run
acceptance, initialize arm/accept, selected qualification/launch/ready/park/cleanup
transitions, qualified initialization acceptance/arm/owned launch/Ready commit/uncertainty,
coordinator session start and host resource/qualification policy
changes. This is not a complete lifecycle audit stream: full visible-transaction
writer coverage remains parent Task 3 work. The transport does not fill those gaps.

Remaining A3 work: protected service listener and credential-path composition,
complete snapshot projection and event writer coverage, remaining mutation and operation
routes, and CLI cutover.
