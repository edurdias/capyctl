# Status: vLLM 0.33.0 launch-surface audit — 2026-11-10 (branch `engines/vllm-0.33.0`)

Owner decision 2026-11-10: check the latest engine releases and update them.
SPEC §§8.2, 9.1, 13.3 and 17, ADR 0014 §3, ADR 0017 §2 and ADR 0034
still govern the launch boundary. The protected entry's `cli_env_setup` move
was already fixed; this change reserves 0.33.0's `sleep_mode_backend` and
`watchdog_config` destinations wherever the installed parser defines them.
The former selects a registered code-loading backend; the latter can write
thread stacks to a configured directory. Older parsers need neither option.
Deploy-time policy also recognizes 0.33.0's inverse `--enable-*` and
`--disable-*` aliases for reserved booleans, without changing other engines.
The installed parser's destination checks already protect those aliases.

The engine guides name 0.33.0 as the newest known release and update the
installation command and its PyTorch dependency. `registration.rs::VERIFIED`
is unchanged: 0.33.0 still shows `CUSTOM yes`. CPU and Fake-engine tests are
not qualification. Live chat, decisions, embeddings, deep/host-backed park
and wake, and the ADR 0034 LMCache paths are still owed on the rented RTX PRO
6000 x86 machine and host A; no qualification is marked passed here.

## Audit evidence

Downloaded the CPython 3.12-compatible x86_64 manylinux wheels with
`python3 -m pip download --only-binary=:all: --no-deps --python-version 312
--platform manylinux_2_28_x86_64 --implementation cp --abi abi3`, once per
version. PyPI publishes these as `cp38-abi3` wheels, usable on CPython 3.12.
Both wheels were extracted, never installed or imported. The initial command
used an unavailable `pip` executable; a first `python3 -m pip` attempt with
the wrong platform tag selected a source distribution and was cancelled.
Only the binary wheels below supplied this audit.

Every upstream path below is relative to the wheel's `vllm/` directory unless
a distribution metadata path is given. References explicitly identify both
versions; identical means a byte comparison of those files matched.

