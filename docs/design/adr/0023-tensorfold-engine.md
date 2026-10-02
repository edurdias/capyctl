# ADR 0023 — TensorFold as the third engine

**Status:** Accepted (owner decision 2026-10-01).
**Amends:** `SPEC.md` §1 (supported engines), §9 (a new §9.3; "Later backends" becomes
§9.4), §20 (T41), and ADR 0018 §1 (detection and the verified set).
**Related:** ADR 0008 (installations and capability probes), ADR 0012 (deep parking),
ADR 0014 §3 and §7 (reserved settings, checkpoint identity), ADR 0018 (engine
registration). Design: `docs/specs/2026-10-01-tensorfold-engine-design.md`.

## Context

TensorFold (Apache-2.0) is an inference server with an OpenAI-compatible API and
drafter-based parallel decoding on NVIDIA GPUs. Home users run it by hand, one model
per process. It has no sleep, release or unload API, no API key, and sizes itself from
free memory unless `--context` fixes its KV allocation. Its first start builds CUDA
extensions with `torch.utils.cpp_extension`.

## Decision

### 1. Engine kind

`tensorfold` joins `vllm` and `sglang` in every closed engine set. The default
profile name is `tensorfold`; the entry point is `<env>/bin/tensorfold`; the
version check is `<env>/bin/tensorfold --version`. Detection reads
`tensorfold-*.dist-info` with ADR 0018 §1's bounds and locations and executes
nothing. The verified set gains TensorFold 0.6.0 and 0.6.1; any other version
is `custom`.

### 2. Registration

`capyctl engine add` refuses a TensorFold installation with `toolchain_missing`
(exit 26) unless `ninja`, `nvcc` and `c++` or `g++` are on the engine's closed
launch PATH: the installation's `bin`, the profile's `<cuda_home>/bin`, then
`/usr/local/bin:/usr/bin:/bin`. TensorFold 0.6.1 also builds with a pip-only
compiler (`pip install ninja "cuda-toolkit[nvcc,cccl]==13.0.*"`), which puts
`nvcc` in `<env>/lib/python3.*/site-packages/nvidia/cu<major>/bin`; `nvcc` is
also looked up there, after `<env>/bin` and before `<cuda_home>/bin`. The
directory pattern is resolved by listing real directories, never following a
link out of the environment; `ninja` and `c++` or `g++` keep the closed search.
The caller's PATH is never used and nothing is run.
The profile is written `security.deep_park: disabled`; `--deep-park enabled` is
refused `capability_missing`. Like vLLM and SGLang, a role's own TensorFold is
`local_engine.tensorfold` (`--tensorfold-bin`, `CAPYCTL_TENSORFOLD_BIN`), with the
same toolchain check and profile names (`local`, or `local-tensorfold` beside
another engine).

### 3. Launch

