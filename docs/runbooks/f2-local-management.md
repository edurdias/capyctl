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

`lifecycle_router` adds ordinary Start/Stop, candidate Initialize and candidate
corpus inference. Its `OwnedActionSource` must share the exact configuration
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

Inference returns the original HTTP202 operation acceptance envelope, not a
completion or response body. A bounded owned queue retains accepted work after
caller loss. Only a New durable grant may send; retries never replay uncertain
backend inference. The Fake program uses exactly `MLLM_ALPHA_71` and `MLLM_BETA_29`
in nonstreaming and streaming modes with max_tokens16. These are not the separate
native F2C corpus or qualification counts. Full candidate grants remain unchanged,
ordinary gates stay closed, and no promotion occurs. Public result reads and
remaining candidate actions are still separate integration requirements.

All mutation routes share two in-flight command permits. Command bodies are
limited to 1 MiB with a five-second body deadline. A fifteen-second response
deadline or client disconnect does not cancel started blocking work or release
its permit early. Retry an uncertain submission with the original key. Read/SSE
capacity is separately bounded. Responses disable caching and never expose raw
Store/provider diagnostics.

Verified at the owned inference slice: all 60 management tests and all-target
Clippy pass; the complete Store/controller/management run passes 466 distinct
tests. Tests cover
real owned Store acceptance for Fake, vLLM and SGLang manifests without engine
execution, historical replay, revocation, malformed input, endpoint exhaustion,
stale sessions, shared cancellation limits, owned Fake execution, exact corpus
progression and real TCP response-loss retries. This is CPU/Fake/API evidence only.
