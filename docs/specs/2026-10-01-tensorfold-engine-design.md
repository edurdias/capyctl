# TensorFold engine — design

Date: 2026-10-01. Status: approved in brainstorming with the owner. Target release
0.1.1. It will be recorded as ADR 0023, amending SPEC §1 (supported engines), §9
(a new §9.3) and ADR 0018 §1 (detection and the verified set).

## Problem

CapyCTL runs vLLM and SGLang. TensorFold
(`github.com/ashhart/TensorFold`, Apache-2.0) is a third inference server with an
OpenAI-compatible API and drafter-based parallel decoding on NVIDIA GPUs. Home users
already run it by hand, one model per process. CapyCTL should deploy, route, drain,
park and wake it like the other two engines.

## What the spike established

A spike on 2026-10-01 ran TensorFold 0.6.0 by hand, outside CapyCTL, on host B
(GB10, aarch64, CUDA 13.0):

- It installs into a plain venv (torch 2.13.0+cu130, triton 3.7.1, `ninja`) with no
  container. The NGC container in TensorFold's runbook is not required.
- The first start builds CUDA extensions with `torch.utils.cpp_extension`, so the
  host needs `nvcc`, a C++ compiler and `ninja`. Later starts reuse the build.
- `GET /health` answers `{"ok": true, ...}` only after the model is loaded. It also
  reports `busy` and `requests_running`.
- `/v1/models`, `/v1/chat/completions` (plain and streaming) work.
- SIGTERM ends the process in about 1 s and releases all of its GPU memory.
- A warm relaunch reaches `/health` in 7 s for Nemotron 3.5 Lightning 30B-A3B
  (4-bit MLX checkpoint).
- The server accepts any `model` value in a request, has no API key, and checks
  GitHub for updates at every start unless told not to.
- TensorFold sizes itself from free memory, less `TENSORFOLD_MEMORY_RESERVE_GIB`.
  It has no flag that caps its memory. An explicit `--context` fixes the KV
  allocation, and the startup log prints the estimate (27.11 GiB for Nemotron).

The same checks ran on a maintainer laptop (RTX 4090 Laptop GPU, 16 GB, x86-64,
CUDA 13.4) with Qwen3.8-27B 2-bit (`lukaskremla/Qwen3.8-27B-2bit-MLX-TextOnly`) and
the `z-lab/Qwen3.8-27B-DFlash2` drafter. It loaded in 11.3 GiB at a 15k context,
served plain and streaming chat, and SIGTERM released all memory in 2 s. The first
request took 142 s while kernels finished building. This is raw-engine evidence,
not qualification.

## Owner decisions

- Parking stops the process and waking relaunches it. TensorFold has no sleep,
  release or unload API.
- Live qualification runs on host B in one TensorFold venv in the home directory
  (owner exception, 2026-10-01).
- TensorFold installs are found in a venv, like vLLM and SGLang. Docker support is
  not a prerequisite.
- Draining works as for the other engines.
- Recipes (model, drafter and settings per hardware) live in a separate
  `capyctl-recipes` repository after 0.1.1, not in this change.
- A role's own TensorFold is declared like its own vLLM or SGLang:
  `local_engine.tensorfold`, `--tensorfold-bin`, `CAPYCTL_TENSORFOLD_BIN`, one
  precedence, on a host and in standalone (2026-10-01).
- The drafter follows the rules vLLM and SGLang draft models follow today
  (2026-10-01): an approved path option, no fetch, no digest.
- An undeclared `timeouts.initialize` is 1800 s; the deployment's existing
  `timeouts.initialize` changes it (2026-10-01).
- A TensorFold stop waits up to 30 s for its counters to read idle and fails
  closed after that (2026-10-01).

## Scope

In scope: the `tensorfold` engine kind; detection and `engine add`; launch;
readiness; drain; `restart_only` park and wake; settings; docs and site; CPU and
Fake-engine tests; live qualification on host B.

Qualification covers host B (GB10, unified memory) only. Discrete-GPU hosts run
TensorFold as unqualified until a live row passes on one, and the engines guide
says so.

Out of scope: deep and host-backed residency for TensorFold; multi-rank
(`--tp 2`); Apple Silicon and MLX hosts; Docker; the recipes repository; a
`capyctl deploy --recipe` command.

## Design

### 1. Engine kind

