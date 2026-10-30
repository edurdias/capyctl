# Status: Engine log of a launch that failed before readiness — 2026-10-30 (branch `fix/engine-log-failed-launch`)

Issue #226. `GET /management/v1/deployments/<id>/engine-log` answered
`404 not_found` ("has no running launch") for a launch whose engine exited
before readiness. A failed launch's binding is released when it fails, and the
read looked only for a binding that had not been released. It did so with
`engine_logs: files` or `memory`, standalone or remote. The operations guide
promised the log ("The engine log says why the engine exited"), and the
`engine_exited` diagnostic sends users to this read.

SPEC §13.3 / T21. The read now names the instance's latest launch:
`engine_logs::launch_scope` still prefers the retained binding of the
instance's current generation. When there is none, it takes the newest
released binding of that generation (`Store::released_launch_at`). Only the
initialize step that launched a binding ties it to a generation: a stop draws
a new generation and steps on the launch it ends, so a stopped instance names
no launch and an earlier generation's launch is never named. A released remote launch is read on the
host its ingress was frozen to before it was sent (`remote_binding_ingress`),
not the instance's current host, so a retry of the same generation placed on
another host does not send the read there. On a host, the `EngineLogTail` gate was
"a launch this host still claims". It is now "a launch this host accepted for
the deployment": one it claims, or one at the command's generation whose claim
a Terminate released (`Journal::accepted_launch`). Nothing else changed: the
engine log's redaction, bounds and closed refusals, the `--debug-engine-logs`
refusal, and the 60-second linger of `engine_logs: memory`, after which the
read answers `404` (no log). `ScopeError::NotRunning` and
`EngineLogFailure::NotRunning` are renamed `NoLaunch`, and the refusal reads
"has no launch". A host running an earlier CapyCTL still answers `missing` for
a released launch, so the server answers `404` for it.

Tests, failing before and passing after:
`capyctl-cli/tests/engine_exit.rs`
`a_standalone_launch_that_failed_before_ready_serves_its_file_log`,
`…_serves_its_memory_log` and
`a_remote_launch_that_failed_before_ready_serves_its_engine_log` (a fake vLLM
that prints one line and exits 1 before serving; before the fix, all three
got `404 not_found`); `capyctl-store`
`a_released_failed_launch_is_named_for_its_generation`; `capyctl-controller`
`a_stopped_launch_is_not_named_for_the_stops_generation` (before the
initialize-only tie it named the stopped launch); `capyctl-agent`
`engine_log_tail_serves_an_accepted_launch_bounded_and_redacted` (now also
reads after the launch's Terminate, and refuses another generation).

CPU and Fake-engine tests are not qualification. Live check still needed: on
a GB10 host with `engine_logs: memory`, a launch that fails before readiness
returns its tail from the read within 60 seconds of the failure.

# Release note: Fixes

- **The engine log of a launch that failed is readable.** The management
  engine-log read answered `404` for a launch whose engine exited before it was
  ready, so the one record of why it failed could not be read through the API.
  It now returns the end of the instance's latest launch log, including a
  failed one (with `engine_logs: memory`, for 60 seconds after the engine
  exited). See [engine logs](../operations/install.md#engine-logs-and-troubleshooting).
