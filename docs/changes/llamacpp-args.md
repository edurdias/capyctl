# Status: llama.cpp option policy, resolution and rendering (plan slice L2) — 2026-10-09 (branch `feat/llamacpp-l2-args`)

[ADR 0029](../design/adr/0029-llamacpp-engine.md) §5, §6, §8, §9, plan slice L2
(`docs/plans/2026-10-09-llamacpp-engine.md`). The fail-closed option arms of slice L1
are replaced by llama.cpp's own policy, a llama.cpp deployment resolves, and the
`llama-server` command and its closed environment are rendered. Flag names were read
from the v0.6.0 tag (`common/arg.cpp`, commit `d812350`) and the rendered command was
checked against the parser of a CPU build of the tag; nothing ran on a GPU.

- **Option policy** (`engine_policy.rs`, tables in `llamacpp.rs`). llama-server
  takes exact names only, so names are compared exactly after `_` → `-`, against
  every long alias and negative form of each option; the other engines' prefix rule
  is not applied (`--cache-reuse` beside the reserved `--cache-ram` is ordinary).
  `--name=value` is refused, saying llama.cpp refuses it. Reserved options are
  ADR 0029 §6's two lists; a reserved option a typed field renders names that field
  (`--ctx-size` → `context_length`, `--parallel` → `max_concurrent_requests`,
  `--cache-type-k`/`-v` → `kv_cache_dtype`, `--gpu-layers` →
  `llamacpp.n_gpu_layers`, `--model` → `llamacpp.gguf_file`, `--mmproj` →
  `llamacpp.mmproj_file`). The sensitive options need approval by name (any
  spelling approves the option); each path a path option names, as llama-server
  splits it (`--lora` and `--control-vector` lists, `<path>:<scale>` lists), lies
  inside `security.approved_paths` lexically at deploy time and through symlinks in
  the launch builder; a quoted list is refused. The name shapes of ADR 0014 open
  issue 5 still apply, except to `--no-host`. Two spellings of one option are a
  duplicate. Host-fixed arguments follow the same exact-name policy.
- **Hidden inputs.** A llama.cpp profile or deployment `env` naming a variable
  llama-server reads in place of a reserved option, its listener, its device or its
  directories is refused `engine_env_reserved:<name>`, whatever `approved_env` says,
  and the launch environment drops it again: `LLAMA_ARG_*`, `LLAMA_SERVER_*` (router
  mode, slot debugging), `AIP_*` (`AIP_MODE=PREDICTION` with `AIP_HTTP_PORT`
  replaces the port), `LLAMA_API_KEY`, `MTMD_BACKEND_DEVICE` (`--mmproj-device`),
  `HF_TOKEN` (`--hf-token`), the ggml device variables (`GGML_CUDA_DEVICES`,
  `GGML_CUDA_ENABLE_UNIFIED_MEMORY`, `GGML_VK_VISIBLE_DEVICES`,
  `GGML_VK_PREFER_HOST_MEMORY`, `GGML_VK_ALLOW_SYSMEM_FALLBACK`,
  `GGML_METAL_DEVICES`, `GGML_OPENCL_PLATFORM`, `GGML_OPENCL_DEVICE`,
  `GGML_HEXAGON_DEVICES`, `ONEAPI_DEVICE_SELECTOR`, `GGML_BACKEND_PATH`),
  `LLAMA_CACHE`, `XDG_CONFIG_HOME` and `HOME`. The list comes from every `getenv` of
  the v0.6.0 tag; other ggml tuning variables stay available through `approved_env`.
  The host's engine cache creates `<state>/engines/llamacpp/{config,cache}` private
  (0700) and refuses a configuration directory that is not empty.
- **Resolution.** `engine_config.llamacpp` (`n_gpu_layers`, a count or `all`,
  default `all`; `gguf_file`; `mmproj_file`, both relative `.gguf` paths inside the
  checkpoint). `context_length` is required; the slots are
  `max_concurrent_requests`, else 4, and `pad256(context_length) × slots` must fit
  llama-server's `int`; `kv_cache_dtype` is one of llama.cpp's nine cache types,
  default `f16`; `dtype`, `quantization`, `cuda_graphs`, `trust_remote_code: true`
  and `language_model_only: true` are refused naming the path; `deep` and
  `host_backed` are `capability_missing` and an undeclared residency is
  `restart_only`; `model.draft` is refused (`--model-draft` is the way). Until the
  request is derived from the GGUF header (slice L4), `resources` are required,
  offline too, and the KV cache is bounded by the declared Ready allocation less
  the measured weights. Defaults are recorded as `capyctl default`, so a snapshot
  re-resolves to itself. Status and `validate config` show the slots under
  `context.streams` with source `declared` or `default`, and they are the load
  read's running limit.
- **GGUF pick** (`checkpoint_layout.rs`, read where the checkpoint is): the one
  `.gguf` that is not a projector, or the first shard of the one complete split
  set, or the file `gguf_file` names (a later shard or a projector is refused);
  several candidates without `gguf_file` are refused naming each.
- **Rendering** (`capyctl-adapters::llamacpp`): `--host 127.0.0.1 --port <p>
  --model <gguf> --alias <served> --ctx-size <pad256(ctx) × slots> --parallel
  <slots> --no-kv-unified --gpu-layers <n|all> --cache-type-k <t> --cache-type-v
  <t> --fit off --cache-ram 0 --no-context-shift --metrics --slots --offline
  --no-webui --cors-origins localhost --no-cors-credentials [--mmproj <file>]`,
  then the host-fixed and the deployment's arguments. The pass-through arguments are
  checked again at render, and the final vector is rechecked: each rendered option
  exactly once, `--mmproj` at most once, no other reserved option, no `=` spelling.
  The environment is closed: `PATH`, the GPU pin, `XDG_CONFIG_HOME`, `LLAMA_CACHE`,
  the engine log and the resolved engine environment less the refused names; no
  `HOME`.
- **Still refused until the later slices land**: the launch (the host agent's launch
  authorization and the embedded coordinator's Initialize refuse the kind, and the
  launch builder, adapter, readiness, idle gate and the launch-time
  `/etc/llama.cpp/config.ini` check are slice L3); deriving the request from the
  GGUF header and the training-context refusal (slice L4, so `resources` stay
  required); per-request figures and load reports (slice L5); park and group paths
  refuse the kind as before.

Tests (T42 with T14, T37): `capyctl-config/tests/llamacpp.rs` (every reserved
spelling and `_` spelling in extra and host-fixed arguments, the fields named, the
`=` spelling, ordinary options, duplicates across spellings, approvals and paths,
options refused even when approved, the tables against llama-server 0.6.0's option
list in `tests/fixtures/llama-server-0.6.0-options.txt`, resolution, slots and their
source, the environment refusals, `gguf_file`/`mmproj_file`, the GGUF pick),
`capyctl-adapters/tests/llamacpp_args.rs` (`--ctx-size 60416 --parallel 4` for 15000
tokens and 4 slots, the reserved block complete and in order, refusals at render,
the recheck, the closed environment, the builder's GGUF and projector, a draft model
outside the approved paths through a symlink) and the `engine_cache` unit test (the
private directories). The engine flag table of `docs/guide/engine-flags.md` and its
test rows land with the guide in slice L6. CPU and Fake-engine tests only; they are
not qualification. Only the live rows LC1–LC6 (slice L6) qualify llama.cpp.

# Release note: none
