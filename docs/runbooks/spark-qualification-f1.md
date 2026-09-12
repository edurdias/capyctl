# F1 Live Qualification — DGX Spark (host-b / host-a)

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
  (via `--served-model-name`), `--kv-cache-memory 17179869184` (16 GiB grant),
  `--gpu-memory-utilization 0.10` (the 0.92 default gate fails on unified
  memory — live capture), `--max-model-len 4096`,
  `--host 127.0.0.1 --port 8150`.

The original 64 GiB grant was superseded after switch overlap exhausted unified
memory on both Sparks. The 16 GiB grant is the qualified lab recipe, not dynamic
admission: F1 still uses synthetic capacity observations. Host-capacity-based
sizing and fail-fast diagnostics remain required F2 work.

## 3. Restart-only qualification — **PASSED live** (2026-09-12)

Executed via `~/mllm-qual/live-run.sh` → `cargo test --release -p mllm-cli --test
live_spark live_restart_only` (evidence: this file + `~/mllm-qual/live-out.log`
on the Spark):

- **Cold init to READY: 62.7s** (deploy → spawn → weight load → API server up →
  mllm readiness via `/v1/models` serving the route id — liveness ≠ readiness).
- **Real inference through mllm**: chat completion returned the model's tokens
  (response "live") through the wired forwarder (controller → adapter → engine
  SSE; router dispatch exercised at simulator tier).
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

**PASSED live (2026-09-12): three level-2 park/reload cycles with authenticated
routed inference after every reload, stock switching, default denial, and
ambiguous-park injection.**

Commands ran in `~/mllm` with `MLLM_LIVE=1`, `MLLM_PORT=8150`,
`MLLM_VLLM_BIN=~/mllm-vllm-venv2/bin/vllm`,
`MLLM_ENGINE_PATH=~/mllm-vllm-venv2/bin`,
`MLLM_MODEL_PATH=~/models/qwen3-4b-instruct`, and
`MLLM_MODEL_ID=qwen3-4b-instruct` (tilde paths expanded by the launching shell).
Each command used `cargo test --release -p mllm-cli --test live_spark <test>
-- --nocapture`. Run these named checks separately, confirming the previous
engine exited before the next check: the recipe uses one engine port.

| Stage | Host / test | Captured result |
|---|---|---|
| A→B→A, stock profile | host-b / `live_switch_restart_only` | PASS, 234.24s; A generation 2→4, B generation 2; A stopped before B launch |
| Level-2 park→wake ×3 | host-a / `live_park_reload` | PASS, 212.28s; each reload followed by authenticated HTTP inference through mllm returning `live`; stale-generation dispatch rejected |
| T21 default denial | host-a / `live_default_denies_sleep_profile` | PASS, 0.06s; `policy_denied`, no engine PID |
| T20 lost acknowledgement | host-b / `live_ambiguous_park_reconciles` | PASS, 63.95s (final harness); real `/sleep` succeeded, downstream ack dropped, engine confirmed sleeping, one request/park operation, uncertainty recorded, final state Failed |

The final three successful cycles freed 24.58 / 24.78 / 24.81 GiB according to vLLM;
weight reload took 49.95 / 44.98 / 43.81s. `/sleep`, `/wake_up`, and
`/collective_rpc` each returned 200. These are engine-reported memory numbers,
not scheduler admission estimates. Both hosts returned to approximately 118 GiB
available memory with no swap in use after cleanup.

Corrections established against the installed vLLM 0.29.0:

- `entrypoints/launchers/api_server/routers.py:34` registers development routers
  only with `VLLM_SERVER_DEV_MODE=1`; `--enable-sleep-mode` alone returned 404.
  mllm now sets the environment explicitly to 1 under the opt-in, 0 otherwise.
- `/collective_rpc` requires `{"method":"reload_weights"}`; the empty request
  returned 400. A successful wake allocation alone does not restore weights.
- The original 30s control timeout expired during a measured 46.14s reload.
  Reload now has a separate bounded 120s timeout; sleep/wake retain 30s.
- Stock switch release previously attempted park despite the shared port.
  Stock deployments now use idle-stop; park-based switching still requires
  per-deployment ports (open item 2).
- Review caught a missing `vllm-sleep` forwarder registration. The first routed
  probe failed with `unsupported: no forwarder for profile vllm-sleep` after
  reload; registering the profile made all three authenticated routed
  completions pass. The earlier 221.70s adapter-only run is retained as
  intermediate evidence, not substituted for the final routed check.

Machine-local evidence (OS-managed `/tmp` state is not permanent):

- host-a final: `~/mllm-qual/park-router-green.log`,
  `<temporary-directory>/engine.log.*`; routed red:
  `~/mllm-qual/park-router-red.log`, `<temporary-directory>/engine.log.*`.
  Intermediate: `~/mllm-qual/park-reload-fix-2.log`,
  `<temporary-directory>/engine.log.*`; failed short-timeout run:
  `~/mllm-qual/park-reload-fix.log`, `<temporary-directory>/engine.log.*`.
- host-b: `~/mllm-qual/switch-fix.log`, `<temporary-directory>/engine.log.*`;
  `~/mllm-qual/ambiguous-park-final.log`, `<temporary-directory>/server/`.
  Earlier injection: `~/mllm-qual/ambiguous-park.log`,
  `<temporary-directory>/server/srv.sqlite3` (read-only query confirmed journal
  states `reconciling` then `failed` for the uncertain park).

The failure-injection test uses a second controller sharing the real deployment
store, with its adapter pointed at a lost-ack proxy that counts connections for
one second after dropping the acknowledgement. No model state is
simulated. F1 has no Stop transition from Failed, so the test terminates its own
verified engine process group after recording evidence; this is not evidence of
automatic production recovery from Failed.

## 5. Evidence ledger

| Claim | Tier | Evidence |
|---|---|---|
| deploy → READY cycle (cold + warm) | live (Spark) | this file §3; test `live_restart_only_qualification` PASSED |
| real inference through mllm | live (Spark) | chat response recorded above |
| engine stop → process group terminated | live (Spark) | `/proc/<pid>` verified gone |
| switching A→B→A live | live (host-b) | §4; `live_switch_restart_only` PASSED |
| park/reload cycles (core feature) live | live (host-a) | §4; three cycles with authenticated routed inference PASSED |
| T21 default-denial live | live (host-a) | §4; profile rejected before spawn |
| T20 ambiguous park | live (host-b) | §4; real effect, lost ack, uncertainty and Failed without repeated park operation |

Every step's command + output is recorded in this file at execution; failures are
recorded as failures (the recipe is revised per the owner decision: park/reload is core
functionality — no downgrade, no deferral).