`tensorfold` joins `vllm` and `sglang` everywhere the engine kind is a closed set:
`capyctl_config::engine_policy::Engine`, `capyctl_domain::launch::LaunchSettings`
(with a `TensorfoldLaunchSettings`), `PreparedLaunch` and `NativeEngine` in the
agent, the controller's engine matches, the protocol's latency engine list, the
CLI's engine names, and the config schema's `local_engine` keys. The serde name is
`tensorfold`. The default profile name is `tensorfold`.

### 2. Detection and `engine add`

- `detect` finds a `tensorfold-*.dist-info` under `site-packages`, with the same
  bounds and locations as ADR 0018 §1. It executes nothing.
- `add` reads the version from `dist-info`, then runs `<env>/bin/tensorfold
  --version` under the bounded version check. The entry point is
  `<env>/bin/tensorfold`; `build_fingerprint` is the checked version.
- `add` also checks the build toolchain the first start needs and refuses with
  `toolchain_missing`, naming what is absent. It looks for `ninja`, `nvcc` and a
  C++ compiler (`c++` or `g++`) only on the engine's closed launch `PATH`
  (SPEC §13.3): the installation's `bin`, then the approved `<cuda_home>/bin`,
  then the fixed system directories. The caller's `PATH` is never used. It runs
  nothing to check them.
- The verified set gains TensorFold 0.6.0. Any other version lists as `custom`.
- The capability probe reports no deep-park support, so the profile is written with
  `security.deep_park: disabled` and deployments on it resolve `restart_only`.

### 3. Launch

The agent starts:

```
<env>/bin/tensorfold serve <model dir> --name <served name> \
  --host 127.0.0.1 --port <service port> --no-update-check [engine flags]
```

- The model argument is the resolved local checkpoint directory, as for the other
  engines. CapyCTL never hands TensorFold a Hugging Face id for the target model.
- The launch uses the existing closed engine environment (SPEC §13.3) and the
  existing `cuda_home` profile setting. It adds `TENSORFOLD_NO_UPDATE_CHECK=1`,
  `CUDA_VISIBLE_DEVICES` from the placement, and `TORCH_EXTENSIONS_DIR`.
- `TORCH_EXTENSIONS_DIR` is
  `<state>/engines/tensorfold/<build_fingerprint>/torch_extensions`, owned by the
  service user with mode 0700, like the other sensitive caches in SPEC §13.3.
  Whoever can write there can run code in the engine.
- Reserved flags, set by CapyCTL and refused in `engine_config` and in extra
  arguments: `--host`, `--port`, `--name`, `--alias`, `--backend`, `--context`,
  `--tp`, `--rank`, `--master`, `--master-port`, `--snapshot-dir`, and the
  update-check flags.
- `engine_config` maps to flags: `context_length` to `--context` (required for
  TensorFold), `kv_cache_dtype` to `--kv-dtype`, `max_tokens` to `--max-tokens`, and `thinking` to `--thinking` or
  `--no-thinking`.
- The drafter follows the draft-model rules of the other engines. vLLM takes a
  draft model only inside `--speculative-config` (ADR 0014 Amendment A3) and
  SGLang through `--speculative-draft-model-path` (ADR 0014 §8): an extra
  argument approved by name, whose path must lie inside
  `security.approved_paths`, checked lexically at deploy time and through
  symlinks at launch. Neither accepts a Hugging Face id, fetches the draft model
  or records its digest. TensorFold's `--drafter` is the same: a path option
  passed in `extra_args`, approved by name, inside the approved paths, checked
  again through symlinks before launch. A repository id is refused. Without it
  CapyCTL renders `--drafter none`.
- Other options pass only as extra arguments under SPEC §8.2
  (`accept_extra_args` and host approval). These TensorFold 0.6.0 `serve` options
  are sensitive and need host approval by name:

  | Class | Options |
  |---|---|
  | Listener or egress | `--vision-urls` (fetches image URLs) |
  | Path | `--snapshot-dir` (reserved), `--drafter` |
  | Code | `--lane-kernels` |

  Every other option is an ordinary engine argument.
- A TensorFold deployment needs an explicit `resources` block. TensorFold has no
  memory cap, so the fixed `--context` is what keeps its allocation stable. The
  memory the process uses is checked against the reservation through the same
  observation path as the other engines.
