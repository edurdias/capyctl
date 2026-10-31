# Status: An editable SGLang is refused with its own code, and a failed SGLang import is named — 2026-10-30 (branch `fix/sglang-editable-install`)

Issue #198. The official `lmsysorg/sglang:v0.5.22` image installs SGLang in
editable mode: `site-packages` holds `__editable__.sglang-0.5.22.pth` and its
finder, and the package lives in `/sgl-workspace/sglang/python/sglang`. The
protected SGLang entry runs under `python -IS` and puts only the environment's
`site-packages` on its path, so the `.pth` finder never ran, `import sglang`
failed, and every launch ended about a second in with only
`sglang_startup_failed: startup_error`. `capyctl engine add` and
`capyctl validate config` accepted the installation.

Design decision for the owner to review: the editable layout is refused, not
supported. The registered fingerprint and drift flag cover only
`site-packages/sglang` (an editable install has none, so it registers
`unmeasured`); adding the checkout's `python` directory to the search path
would load code those checks never attest and expose every other top-level
name in that directory. This is proposed as an amendment to ADR 0008
(Amendment 2026-10-30, in the ADR), and it narrows the 2026-09-23 amendment's
rule that an installation that cannot be measured is `unmeasured`, never
refused, for this one case: an installation the entry can never import is
refused before any effect instead of failing at launch.

What changed:

- `capyctl engine add` refuses an SGLang whose package is not in its
  environment's `site-packages` and which `site-packages` records as editable
  (`__editable__.sglang-<version>.pth`, setuptools' finder module
  `__editable___sglang_<version>_finder.py`, `sglang.egg-link`, or
  `"dir_info": {"editable": true}` in `direct_url.json`). The code is
  `installation_unsupported:editable`, exit 17; nothing runs and nothing is
  written. A package directory in `site-packages`, or a link to one (the
  entry imports it through the link), is never refused, whatever lies
  beside it; sgl-router's own editable hooks
  (`__editable___sglang_router_<version>_finder.py`) are not SGLang's.
  vLLM, started without `-S`, is unaffected.
