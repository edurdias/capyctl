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

## 2. Doctor capture + recipe freeze

(To be filled from live `mllm doctor host` output at execution.)

## 3. Restart-only qualification (T07, T10, T11, T12, T15, T16 live)

(To be filled during execution: deploy → READY → serve → stop → re-deploy; A→B→A;
attach; simultaneous wake.)

## 4. Park/reload under the opt-in profile (T20, T21 live)

(To be filled: 3 consecutive clean park→wake→generation-check cycles with measured
request-to-first-token and memory behavior; ambiguous-park failure injection; denial
check with opt-in removed.)

## 5. Evidence ledger

Every step's command + output is recorded in this file at execution; failures are
recorded as failures (the recipe is revised per the owner decision: park/reload is core
functionality — no downgrade, no deferral).