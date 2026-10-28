# Status: TensorRT-LLM as a native engine (ADR 0038) — 2026-10-28 (branch `feat/tensorrt-llm`)

ADR 0038 (Accepted 2026-10-27; target 0.2.0) is implemented: `tensorrt-llm` is the sixth
engine kind, running NVIDIA's `trtllm-serve` for chat, `restart_only`, on one GPU of one
host. The option, LLM API key and environment tables were read from three tags, v1.2.1,
v1.3.0rc29 and v1.4.0rc1, and are kept per version line (§2a); a name reserved on any line
is refused on all three, and a version outside them uses its minor line's tables, else
1.2.1's.

- **Registration** (§2, ADR 0018 §1 amended): detection reads `tensorrt_llm-*.dist-info`
  and runs nothing; `engine add` takes `<env>/bin/trtllm-serve` and its version check runs
  the environment's Python on `importlib.metadata`, never importing the engine. The same
  query records the versions of `torch`, `tensorrt`, `flashinfer-python`, `transformers`
  and `mpi4py` as the profile's new `companions` field, which `engine list` shows with
  `idle signal: none` and `engine key: none`. The fingerprint covers `tensorrt_llm/` and
  `triton_kernels/`. The role's own engine is `local_engine.tensorrt-llm`,
  `--tensorrt-llm-bin`, `CAPYCTL_TENSORRT_LLM_BIN` (profile `local-tensorrt-llm`).
- **Verified set** (§2, §2a): an entry names the version and its torch build. It is empty:
  every TensorRT-LLM installation lists as `custom` and runs as an uncertified custom
  setup, never refused for its version (owner decision 2026-10-27, question 7).
- **Launch** (§3, §4): `trtllm-serve serve` on `127.0.0.1` and a leased port, with a
  per-launch served-name link (`served_name_unsupported` for a name that is not one path
  component) and a CapyCTL-written `llm_api_options.yaml`. A deployment adds LLM API
  arguments as `engine_config.tensorrt-llm.llm_api_options`; reserved keys are refused at
  deploy time and again at launch. `kv_cache_config.max_tokens` comes from the grant and
  the checkpoint's geometry (`kv_geometry_unsupported` where CapyCTL cannot compute it);
  after start the built pool must hold one window (`kv_cache_short`) and not exceed the
  rendered figure by more than a block (`effective_args_mismatch`). The closed environment
  sets `TLLM_WORKER_USE_SINGLE_PROCESS=1`, pins Open MPI to an isolated singleton, sets
  `TRTLLM_RESPONSES_API_DISABLE_STORE=1` and refuses the TensorRT-LLM, MPI, NCCL, Hugging
  Face and FlashInfer variable families.
- **Ownership** (§9): at readiness and every re-proof, a descendant outside the launch's
  process group fails the launch `engine_process_escaped`, and a listener off loopback
  fails it `engine_listener_exposed`.
- **Refused** (§1, §11–§13): deep and host-backed parking, `topology` (ADR 0035 §10
  amended), groups, decisions, embeddings, `quantization`, `language_model_only`,
  `model.draft`, and `lmcache: required` (ADR 0033 §2 amended).
- **Cancellation** (§7): no idle signal, so a cut request settles at the next verified
  stop, as for custom engines without one.

The guide (`engines.md`, `install-engines.md`, `engine-flags.md`, `parking.md`,
`errors.md`) and the operations pages (`install.md`, `configuration.md`,
`data-retention.md`) describe it; `crates/capyctl-config/tests/engine_flag_table.rs`
checks the TensorRT-LLM option table against the policy.

**Compatibility.** A TensorRT-LLM profile's `companions` field makes a release before
0.2.0 refuse the `engines.yaml` that holds it, as the `lmcache` field did; remove the
profile with the new binary before starting an older one on the same file. No store
migration is used.

Tests tagged T49 cover the option, key and environment tables, the served-name rule, KV
geometry and `max_tokens`, the configuration file per version line, registration and
companions, the fingerprint over both import packages, launch rendering, readiness, the
KV pool check, and the process-group and listener checks. CPU and Fake-engine tests are
not qualification. The live rows on host A are still to run: TR1–TR11 for 1.2.1,
TR1a–TR11a for the 1.3 pre-release and TR1b–TR11b for 1.4.0rc1; the system packages and
environments they need are added to AGENTS.md when they are made.

# Release note: TensorRT-LLM

- **TensorRT-LLM.** CapyCTL can run NVIDIA TensorRT-LLM's `trtllm-serve` (engine kind
  `tensorrt-llm`) for chat on one GPU, restart-only: register its environment with
  `capyctl engine add <env>/bin/trtllm-serve` and add LLM API arguments with
  `engine_config.tensorrt-llm.llm_api_options`. Versions 1.2.1, the 1.3 pre-release and
  1.4.0rc1 are being qualified; until their live checks pass, every installation runs as
  an uncertified custom setup. See [TensorRT-LLM](../guide/engines.md#tensorrt-llm) and
  [Install an engine](../guide/install-engines.md#tensorrt-llm-121-13-pre-release-or-140rc1).
  An `engines.yaml` with a TensorRT-LLM profile is refused by releases before 0.2.0.