| CapyCTL surface | vLLM 0.32.0 evidence | vLLM 0.33.0 evidence and result |
|---|---|---|
| Protected entry's CLI mirror | `entrypoints/cli/main.py:52-56,71,101-120`; `entrypoints/cli/serve.py:54-177` | `entrypoints/cli/main.py:52-53,68,98-117`; `entrypoints/cli/serve.py:54-177`. Only startup import and frontend argument-module locations change for the serve path; setup, parser, validation and dispatch remain compatible. The existing startup-import fix is retained. |
| `RESERVED`, optional destinations and typed arguments | `engine/arg_utils.py:475-849,1022,1060,1363,3206`; `entrypoints/launchers/cli_args.py:283-411`; `config/logging.py` | `engine/arg_utils.py:493-873,1047-1050,1088,1395,3260`; `entrypoints/common/cli_args.py:293-421`; `config/logging.py`. Every existing owned and typed name remains declared. `RESERVED_IF_PRESENT` adds `sleep_mode_backend` and `watchdog_config`; `RESERVED_IF_DEFINED`, SSL/data-parallel families, logging and typed names need no change. |
| New backend and disk-writing options | `config/model.py:375-380` has an internal backend field, but `engine/arg_utils.py:1021-1027` exposes no backend switch; no watchdog config file | `engine/arg_utils.py:1047-1051,1909`; `config/model.py:379-384`; `device_allocator/sleep_mode_backend.py:160-214`; `config/watchdog_config.py:13-30`. New CLI backend selection imports its registered module; watchdog `dump_dir` enables stack files. Both destinations and their dotted/abbreviated forms are now reserved in Python and Rust. |
| Boolean aliases, Rust reserved policy and bidirectional parity | `utils/argparse_utils.py:142-143` uses the normal BooleanOptionalAction registrations | `utils/argparse_utils.py:120-151,179` adds EnableDisableAction. Rust reserved matching now handles inverse enable/disable spellings, including `--disable-per-request-metrics` and `--enable-log-stats`; runtime still compares their original destinations. Both directions of `reserved_parity.rs` remain intact. |
| Marker and config-file handling | `utils/argparse_utils.py:276-414,495-568`; `entrypoints/cli/serve.py:54-57,169-174` | `utils/argparse_utils.py:313-451,532-605`; same serve lines. Apart from boolean registrations, argument preprocessing is unchanged: CapyCTL strips its own two markers before parsing and refuses config files before parser file I/O. No new upstream marker bypass. |
| Plugins | `plugins/__init__.py:18-40,61-87`; `envs.py:1172-1175` | Plugin implementation is identical; `envs.py:1170-1173` retains the empty-list pin. The pre-import foreign `vllm.*` entry-point refusal still covers general, platform, endpoint, logger and I/O plugins. No new plugin group. |
| Guard middleware hook and authentication prefixes | `entrypoints/serve/middleware/register.py:65-74`; `authenticate.py:11,59` | Both files identical. Dotted class middleware is still imported and added to the app; guarded prefixes remain `/v1`, `/v2`, `/inference`, `/cohere`. CapyCTL's separate admin key continues to protect development controls. |
| Deep-park routes and parser destinations | `entrypoints/serve/dev/sleep/api_router.py:26-64`; `dev/rpc/api_router.py:23-55`; `dev/cache/api_router.py:20-44`; `engine/arg_utils.py:1022,1060`; frontend `cli_args.py:351` | All three route files identical: `/sleep`, `/wake_up`, `/is_sleeping`, `/collective_rpc`, `/reset_prefix_cache`, including query/body fields. Destinations remain `enable_sleep_mode`, `middleware`, `safetensors_load_strategy` at `engine/arg_utils.py:1047,1088`, common `cli_args.py:361`. Capability probes and residency requests need no change. |
| Rendezvous and transport environment | `utils/network_utils.py:53-59,149-160`; `envs.py:722-727,1726-1727` | Network helper is identical; `envs.py:724-729,1712-1713` retains host IP, port and serialization semantics. Loopback/file-store pins, group-only address/interface handling and inherited transport stripping remain valid. |
| Rendering, readiness, health, models and metrics | `engine/arg_utils.py:1022,1060,1363`; frontend `cli_args.py:150,166,283-351`; `entrypoints/serve/instrumentator/health.py:22`; `entrypoints/openai/models/api_router.py:20`; instrumentator `metrics.py:66-80`; `v1/metrics/loggers.py:528,538,620` | Same render destinations, frontend `cli_args.py:158,174,293-361`, engine lines above. Health, models router, metrics instrumentation and metrics logger are identical. `/health`, `/v1/models`, `/metrics`, `vllm:num_requests_running`, `vllm:num_requests_waiting`, `vllm:kv_cache_usage_perc` retain their names; existing fixed completion/embedding probes remain applicable. The models serving change only clears a LoRA load-in-place flag (`entrypoints/openai/models/serving.py:226-228`). |
| Decisions `/v1/systemone` | `entrypoints/generate/structured_decisions/api_router.py:33-57`; `protocol.py:23-86` | Router remains `/v1/systemone` (`api_router.py:33-54`). Protocol now forbids extra fields (`protocol.py:22`), adds optional `joint` default false (`42-45`) and optional joint diagnostics (`100`), and preserves `model`, `state`, `questions`, images/files, answers, usage and diagnostics (`24-40,90-100`). CapyCTL's closed request subset and fixed decision probe use only retained fields; the additional response field is ignored. No adapter change required. |
| LMCache names and ADR 0034 | `distributed/kv_transfer/kv_connector/factory.py:103-108,165-175`; `config/kv_transfer.py:64,112` | Both files identical: external module-path lookup, `LMCacheMPConnector`, bundled `LMCacheConnectorV1`, and the per-parse random `engine_id` remain unchanged. CapyCTL keeps the explicit external module path and engine-driven transfer. New `config/vllm.py:1254-1267` checks sleep support; `kv_connector/v1/base.py:210-212` defaults it to true. LMCache 0.5.6 (`lmcache/integration/vllm/lmcache_mp_connector.py:19-23,535`) and 0.5.7 (`19-23,556`) inherit that base without overriding it, as checked in the companion LMCache audit. This is interface compatibility, not proof that parking works. |
| Environment CapyCTL sets or strips | `envs.py:722-727,833-838,1172-1175,1487,1726-1727`; `usage/usage_lib.py` | `envs.py:724-729,842-847,1170-1173,1468,1712-1713`; usage implementation identical. API key, dev mode, plugin pin, telemetry opt-outs and transport switches retain their meaning. Adapter environment clearing and its explicit allowlist still strip inherited engine-specific inputs. New variables are listed below; none independently introduces a listener, module path, disk destination or network target. |
| Installation fingerprint and version detection | `vllm-0.32.0.dist-info/METADATA:2-3,19,82`; CLI `main.py:95-100` | `vllm-0.33.0.dist-info/METADATA:2-3,19,85`; CLI `main.py:92-97`. Distribution/import package is still `vllm`, metadata version is `0.33.0`, console `--version` still uses importlib metadata; torch moves from 2.13.0 to 2.14.0. Existing whole-package fingerprinting and metadata/version comparison need no change and cover moved source files automatically. |

