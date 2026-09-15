# F2 local management boundary

Status: preparatory library routers only. The production listener, CLI cutover,
lifecycle actions, candidate execution/inference and complete public inventory
are not wired. These APIs do not start an engine or qualify a native runtime.

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

All mutation routes share two in-flight command permits. Command bodies are
limited to 1 MiB with a five-second body deadline. A fifteen-second response
deadline or client disconnect does not cancel started blocking work or release
its permit early. Retry an uncertain submission with the original key. Read/SSE
capacity is separately bounded. Responses disable caching and never expose raw
Store/provider diagnostics.

Verified locally: all 42 management tests and all-target Clippy pass. Tests cover
real owned Store acceptance for Fake, vLLM and SGLang manifests without engine
execution, historical replay, revocation, malformed input, endpoint exhaustion,
stale sessions and shared cancellation limits. This is CPU/API evidence only.
