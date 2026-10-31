# Status: An editable SGLang is refused where it is added, and a failed SGLang import is named — 2026-10-30 (branch `fix/sglang-editable-install`)

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
(Amendment 2026-10-30, in the ADR). It narrows the 2026-09-23 amendment's
rule that an installation that cannot be measured is `unmeasured`, never
refused, for this one case, and only where no protocol is involved.

Owner decision: this ships in the 0.1.11 patch, and ADR 0017 §2 says a patch
never changes the protocol. So an enrolled host never sends the new code; the
host-side refusal, gated on a new capability, follows in 0.2.0 (kept on
branch `feat/sglang-editable-host-refusal`).

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
- `capyctl start standalone` refuses such an installation before every
  Initialize, under either `installation_drift` policy, including one named
  with `--sglang-bin` or `local_engine`, in its embedded installation check.
  The refusal stays in that process: status shows it as the operation's
  reason text (`launch_failed`) with a fixed operator hint; nothing crosses
  the host protocol and no closed code is stored for another binary to
  parse.
- An enrolled host is unchanged: its launch-time installation check calls an
  editable SGLang `unmeasured` and refuses nothing, the launch starts, and
  the entry fails it at once with `engine_import_failed` in the free-text
  launch failure. The protocol (capabilities, policy refusals) is unchanged.
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
- `capyctl-controller`
  `installation_gate::tests::an_editable_sglang_is_refused_before_any_effect`
  (T21 T22): standalone refuses with the code and its hint under either
  policy, journals no drift, and keeps the installation `unmeasured`.
- `capyctl-agent`
  `native_execution::tests::a_host_never_refuses_an_editable_sglang_with_a_code_servers_do_not_know`
  (T21 T34): an enrolled host's admission of an editable SGLang launch,
  under `warn` and `refuse`. With the refusal in the shared launch-time
  check it got `Refused("installation_unsupported:editable")`; now it takes
  the pre-#198 path to the fixture's next gate (`checkpoint_mismatch`, a
  refusal every server knows).
- `capyctl-agent` `installation::tests::an_editable_sglang_is_named_but_the_shared_launch_check_never_refuses_it`
  (T21 T22) and `another_distributions_hook_and_a_linked_package_are_not_refused`
  (T21; with the earlier matching, sgl-router's
  `__editable___sglang_router_0_3_1_finder.py` named a missing SGLang
  editable); `capyctl-adapters`
  `launch_failure::tests::an_sglang_import_failure_is_named_by_its_code`;
  `capyctl-domain` `diagnostics::tests::classification_prefers_the_named_category`;
  `capyctl-cli` `site_errors_page` (the new exit 17 row).

CPU and Fake-engine tests are not qualification.

Live check still needed: on a GB10 machine, run CapyCTL against the official
`lmsysorg/sglang:v0.5.22` image as shipped: `capyctl engine add
/opt/sglang/bin/python3` must exit 17 with `installation_unsupported:editable`
and write nothing, and `capyctl start standalone --sglang-bin
/opt/sglang/bin/python3` must refuse a start with the same code. On a host
enrolled with a server, a start of that installation must fail at once with
`engine_import_failed` and leave the host's control session up. Then
reinstall SGLang without `-e` inside the image (`/opt/sglang/bin/python3 -m
pip install --no-deps --no-build-isolation --force-reinstall
/sgl-workspace/sglang/python`), add it again, and launch a model: it must
reach `ready` and serve.

# Release note: Fixes

- **An editable SGLang is refused when you add it, with a clear code.**
  SGLang installed with `pip install -e`, as in the official
  `lmsysorg/sglang` image, cannot be loaded by CapyCTL, which never runs the
  `.pth` file such an install depends on. `capyctl engine add` now refuses it
  (`installation_unsupported:editable`, exit 17), and standalone refuses to
  start one with the same code, instead of each launch failing a second in
  with `startup_error`. On a host enrolled with a server the launch still
  starts and now fails with `engine_import_failed` and a hint of what to
  check; the refusal on such hosts follows in 0.2.0. Reinstall SGLang without
  `-e`.
