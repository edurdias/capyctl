# F2 local management boundary

Status: preparatory library routers with owned Fake execution. The production
listener, CLI cutover, remaining lifecycle actions, public operation-result reads
and complete inventory are not wired. No native execution or qualification is
enabled by this composition.

The trusted service must mount one router on its separate loopback management
listener, using independently resolved management and inference credentials.
Never mount management routes on the inference listener. Existing read-only and
stopped-configuration router constructors retain their narrower capabilities.

`candidate_acceptance_router` adds `POST /management/v1/qualification-runs` to
stopped configuration commands, historical snapshots and durable SSE. Its
`SharedConfigurationSource` uses the coordinator's exact owned Store/session and
retains the service lock. It does not open another database, rotate sessions,
import host policy or construct a runtime driver.

Candidate creation requires a management bearer credential, a single
`Idempotency-Key`, and a strict JSON command containing `host_id`,
`expected_host_revision`, `recipe_digest`, `manifest`, `deadline_ms` and
`allow_owned_abort_cleanup`. Request revisions and deadlines are JSON integers;
response revisions are decimal strings. Unknown fields, duplicate keys and
caller-supplied evidence are rejected. The reviewed manifest digest must match
its content and be allowed by the current locally imported qualification policy.
Management authentication alone never enables qualification.

Successful creation returns HTTP 202 with `api_version`, `operation_id`,
`deployment_id`, `qualification_run_id`, `revision` and `joined:false`. The Store
has already committed the scoped run, fresh binding/endpoint reservation,
operation, receipt and event. No ordinary route, execution grant, process launch
or inference request is created. Exact-key retries return the original receipt,
including after qualification policy revocation; a new key uses current policy.
A receipt grants no new execution authority.

`lifecycle_router` adds ordinary Start/Stop, candidate Initialize/Park/Restore/Finish/Abort and
candidate corpus inference. Its `OwnedActionSource` must share the exact configuration
source's owned Store and coordinator session. The service retains the
`OwnedCoordinator` outside the router; constructing a router does not start a
listener. The narrower constructors above gain no execution capability.

Ordinary actions use `POST /management/v1/deployments/{id}/actions` with
`expected_revision`, `action` (`start` or `stop`), and `deadline_ms`. The current
owned execution path supports qualified Fake initialization and verified cleanup.
Stop before Initialize arms releases only after the old worker exits and an atomic
no-effect proof succeeds. Unknown ownership or uncertain effects retain accounting.

Candidate Initialize uses `POST /management/v1/qualification-runs/{id}/actions`
with `expected_revision`, `action:"initialize"`, and `deadline_ms`. The owned
worker executes Initialize and the separately armed mandatory Ready probe through
one retained Fake instance. Candidate inference uses the run's `/inference` path
with `expected_revision` and a strict `request` object; it cannot override the
original run deadline or select a different model, prompt, stream mode or token
limit from the next reviewed corpus item. Both commands require idempotency keys.

After all four baseline corpus results, the owned worker runs the fixed internal
Security checks through that same retained Fake. There is no public Security
action. Each check requires a new durable arm and current policy, lease,
observation and clock fences; recorded uncertainty cannot authorize a replay.

Candidate Park and Restore use the same action endpoint and envelope with
`action:"park"` or `action:"restore"`. Park requires completed predecessor
coverage and separately executes Drain, Park and a read-only parked-status check.
Restore separately executes allocation restore, weight reload, cache invalidation
and the required Ready probe before post-wake corpus inference. Each effect is
armed independently. Shutdown rechecks under the owned Store lock prevent a new
arm after admission closes. Full candidate ownership remains retained throughout.

Candidate Finish uses `action:"finish"` with the same strict revision/deadline/key
envelope. Its V4 catalog and command receipt share one atomic record containing
the deadline; legacy V3 records retain their original serialization and hash.
The existing exact suite evaluator validates every required source before the
transaction records qualification. Service clocks are sampled after evaluation
and immediately before commit; expiry or regression rolls back completion.
Finish returns the original operation envelope without a runtime step. Exact
same-key retries survive expiry, policy closure and current-session replacement;
a different service key after completion conflicts. Completed immutable evidence
from an older session remains eligible for validation by a current coordinator.
Finish neither releases resources nor adopts the candidate into ordinary routing.
Verified cleanup and a fresh qualified binding remain required for ordinary use.

Candidate Abort uses `action:"abort"` with the same strict revision/deadline/key
envelope. It atomically closes an accepted, running or uncertain retained run and
records its own operation, receipt and event. New Abort deadlines must remain
inside the original run deadline. Passed, failed, expired or already aborted
runs reject a new key; exact-key retries preserve the original acceptance after
expiry and current admission closure. Abort cannot rewrite a passed catalog.

Owned execution serializes Abort acceptance with candidate future polling, fences
new Store arms and drops the matching in-flight child before later work. Late
success cannot reopen Ready or qualify the run. The original runtime, all resource
grants, endpoint reservations, armed children and unsettled leases remain retained;
Abort neither terminates the runtime nor creates a resource completion epoch.
Its durable event replays through authenticated SSE with only exact operation,
deployment and run IDs plus a positive session epoch string. Retrying the action
does not duplicate the event or interrupt later live delivery.
Cleanup remains a separate authority and an outstanding worker integration.
Fatal Store errors and an already stopped worker still deny new commands.

Inference returns the original HTTP202 operation acceptance envelope, not a
completion or response body. A bounded owned queue retains accepted work after
caller loss. Only a New durable grant may send; retries never replay uncertain
backend inference. The Fake program uses exactly `MLLM_ALPHA_71` and `MLLM_BETA_29`
in nonstreaming and streaming modes with max_tokens16. These are not the separate
native F2C corpus or qualification counts. Full candidate grants remain unchanged,
ordinary gates stay closed, and inference itself cannot promote. Public result reads and
remaining candidate actions are still separate integration requirements.

All mutation routes share two in-flight command permits. Command bodies are
limited to 1 MiB with a five-second body deadline. A fifteen-second response
deadline or client disconnect does not cancel started blocking work or release
its permit early. Retry an uncertain submission with the original key. Read/SSE
capacity is separately bounded. Responses disable caching and never expose raw
Store/provider diagnostics.

Verified at the owned Abort slice: all 66 management tests pass; final checked
Store/controller/management sources pass 508 distinct tests, with four-crate
all-target Clippy passing. Management was rerun after the final SSE-only repair;
unchanged Store/controller results come from the preceding full run. Tests cover
real owned Store acceptance for Fake, vLLM and SGLang manifests without engine
execution, historical replay, revocation, malformed input, endpoint exhaustion,
stale sessions, shared cancellation limits, owned Fake execution, exact corpus
progression, the full real TCP warm cycle and response-loss retries. This is
CPU/Fake/API evidence only.
