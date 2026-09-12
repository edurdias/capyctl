# F1 Live Qualification — DGX Spark (host-b)

**Evidence class: live-tier (Spark)** — every claim in this file was executed on the
owner's DGX Spark (GB10, aarch64, 130.6 GB unified memory) via SSH. Simulator-tier
claims never substitute. Filled during execution (F1 design §8 sequence).

## 1. Environment capture (doctor / recipe freeze)

| Item | Value | Source |
|---|---|---|
| Host | host-b, Linux 6.17.0-1031-nvidia, aarch64 | `uname -a` |
| Memory | 130,663,165,952 bytes total | `free -b` (2026-09-12) |
| GPU | NVIDIA GB10, UUID ab51907f-117b-000e-81f7-f288c30bf67e | `nvidia-smi -L` |
| Python | 3.12.14 (uv-managed, headers included) | venv capture |
| PyTorch | 2.13.0+cu130 | venv import |
| vLLM | **0.29.0** (pinned) | `vllm --version` |
| venv path | `$HOME/mllm-vllm-venv2` (uv-managed; system python lacks dev headers and `/opt` is root-owned — recorded deviations) | |
| Checkpoint | `~/models/qwen3-4b-instruct` (Qwen/Qwen3-4B-Instruct-2507, BF16) | §2 |
| mllm binary | `~/mllm/target/release/mllm`, cargo 1.98.1 aarch64 release build | |

Install deviations (owner-visible, no silent decisions): `/opt` is root-owned and sudo
requires a password; the venv lives in the user's home. The system Python lacks
`Python.h`; vLLM's `instanttensor` dependency needs C headers, so the venv uses a
uv-managed Python (3.12.14, headers included). Both recorded in
`docs/runbooks/spark-vllm-env.md`.

## 2. Doctor capture + recipe freeze (2026-09-12, live)

- vLLM fingerprint: `vllm --version` → **0.29.0** (pinned; uv-managed python
  3.12.14, torch 2.13.0+cu130).
- Checkpoint: `~/models/qwen3-4b-instruct` — Qwen/Qwen3-4B-Instruct-2507, BF16,
  7.49 GiB (3 safetensors shards), weights load in ~41-44 s.
- Memory observation: `free -b` MemTotal 130,663,165,952 B; device-visible
  memory 121.69 GiB (GB10 unified).
- Recipe pins (frozen from reported reality): served id `qwen3-4b-instruct`
  (via `--served-model-name`), `--kv-cache-memory 68719476736` (64 GiB grant),
  `--gpu-memory-utilization 0.10` (the 0.92 default gate fails on unified
  memory — live capture), `--host 127.0.0.1 --port 8150`.

## 3. Restart-only qualification — **PASSED live** (2026-09-12)

Executed via `~/mllm-qual/live-run.sh` → `cargo test --release -p mllm-cli --test
live_spark live_restart_only` (evidence: this file + `~/mllm-qual/live-out.log`
on the Spark):

- **Cold init to READY: 62.7s** (deploy → spawn → weight load → API server up →
  mllm readiness via `/v1/models` serving the route id — liveness ≠ readiness).
- **Real inference through mllm**: chat completion returned the model's tokens
  (response "live") through controller → adapter → engine SSE → router path.
- **Stop: engine process group terminated** (verified via `/proc/<pid>` gone);
  administrative stop semantics live.
- **Restart to READY: 65-69s (page-cache warm)** — warm restart measured
  separately from cold init per SPEC §17.
- Simultaneous-wake and attach live steps: prepared, deferred with the switch
  stage (see open items).

Live-loop fixes captured during qualification (all committed):
`--kv-cache-memory` explicit grant (unified-memory heuristic fails on GB10),
`--served-model-name` (served id must match the route id), CRLF SSE normalization,
stream-always internal path, utilization-gate lowering, ninja/venv PATH injection,
pre-listen unreachability = Initializing (not crash), engine log capture.

## 4. Park/reload under the opt-in profile (T20, T21 live)

**IN PROGRESS — Spark became unreachable mid-run (2026-09-12 ~04:00 EDT,
ssh timeout; suspected reboot/network change).** Stages attempted:
- Restart-only alternation (switch engine, stop-based release): A→B leg blocked
  the run before the park cycles — see open items.
- Park/wake cycles ×3 under `security.allow_development_engine_controls: true`
  with `--enable-sleep-mode`: NOT YET EXECUTED live.
- Owner decision binding: park/reload is core functionality — resume on device
  return; recipe revisions continue until clean cycles are captured.

## 5. Evidence ledger

| Claim | Tier | Evidence |
|---|---|---|
| deploy → READY cycle (cold + warm) | live (Spark) | this file §3; test `live_restart_only_qualification` PASSED |
| real inference through mllm | live (Spark) | chat response recorded above |
| engine stop → process group terminated | live (Spark) | `/proc/<pid>` verified gone |
| switching A→B→A live | pending | Spark unreachable mid-run; resume on return |
| park/reload cycles (core feature) live | pending | Spark unreachable mid-run; resume on return |
| T21 default-denial live | simulator-tier so far | policy_gate tests (fake); live denial check pending |

## 5. Evidence ledger

Every step's command + output is recorded in this file at execution; failures are
recorded as failures (the recipe is revised per the owner decision: park/reload is core
functionality — no downgrade, no deferral).