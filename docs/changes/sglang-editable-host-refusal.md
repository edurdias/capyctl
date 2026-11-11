# Status: Enrolled hosts refuse editable SGLang toward a server that accepts the code — 2026-11-10 (branch `integration/0.2.0-on-0.1.11`)

PR #235, ADR 0017 and ADR 0008's 2026-10-30 amendment: a 0.2.0 host refuses
an editable SGLang at its shared launch-time installation check, under either
drift policy. The server advertises `installation_unsupported` in
SessionReady, and the host declares it in Connect. The host reports
`installation_unsupported:editable` only when that server advertises support.
Before SessionReady, and toward a compatible server lacking that capability,
the host keeps the unmeasured/start/import-failure path; every older refusal still passes.
A reconnect replaces the capability list before the new session's commands.
The 0.1.11 refusal at engine add and in standalone is unchanged.

The existing PR's gate is carried into the integration with T21/T22/T34
regressions for all editable layouts, both drift policies, no SessionReady,
an older capability list, the current list and replacement by an older list.
Tests were authored or strengthened before production edits. Integrated
verification is owned by the integration's deep CI run. CPU and Fake-engine
tests are not qualification.

The live #235 run exposed a controller registration defect before that gate:
`inventory_shape_valid` treated `build_fingerprint` as an identifier and
rejected the documented display form `sglang 0.5.23`. The restarted host
therefore repeatedly received an authorization rejection, without reaching
SessionReady or the installation check. The controller now accepts bounded
printable ASCII fingerprints, including spaces, while retaining strict profile
names and rejecting empty, oversized and control-character fingerprints.
The T21/T22/T34 regression
`editable_profile_display_fingerprint_does_not_block_registration` reproduces
the pre-fix authorization failure with the live profile's fingerprint and passes
after the fix; all four inventory/memory-source validation tests pass.

Still owed live: a 0.2.0 server and host refuse the editable SGLang before
starting an engine. A released 0.1.x server cannot exercise the import-failure
fallback with a 0.2.0 host: ADR 0017 §2 and
`crates/capyctl-protocol/src/version.rs` (`assess`) refuse a newer-minor host
with `host_version_newer_than_server`, before installation capability gating.
The import-failure fallback applies only to an otherwise compatible server.

# Release note: Fixes

- **An enrolled host refuses an editable SGLang before starting it.** This
  needs both server and host on 0.2.0. A compatible server without the refusal
  capability retains the `engine_import_failed` path. A 0.1.x server refuses
  a 0.2.0 host: upgrade the server first. Reinstall SGLang without `-e`.