`<env>/bin/tensorfold serve <model dir> --name <served> --host 127.0.0.1
--port <port> --no-update-check --backend cuda --snapshot-dir none --context <n>
[--drafter none] [typed flags] [host args] [extra args]`, with `--drafter none`
only when the host or extra arguments name neither `--drafter` nor
`--no-drafts`: TensorFold's default `--drafter auto` would pick a drafter from
the Hugging Face cache. `--no-drafts` is ordinary and renders alone, since
TensorFold 0.6.1 refuses `--drafter none` for a family whose CUDA engine needs
a drafter (Qwen3.8 dense); naming both is refused at deploy time and again when
rendering, and a launch TensorFold refuses for a missing drafter reports that
fix (found live 2026-10-02). The environment is
closed (SPEC §13.3) and adds `TENSORFOLD_NO_UPDATE_CHECK=1`, `HF_HUB_OFFLINE=1`,
`TRANSFORMERS_OFFLINE=1`, the placement's `CUDA_VISIBLE_DEVICES` and
`TORCH_EXTENSIONS_DIR=<state>/engines/tensorfold/<build_fingerprint>/torch_extensions`
(service user, 0700). torch holds `<extension>/lock` in that directory while it
builds, and a build killed mid way leaves it, so the next start waits on it
forever. Before a launch, CapyCTL removes those lock files, and nothing else,
only when every process recorded for a launch the role still retains (host
journal claims, or the standalone's unreleased bindings) is proved gone: a
concurrent start of the same version shares the directory, and the lock of a
launch that may be building stands (found live 2026-10-02). Reserved: `--host`, `--port`, `--name`, `--alias`,
`--backend`, `--context`, `--tp`, `--rank`, `--master`, `--master-port`, `--snapshot-dir`,
`--no-update-check`, checked with the
abbreviation rule at deploy time and again when the command is rendered. There is
no protected entry: TensorFold's parser accepts abbreviations, and the prefix rule
is what re-verifies them. `--vision-urls`, `--lane-kernels` and `--drafter` need
host approval by name; `--vision` is ordinary.

### 4. Deployment configuration

`context_length` is required and renders `--context`; `kv_cache_dtype` renders
`--kv-dtype`; `engine_config.tensorfold` takes `max_tokens` and `thinking`. The common fields TensorFold has no flag for are refused. A
TensorFold deployment states `resources`. Its residency is `restart_only`; `deep`
or `host_backed` fails resolution with `capability_missing`. An undeclared
`timeouts.initialize` is 1800 s and an undeclared `request_deadline` is at least
1800 s; the host gives up at the ordinary derived bound once
`TORCH_EXTENSIONS_DIR` holds a build.

### 5. Drafter

A drafter is handled as vLLM's and SGLang's draft models are (ADR 0014 §8 and
Amendment A3): `--drafter <dir>` is an extra argument the host approves by name,
whose directory must lie inside `security.approved_paths`, checked lexically at
deploy time and through symlinks before launch. A repository id is refused.
CapyCTL neither fetches a drafter nor records its digest, as for the other
engines' draft models.

### 6. Readiness, drain, park and wake

Ready is `GET /health` with `"ok": true` and the served name in `/v1/models`, then
the chat probe; a non-empty `content` or `reasoning_content` is an answer. The
probe is part of startup: it is bounded by what remains of the startup bound
(§4: the whole deadline on a first build, the ordinary bound once a build
exists) and by no shorter read bound, because TensorFold 0.6.1 builds more
extensions on its first request (found live 2026-10-02). vLLM and SGLang
probes are bounded the same way.
Parking a TensorFold deployment is the restart-only release: drain, read `/health`
until `requests_running` is 0 and `busy` is false, SIGTERM the owned group, verify
exit, then release memory. A group that is gone, or an engine that does not listen
or answers 503, serves nothing and is signalled at once. A `/health` that does not
answer is signalled once the bound (30 s or the command's remaining time) ends.
Only an engine that still answers `busy` or a running request when the bound ends
is not signalled; that cleanup is uncertain. On a host the check runs before the
Terminate is journaled, so a held-back Terminate is answered unresolved and is
checked again when redelivered (owner decision 2026-10-01). Waking is a fresh launch of the same pinned effective contract.
`capyctl park deployment` on a TensorFold deployment is refused as unsupported.

### 7. Requests

Routing selects the deployment; the forwarded request carries the served name.
Responses pass through unchanged, including `reasoning_content` and the
`tensorfold` object, in both streaming and collected form.

### 8. Load reports

The host scrapes `tensorfold:requests_running`, `tensorfold:requests_waiting` and
`tensorfold:kv_cache_usage_ratio`, and forwards `tensorfold:time_to_first_token_seconds`
and `tensorfold:request_latency_seconds` as engine latency series.

## Consequences

Qualification covers host B (GB10, unified memory) only; discrete GPUs run
TensorFold unqualified until a live row passes there. CPU and Fake-engine tests
are not qualification. Out of scope: deep and host-backed residency, `--tp 2`,
Apple Silicon and MLX, Docker, the recipes repository, `deploy --recipe`, and
fetching or digesting a drafter.
