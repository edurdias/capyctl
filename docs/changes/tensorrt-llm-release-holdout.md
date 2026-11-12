# Status: TensorRT-LLM held out of 0.2.0 — 2026-11-11 (branch `feat/hold-out-tensorrt-llm`)

ADR 0038 remains Accepted, amended by the owner's decision of 2026-11-11.
TensorRT-LLM is unavailable in 0.2.0 and returns in 0.2.x; its implementation
is retained (R01, §9.4, T49).

The required `TLLM_WORKER_USE_SINGLE_PROCESS=1` mode does not populate
`/metrics` iteration statistics in 1.2.1 or 1.4.0rc3. CapyCTL therefore
cannot prove the KV pool. See the [accepted amendment and upstream
worker/proxy evidence](../design/adr/0038-tensorrt-llm-engine.md#accepted-amendment-owner-decision-of-2026-11-11--release-holdout).
Readiness and single-process ownership requirements are not relaxed.

Explicit registration, local-engine configuration, deployment admission and launch
refuse `engine_unavailable_in_release:tensorrt_llm`, with a hint that the
engine returns in 0.2.x. Detection and the interactive picker omit it.
Existing profiles in `engines.yaml` and host `runtime_profiles` are skipped
with a warning naming the profile and `capyctl engine remove <name>`; other
profiles proceed. Explicit local-engine settings refuse before probes or
fingerprints. `engine add` returns exit 5 and the holdout code; explicit
validation and management refusals use `invalid_config` with the holdout code
in the message. Uncertified/custom status cannot bypass the gate. T49 covers
these release boundaries; low-level normalization is retained and TR
qualification remains necessary for the future return.

**Verification (2026-11-11):** All commands used `CARGO_BUILD_JOBS=12`.

- `cargo fmt --all` — passed.
- `cargo clippy --workspace --all-targets --locked -- -D warnings` — passed
  after correcting a cloned-reference slice in the picker regression test.
- `cargo test -p capyctl-adapters -p capyctl-agent -p capyctl-cli -p capyctl-config -p capyctl-management -p capyctl-protocol -p capyctl-store --all-targets --locked --no-fail-fast`
  — adapters, agent, config, protocol and store passed. CLI wording and
  management fixture failures were corrected and rerun below.
- `cargo test -p capyctl-cli -p capyctl-management --all-targets --locked --no-fail-fast`
  — CLI passed; a management regression fixture's refused-host selector
  assertion was corrected.
- `cargo test -p capyctl-management --all-targets --locked --no-fail-fast`
  — passed (139 tests across 18 suites).

Every touched crate's all-target suite passed. `scripts/ci-local.sh` was not
run: the integration owner runs `--deep` afterwards. The runtime Python suite
was not needed because `runtime/` was unchanged. CPU and Fake-engine tests are
not native-engine qualification.

# Release note: TensorRT-LLM deferred to 0.2.x

- **TensorRT-LLM is unavailable in 0.2.0; returns in 0.2.x.** Its required
  single-process mode leaves iteration metrics empty, blocking KV-pool proof.
  Attempts to register, configure, admit or launch it fail with
  `engine_unavailable_in_release:tensorrt_llm`. Use another engine in 0.2.0.
