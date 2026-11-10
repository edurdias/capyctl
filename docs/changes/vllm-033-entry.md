# Status: CapyCTL launches vLLM 0.33.0 — 2026-11-10 (branch `fix/vllm-033-entry`)

Issue #239. CapyCTL 0.1.11 registered vLLM 0.33.0 but every launch failed at
import: the protected entry `runtime/vllm_entry.py` (`InstalledVllm`) imported
`cli_env_setup` from `vllm.entrypoints.serve.utils.api_utils`, and vLLM 0.33.0
moved it to `vllm.entrypoints.common.startup`. Found on host A on 2026-11-10
while qualifying 0.33.0. vLLM 0.30.0 to 0.32.0 keep only the old location and
0.33.0 only the new one; the rest of vLLM's own `main()` that the entry mirrors
is unchanged between 0.32.0 and 0.33.0, and the development routes deep parking
probes are still there.

ADR 0017 §2 (version skew): the entry now imports `cli_env_setup` from the new
location and falls back to the old one.

Tests: `runtime.tests.test_vllm_entry` `test_cli_env_setup_is_found_in_either_layout`
loads `InstalledVllm` against stand-in modules in each layout and checks that
the build's own setup runs; it fails before the change (the 0.33.0 layout) and
passes after.

CPU and Fake-engine tests are not qualification. Live check, host A,
2026-11-10, a build of this branch, CapyCTL standalone with Qwen3-4B: with
vLLM 0.33.0 the model reached ready and answered a collected and a streamed
chat request (HTTP 200, the stream ending in `[DONE]`), and the deep-park
capability probe reported its routes present (deep parking itself was not
exercised); with vLLM 0.32.0 it reached ready and answered a collected
request. Functional only, beside other work on the host: no timing or memory
measured.

# Release note: Fixes

- **CapyCTL launches vLLM 0.33.0.** 0.1.11 accepted a vLLM 0.33.0
  installation at `capyctl engine add`, but every model launch on it failed
  at startup, because vLLM 0.33.0 moved a function CapyCTL's launcher calls.
  The launcher now finds it in either place; earlier vLLM versions launch as
  before. vLLM 0.33.0 is still shown as a custom build (`CUSTOM yes`) until
  it is qualified.
