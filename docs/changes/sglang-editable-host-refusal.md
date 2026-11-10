# Status: Enrolled hosts refuse editable SGLang toward a server that accepts the code — 2026-11-10 (branch `integration/0.2.0-on-0.1.11`)

PR #235, ADR 0017 and ADR 0008's 2026-10-30 amendment: a 0.2.0 host refuses
an editable SGLang at its shared launch-time installation check, under either
drift policy. The server advertises `installation_unsupported` in
SessionReady, and the host declares it in Connect. The host reports
`installation_unsupported:editable` only when that server advertises support.
Before SessionReady, and toward a 0.1.x server, the host keeps 0.1.11's
unmeasured/start/import-failure path; every older refusal still passes.
A reconnect replaces the capability list before the new session's commands.
The 0.1.11 refusal at engine add and in standalone is unchanged.

The existing PR's gate is carried into the integration with T21/T22/T34
regressions for all editable layouts, both drift policies, no SessionReady,
an older capability list, the current list and replacement by an older list.
Tests were authored or strengthened before production edits. Integrated
verification is owned by the integration's deep CI run. CPU and Fake-engine
tests are not qualification.

Still owed live: a 0.2.0 server and host refuse the editable SGLang before
starting an engine; a 0.1.x server with a 0.2.0 host sees engine_import_failed
and keeps its control session, without reconnecting or affecting other models.

# Release note: Fixes

- **An enrolled host refuses an editable SGLang before starting it.** This
  needs both server and host on 0.2.0. Toward an older server the engine still
  starts and fails with `engine_import_failed`, without ending the control
  session. Reinstall SGLang without `-e`.
