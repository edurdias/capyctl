# Status: Guide: driving CapyCTL from a controller — 2026-10-20 (branch `docs/controller-guide`)

Documentation only; no code changes. Owner decision of 2026-10-20: an outside
controller is one more user of the existing management API (SPEC §14: the API
is the source of semantics for the CLI and integrations), with no special
mode, and requests still start and wake models unless
`lifecycle.activation: explicit` is set (SPEC §6.5, §10). Idempotency keys and
revision preconditions are SPEC §6.4's. The new guide page,
`docs/guide/controller.md`, describes that use: the admin token and the
loopback base path; the loop of reading `GET /management/v1/snapshot` (what
each field a controller needs means, and how an instance's `observed_state`
maps to what is running), creating with `POST /management/v1/deployments`
(`activate: false`, then a start; `activate: true` is answered `503
unsupported_capability`), replacing with `PUT /management/v1/deployments/{id}`
and `expected_revision`, and acting with
`POST /management/v1/deployments/{id}/actions` (`start`, `stop`, `park`,
`preinitialize`, `delete`, with `deadline_ms` an absolute Unix time taken from
the snapshot's `timeouts`); following `GET /management/v1/events` from the
snapshot's cursor; `Idempotency-Key` retries (same key and body replays the
first answer, a different body is `409 idempotency_conflict`, a refused change
does not use up its key); `409 revision_conflict` on a stale revision; request
IDs, which belong to the inference endpoint while the management API
correlates by key and `operation_id`; which codes to retry, fix or stop on, by
`retryable`; `lifecycle.activation: explicit`, `max_pending_per_deployment: 0`
and `--no-inference-auth` for a controller that decides everything; and a
worked `curl` and `jq` script. The page is linked from the guide index,
`docs/README.md` and the site's Tasks sidebar (`site/scripts/pages.mjs`,
`site/astro.config.mjs`).

Every request and answer shown was taken from the handlers in
`crates/capyctl-management/src` and checked against a local
`capyctl start standalone` built from `main` (`fdbca58`) with the site's
stand-in engine (`site/scripts/transcript/fake-vllm.py`), which loads nothing:
create, replay and conflicting replay, `activate: true`, an unpinned `hf:`
reference and a `~/` path, `route_conflict`, stale and current revisions,
start (with `checkpoint_digest_pending` and its retry), stop, park,
preinitialize, delete before and after the stop, `runtime_retained`,
deadlines out of range, a string revision, the event stream and its cursor,
`deployment_stopped` and `deployment_inactive` on the inference endpoint, and
the worked script end to end, whose output the page shows. CPU and Fake-engine
runs are not qualification; no native engine was run for this page, and none
is needed for a documentation change.

# Release note: Operations

- **Driving CapyCTL from a controller.** A new guide page shows how a program
  uses the management API to create, start, watch, replace and delete
  deployments: the admin token, the snapshot, idempotency keys and retries,
  revisions, the event stream, request IDs, which errors to retry, and how to
  leave every start to the controller. See
  [Driving CapyCTL from a controller](../guide/controller.md).