The complete new central environment-variable set is
`VLLM_BEAM_ALWAYS_CONSTRAIN_OUTPUT`, `VLLM_CHAT_TEMPLATE_FUEL`,
`VLLM_COHERE2_ATTN_MOE_DUAL_STREAM_TOKEN_THRESHOLD`,
`VLLM_HTTP_MAX_REQUEST_BODY_SIZE`, `VLLM_MAX_MEDIA_REQUEST_DECODE_BYTES`,
`VLLM_MAX_MEDIA_REQUEST_DOWNLOAD_SIZE_MB`, `VLLM_REQUEST_BODY_LOG_LEVEL`,
`VLLM_ROCM_MUSE_MOE_ONLINE_QUANT`, `VLLM_ROCM_MUSE_MOE_REDUCE_DTYPE` and
`VLLM_STEP5_SHARD_SP_SHARED_EXPERT` (`envs.py:818-826,1047-1057,1136-1143,
1273-1282,1642-1647,1848-1854,2131-2138,2282-2285`; absent from 0.32.0).
These control resource limits, kernels, decoding and logging, not new targets.
The new request-body log level cannot override CapyCTL's rendered-off request
logging. A repository-wide static scan of environment reads also found new
kernel controls `ARCH`, `COMPILE_ONLY`, `PYTORCH_ROCM_ARCH`,
`VLLM_MINIMAX_M3_MSA_CUTLASS_MIN_BATCH`, a benchmark-only
`VLLM_DEEP_GEMM_WARMUP`, and `NCCL_RAS_ENABLE` (the existing owned `NCCL_`
family); none requires a new environment reservation.

The other new parser options are `--reasoning-effort-map`
(`entrypoints/common/cli_args.py:111-118,249,513-528`) and `--monokernel`
(`engine/arg_utils.py:1842`, `config/kernel.py:299-304`): request-value
mapping and kernel selection, not listener, code-path, disk or egress
configuration. Static comparison of all `config/*.py` declared fields found
only additional kernel/cache/speculation controls besides the watchdog; no
other newly exposed sensitive destination was found.

## CPU checks

- `python3 -m unittest runtime.tests.test_vllm_entry runtime.tests.test_capyctl_vllm_guard runtime.tests.test_engine_capabilities runtime.tests.test_loopback_rendezvous`: 83 passed.
- `cargo test -p capyctl-config --test engine_config --test reserved_parity --locked`, serialized with the build lock and an isolated worktree target directory: 26 policy tests and both parity tests passed.
- `cargo fmt --all -- --check`: passed after correcting the first check's formatting-only differences.
- The new runtime and deploy-policy cases carry T14/T21 references. No pre-change red test run was performed; checks were restricted to the requested final targeted runs.

# Release note: Fixes

- **vLLM 0.33.0 keeps launch controls private.** Its sleep-backend selector and watchdog stack-dump configuration are reserved, and its new inverse boolean aliases cannot override CapyCTL-owned settings. Installation instructions now name 0.33.0; it remains `CUSTOM yes` pending live qualification.