- The engine listens on loopback only (SPEC §9.1 protections). TensorFold has no
  API key, so any local process can reach its port; CapyCTL's routed path is the
  only network path to it. No engine control path is
  exposed, so the deep-park key guard does not apply.

### 4. Readiness

Ready means `GET /health` returns 200 with `"ok": true` and `GET /v1/models` lists
the served name. A listening port alone is not readiness (SPEC §6.1). The startup
bound is 1800 s for a launch whose `TORCH_EXTENSIONS_DIR` has no build yet (the
first start after `engine add` or a version change). Later launches and wakes use
the ordinary startup bound. The profile can change both.

### 5. Draining

Draining stays engine-agnostic: the router closes admission and waits for the
requests it forwarded (SPEC §10). Before it sends a signal, the TensorFold adapter
also reads `/health` and requires `requests_running: 0` and `busy: false`. An
engine whose process group is gone, that does not listen, or that answers 503
serves nothing and is signalled at once. One whose `/health` does not answer is
signalled once the bound passes (owner decision 2026-10-01: CapyCTL's own drain
already counted zero forwarded requests on a loopback-only engine). Only an
engine that still answers `busy: true` or `requests_running` above 0 when the
bound expires fails closed, as an uncertain drain does today.

### 6. Park and wake

TensorFold deployments are `restart_only`. Park is drain, SIGTERM to the owned
process group, then exit verified from ownership evidence, with SIGKILL after the
stop bound. Memory is released only after exit is verified. Wake is a fresh launch
with the same pinned effective contract, then readiness. A `deep` or
`host_backed` residency on a TensorFold profile fails resolution with
`capability_missing`.

### 7. Requests

The router already selects the deployment by name. TensorFold ignores the
request's `model` field, so routing is the only guard, and the forwarded request
keeps the served name. Responses pass through unchanged, including
`reasoning_content` and TensorFold's extra `tensorfold` object.

### 8. Settings

One new setting, as vLLM and SGLang have: the role's own TensorFold executable,
YAML `local_engine.tensorfold`, flag `--tensorfold-bin`, environment variable
`CAPYCTL_TENSORFOLD_BIN`, with the shared precedence (flag > environment > YAML)
on a host and in standalone. One `local_engine` executable is the profile
`local`; several are `local-vllm`, `local-sglang` and `local-tensorfold`. It gets
the same toolchain check and `deep_park: disabled` as `engine add`. TensorFold
reuses the existing `cuda_home` profile setting and its `engine add` detection
(YAML `local_engine.cuda_home`, flag `--cuda-home`, environment variable
`CAPYCTL_CUDA_HOME`).

## Errors

- `toolchain_missing` (`engine add`): names the missing tool and where it looked.
- `capability_missing` (resolution): TensorFold supports `restart_only` only.
- A startup failure (unknown `model_type`, GPU below compute capability 8.9, a
  model that does not fit) surfaces the engine's last log lines through the
  existing launch-failure path. CapyCTL does not pre-check model support.

## Testing

- CPU and Fake-engine tests for each design section, tagged with a new acceptance
  ID, T41 (TensorFold conformance): detection, `add` with a missing tool, launch
  arguments and reserved flags, readiness from `/health`, drain gating on
  `requests_running: 0` and `busy: false`, `restart_only` park and wake, `deep`
  refused, a drafter repository id refused, and the toolchain checked on the
  closed `PATH`.
- Live rows on host B, using Nemotron 3.5 Lightning 30B-A3B through CapyCTL:
  - TF1: `engine add` lists TensorFold 0.6.0.
  - TF2: deploy, then plain and streaming chat succeed. Peak memory stays within
    the reservation.
  - TF3: park releases memory; a request wakes the deployment and succeeds.
  - TF4: a vLLM deployment and a TensorFold deployment switch under memory
    pressure. Peak memory stays within each reservation.
  - TF5: cancel a streaming request partway, then park. `busy` and
    `requests_running` return to idle within the drain bound.
- CPU and Fake-engine tests are not qualification. Only the live rows qualify the
  engine.

## Documentation

- SPEC: TensorFold in §1, a new §9.3, and T41 in §20.
- ADR 0023, with the verified set and detection amendments to ADR 0018.
- Docs: the engines guide (install, toolchain, first-start build, `restart_only`),
  the settings reference, and the release notes for 0.1.1.
- Site: the Platforms table lists vLLM, SGLang and TensorFold.
