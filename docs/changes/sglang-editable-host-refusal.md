# Status: An enrolled host refuses an editable SGLang at launch, toward a server that takes the code — 2026-10-31 (branch `feat/sglang-editable-host-refusal`)

Follows #198 (change file `sglang-editable-install.md`, shipped in 0.1.11),
which refuses an editable SGLang at `capyctl engine add` and in standalone
only, because a patch release never changes the protocol (ADR 0017 §2). An
enrolled host in 0.1.11 starts such an installation, and the entry fails it
with `engine_import_failed`. This branch, for 0.2.0, adds the host-side
refusal, as the ADR 0008 amendment of 2026-10-30 now states (one amendment
covering both releases; still proposed, for owner review).

What changed on top of 0.1.11:

- A new ADR 0017 capability, `installation_unsupported`
  (`capyctl_protocol::capabilities::INSTALLATION_UNSUPPORTED`). The server
  lists it in `SessionReady.capabilities` (`server_capabilities()`), and the
  host declares it in `Connect.capabilities` (catalogued host-to-server). The
  code `installation_unsupported:editable` joins `POLICY_REFUSALS`.
- The host's launch-time installation check (`InstallationRegistry::verify`,
  used by LaunchSingle, group Launch and ingress provisioning through
  `admit_launch_full`) refuses an editable SGLang with that code under either
  drift policy. Standalone's embedded check uses the same path.
- The host reports the refusal only to a server whose SessionReady listed the
  capability (`capabilities::server_takes_refusal`). `SessionExecution` gains
  `server_ready(capabilities)`, called on each SessionReady before that
  session's commands run; until one arrives nothing is listed. Toward a
  server that did not list it (0.1.x), whose refusal check would end the
  host's whole control session on an unknown code, the launch takes the
  0.1.11 path: unmeasured, started, failed at once with
  `engine_import_failed` in the free-text launch failure.
- Docs: `docs/guide/engines.md`, `docs/guide/errors.md` and
  `docs/operations/install.md` describe the refusal on a host under a 0.2.0
  server and the 0.1.x fallback; ADR 0008's amendment says what each release
  refuses where.

Tests:

- `capyctl-protocol` `version_skew::installation_unsupported_reaches_only_a_server_that_takes_it`
  (T34): the capability is catalogued, declared by the agent and advertised
  by the server; toward 0.1.10's SessionReady list and toward none the new
  code is withheld while every older refusal still passes; toward the
  current list every refusal passes. `live_profile_update_is_declared_both_ways`
  expects the third server capability.
- `capyctl-agent`
  `native_execution::tests::an_editable_sglang_is_refused_only_toward_a_server_that_takes_the_code`
  (T21 T34): admission of an editable SGLang launch with no SessionReady,
  with 0.1.10's capability list, and with the current list. With the gate
  bypassed, the first gave `Refused("installation_unsupported:editable")`
  (failing before); with it, the first two take the 0.1.11 path to the
  fixture's next gate (`checkpoint_mismatch`) and only the current list gets
  the new code.
- `capyctl-agent` `installation::tests::an_editable_sglang_is_refused_at_every_launch_whatever_the_policy`
  (T21 T22): the shared launch-time check refuses it under `warn` and
  `refuse`, registered or not.

CPU and Fake-engine tests are not qualification.

Live check still needed: a 0.2.0 server and a 0.2.0 host with an editable
SGLang (the official `lmsysorg/sglang:v0.5.22` image as shipped, registered
by hand or with `--sglang-bin`): a start is refused at launch with
`installation_unsupported:editable` and its hint, before any engine process
starts. A 0.1.x server and a 0.2.0 host with the same installation: the start
fails with `engine_import_failed`, and the host's control session stays up
(no reconnect, other deployments on the host unaffected).

# Release note: Fixes

- **A host refuses an editable SGLang before starting it.** On a GPU machine
  enrolled with a server, a launch of SGLang installed with `pip install -e`
  (as in the official `lmsysorg/sglang` image) is now refused before
  anything starts, with `installation_unsupported:editable` and what to do,
  as standalone and `capyctl engine add` already did. This needs the server
  and the host both on this release; under an older server the engine still
  starts and fails with `engine_import_failed`. Reinstall SGLang without
  `-e`.