- The launch-time installation check (`InstallationRegistry::verify`, on a
  host agent and in standalone's embedded gate) refuses such an installation
  before any effect with the same code under either `installation_drift`
  policy, so a profile registered by an earlier release, or named with
  `--sglang-bin` or `local_engine`, is refused too. The code is added to the
  protocol's policy refusals and has a fixed operator hint in status.
- Version skew (ADR 0017): a 0.1.10 controller's refusal check does not know
  the code and would end the host's whole control session on it, at every
  redelivery. The code therefore rides on a new capability,
  `installation_unsupported`, which this server lists in
  `SessionReady.capabilities` (and the host declares in `Connect`). A host
  reports the refusal only to a server that listed it
  (`capabilities::server_takes_refusal`); under an older server the launch
  goes ahead unmeasured, exactly as before this change, and fails at the
  engine's start with `engine_import_failed` in the free-text launch
  failure, which every server takes. `engine add` and standalone's embedded
  check involve no older server and refuse directly.
- The entry reports a failed SGLang import as its own closed code,
  `engine_import_failed`, with a fixed hint and no exception text or path,
  instead of `startup_error`. The launch failure summary names the code, and
  `capyctl status deployment` shows its hint.
- Docs: `docs/guide/install-engines.md` (install SGLang without `-e`, the
  reinstall command for the image, two troubleshooting entries),
  `docs/guide/engines.md` (Add one), `docs/guide/errors.md` (the exit 17 row
  and both launch codes), `docs/operations/install.md` (engine add), ADR
  0008 (proposed amendment).

Tests, failing before and passing after:

- `runtime/tests/test_sglang_startup_guards.py`
  `EditableInstallationTests.test_an_editable_sglang_fails_with_its_own_code_and_its_pth_never_runs`
  (T21): a real stdlib venv with a fake editable SGLang (the `.pth` and finder
  in `site-packages`, the package in a checkout elsewhere). The positive
  control imports it with site processing on; under `-IS` the entry composes
  only the interpreter's own `site-packages`, never runs the hook, and its
  guarded import raised `ModuleNotFoundError: No module named 'sglang'`
  before and fails `engine_import_failed` with a path-free hint after.
- `runtime/tests/test_sglang_entry.py`
  `test_failed_engine_import_surfaces_engine_import_failed` (replaces
  `test_failed_engine_import_denies_through_startup_error`): a missing and a
  raising SGLang printed `sglang_startup_failed: startup_error` before and
  `engine_import_failed` with its hint and no exception detail after.
- `crates/capyctl-cli/tests/first_run.rs`
  `engine_add_refuses_an_editable_sglang_before_anything_runs` (T07 T21),
  through the binary: before, `engine add` exited 0 and saved the profile
  (`fingerprint: null`); after, it exits 17 with
  `installation_unsupported:editable`, never runs the interpreter and writes
  nothing, and the same environment reinstalled as a regular package is
  added.
- `capyctl-agent` `installation::tests::an_editable_sglang_is_refused_at_every_launch_whatever_the_policy`
  (T21 T22; with detection disabled, as before the change, it fails on the
  first marker); `capyctl-controller`
  `installation_gate::tests::an_editable_sglang_is_refused_before_any_effect`;
  `capyctl-adapters` `launch_failure::tests::an_sglang_import_failure_is_named_by_its_code`;
  `capyctl-domain` `diagnostics::tests::classification_prefers_the_named_category`;
  `capyctl-cli` `site_errors_page` (the new exit 17 row).
- `capyctl-agent`
  `native_execution::tests::an_editable_sglang_is_refused_only_toward_a_server_that_takes_the_code`
  (T21 T34): admission of an editable SGLang launch toward no SessionReady,
  0.1.10's capability list, and this server's. With the capability gate
  bypassed (the state before it) the older server got
  `Refused("installation_unsupported:editable")`; with the gate it gets the
  pre-#198 path (the fixture's next gate, `checkpoint_mismatch`), and only
  the current server gets the new code.
- `capyctl-protocol` `version_skew::installation_unsupported_reaches_only_a_server_that_takes_it`
  (T34): the capability is catalogued, declared by the agent and advertised
  by the server; toward 0.1.10's list or none the new code is withheld and
  every older refusal still passes; toward the current server every refusal
  passes. `live_profile_update_is_declared_both_ways` now expects the third
  server capability.
- `capyctl-agent` `installation::tests::another_distributions_hook_and_a_linked_package_are_not_refused`
  (T21): with the earlier matching it failed (sgl-router's
  `__editable___sglang_router_0_3_1_finder.py` refused a missing SGLang
  `installation_unsupported:editable`); now neither sgl-router's hooks nor a
  `site-packages/sglang` link to a package directory is refused, while a
  link to nothing beside SGLang's own hook still is.

CPU and Fake-engine tests are not qualification.

Live check still needed: on a GB10 machine, run CapyCTL against the official
`lmsysorg/sglang:v0.5.22` image as shipped: `capyctl engine add
/opt/sglang/bin/python3` must exit 17 with `installation_unsupported:editable`
and write nothing. Then reinstall SGLang without `-e` inside the image
(`/opt/sglang/bin/python3 -m pip install --no-deps --no-build-isolation
--force-reinstall /sgl-workspace/sglang/python`), add it again, and launch a
model: it must reach `ready` and serve. A launch whose SGLang import fails for
another reason must end with `sglang_startup_failed: engine_import_failed`
and show that code's hint in `capyctl status deployment`. Mixed versions:
with a 0.1.10 server and a host of this release, an editable launch must
start, fail with `engine_import_failed`, and leave the host's control
session up.

# Release note: Fixes

- **An editable SGLang is refused when you add it, with a clear code.**
  SGLang installed with `pip install -e`, as in the official
  `lmsysorg/sglang` image, cannot be loaded by CapyCTL, which never runs the
  `.pth` file such an install depends on. `capyctl engine add` now refuses it
  (`installation_unsupported:editable`, exit 17) and every start of one is
  refused with the same code, instead of each launch failing a second in
  with `startup_error`. Reinstall SGLang without `-e`. A launch whose SGLang
  cannot be imported for another reason now ends with `engine_import_failed`
  and a hint of what to check.
