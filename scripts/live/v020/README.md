# CapyCTL 0.2.0 one-session qualification

This foreground harness runs the rented-VM plan in priority order, preserving JSON
observations after every row. It does not rent a machine, publish a release, or
qualify an engine from CPU tests. A separate supervisor must delete the rental.

## Rental and launch

The rented x86_64 VM has one A100 80 GB (Ampere sm_80), 118 GiB RAM, 16 vCPU
and 984 GB disk. The rate is $1.38/hour; eight hours cost $11.04. The initially
selected RTX PRO 6000 was unavailable. Configure provider
`auto_delete.spend_threshold: 20` before launch. There are no lab-host connections.

Prepare the integration binary on the control machine:

```bash
cargo build --release --locked -p capyctl-cli
scripts/live/v020/run.sh --dry-run
systemd-run --user --unit="capyctl-v020-$RUN_ID" --collect \
  --working-directory="$PWD" \
  python3 scripts/live/shade_session.py \
  --instance "$INSTANCE_UUID" --out "$OUT" --max-seconds 28800 -- \
  bash scripts/live/v020/run.sh --ssh "$SSH_TARGET" --ssh-port "$SSH_PORT" \
  --binary target/release/capyctl --out "$OUT" --max-seconds 28200
```

`shade_session.py` deletes that instance on success, failure or deadline. `--keep`
retains remote files only until provider deletion; it never overrides the supervisor.
SSH and rsync have connection, liveness and overall time bounds. The runner polls
sanitized evidence every 20 seconds. A five-minute disconnection ends the local
run and triggers bounded cleanup; provider deletion is the final safety net.

Flags: `--ssh`, `--ssh-port`, `--binary` (local executable) or `--remote-binary`
(absolute path to an executable already built on the target), `--out`, `--only`
(comma-separated row IDs), `--skip-setup`, `--keep`, `--purge-only`, `--dry-run`, `--max-seconds`,
`--engines FILE`, `--functional-only`. `--dry-run` imports the row catalog and
prints the full remote setup and acceptance plan without SSH or a binary.
`--purge-only` first copies evidence, then removes the stopped session. A normal
run purges unless `--keep`; failed cleanup or failed collection refuses deletion
of the evidence-bearing remote directory.

## Setup and time envelope

Everything created remotely lives in the home directory, principally
`~/capyctl-v020`. Each installer records ownership before creating directories,
refuses unmarked existing installations, and can resume from prior artifacts.
Runtime profiles live under the attempt's archived state, so a resumed run does
not collide with profile names left by a previous attempt.
Sanitized native settings retain quoted placeholders, allowing graph-phase
observations to parse ServerArgs without retaining its credential values.
Embedding discovery checks the served model's nested `capyctl.api` metadata,
not a flattened JSON key.
TensorFold memory sizing parses its native startup's model/stream GiB fields
and labels those rounded diagnostics rather than claiming byte-exact counters.
Isolated measurement arms keep separate profile registries inside archived
attempt state. Lifecycle fixtures use restart-only decisions and a llama.cpp
chat context/slot count that fits their declared KV budget.
SHA-256 manifests are generated for collected/exported evidence only, not for
per-row summaries rooted above the private model and engine installations.
The ADR 0034 sleep-preservation amendment is checked explicitly: vLLM 0.33
must refuse `required` with `sleep_clears_cache`, and `auto` must park/wake
with LMCache off (an unqualified auto installation may instead report
`park_unqualified`). These guards never qualify an installation. LC13–LC16
run their full protocols on preserving vLLM 0.34.0rc1 and SGLang 0.5.23,
including store retention, salts, footprint and latency bounds as applicable.
The setup adds `VLLM_034` at
`$V020/engines/vllm-0.34.0rc1-venv` and `LMCACHE_VLLM_034` at
`$V020/engines/vllm-0.34.0rc1-lmcache057-venv`; `$V020` defaults to
`~/capyctl-v020`. LMCache 0.5.7 is built against that fresh engine's installed
torch, without build isolation and with engine constraints. `LMCACHE`
remains the vLLM 0.33 guard environment, and `LMCACHE_SGLANG` is the separate
SGLang 0.5.23 environment. Source shape, not a version label alone, selects
preservation. No live result or qualified-table row follows from setup.
Embedding render observations accept SGLang's managed native `is_embedding`
setting; its protected-descriptor launch does not expose an engine CLI flag.
Every managed host configuration is validated before its process starts;
editable profiles include log policy and distinct credential references.
OT8 reads JSON timing headers and the complete timing SSE comment, not a
Server-Timing-style string. TRT kernel caches are archived per attempt.
The ISSUE235 control model also declares an 8192-token context for its explicit
2 GiB KV pool, so an impossible control launch cannot mask editable refusal.
llama.cpp embedding fixtures retain the required 32768-token context and eight
slots, deriving their native KV reservation from the GGUF rather than inheriting
the Python-engine fixture's 5 GiB override.
ISSUE235 keeps current-server/current-host editable refusal separate from
old-server/current-host version refusal. It asserts the recorded `0.2.0` host,
`refused` compatibility, the human-readable newer-host reason plus
`host_version_newer_than_server` in the host log, and no reconciled
session; reconnecting backoff is expected. Other old-server fixtures enroll
matching-old hosts by default.
EM2 explicitly enrolls Qwen3-Embedding-4B's Matryoshka dimensions with
`--hf-overrides '{"is_matryoshka":true}'` on vLLM and
`--json-model-override-args '{"is_matryoshka":true}'` on SGLang. Launch evidence
retains the requested override and effective native settings. Both Python engines must
return 256-length vectors; llama.cpp must refuse with `400 invalid_request`.
EM8 failures retain sanitized router and engine logs before managed cleanup;
per-request evidence includes the complete request, response and elapsed timing.
EM8 declares 16 concurrent engine slots for its 16-client matrix cells; queue
refusals and incomplete responses still fail the row rather than count as samples.
OT11 checks SGLang's actual `not accepted here` profile diagnostic rather than
requiring the word `reserved`; full sanitized refusal text remains in evidence.
TensorRT-LLM 1.2.1 pins NumPy 1.26.4 and PyArrow 21: its NumPy `<2`
requirement conflicts with PyArrow 26's import-time NumPy `>=2` requirement,
which its wheel metadata does not declare. The rc3 environment is unchanged.
TensorRT fixtures omit an implicit context length so the controller fits it to
the fixed KV budget; TR3 expects the engine's whole-block floor, not a ceiling.
No apt, driver update, system CUDA modification or reboot is allowed. Missing
compilers/tools are blocking preflight errors, not permission to install packages.

| Phase | Estimated minutes | Estimated cost |
|---|---:|---:|
| uv, Python 3.12, tools and toolkit detection/fallback | 3–8 | $0.11–0.29 |
| Concurrent engine environments, native archives, model downloads and llama.cpp build | 25–50 | $0.91–1.83 |
| Priority 1 discrete regressions and cache correctness/endurance | 120–180 | $4.38–6.57 |
| Priority 2 measurement matrix | 90–150 | $3.29–5.48 |
| Priority 3 latest-engine serving and inexpensive Priority 4 rows | 45–90 | $1.64–3.29 |
| Final collection and cleanup reserve | 5–10 | $0.18–0.37 |

Concurrent setup targets one hour, not the sum of the installation times. The
outer deadline is authoritative: later rows receive failure evidence if not
reached, never an invented pass. Row budgets include startup and cleanup and
are printed by `run.sh --dry-run`; total worst-case row budgets may exceed the
session envelope. Priority 1 always runs first. Skips are not release qualification.

The installed versions are vLLM 0.32.0/0.33.0, SGLang 0.5.22/0.5.23 plus an
isolated editable 0.5.23, LMCache 0.5.7 built from its source tag in separate
vLLM 0.33.0 and SGLang 0.5.23 environments, TensorFold native 1.2.6/1.1.5 and
otelcol 0.164.0. LMCache builds keep the engine's torch pinned, use CUDA major
13 and the detected GPU architecture (8.0 on the A100), and do not reuse a wheel
compiled against another engine's torch. Unified-memory cache rows accept the
approved vLLM 0.33.0/LMCache 0.5.7 tuple as well as the existing 0.32.0/0.5.6 tuple;
the GPU memory architecture does not select a package version.
TensorFold 0.6.6 is an optional TF3 comparison: explicitly select TF3 to install
it, or supply its mapping. It does not delay the default setup.
The editable SGLang fixture disables optional Rust extensions: #235 must refuse
it before any engine launch, and setup does not install an extra Rust toolchain.

TensorRT-LLM has separate row-gated no-apt installations: 1.2.1 with torch
2.9.1+cu130, and 1.4.0rc3 with torch 2.14.0+cu132; pip Open MPI 4.1.8 and mpi4py
supply MPI. NVIDIA wheels are installed with the engine's torch constraint.
The rc3 CUDA availability and tiny-kernel preflight may report
`skip: driver does not support cu132`; its sanitized error stays in evidence.
A supplied engine mapping is read-only. Missing non-driver prerequisites fail
selected TRT rows, never silently pass them.

The original requested llama.cpp wrapper interface was superseded by the parent:
its `LLAMACPP_TAG=v0.9.0` and `LLAMACPP_CUDA_ARCH=120a-real` variables describe the
wrapper, but this harness uses a small local build function instead of shimming
its SSH dependency. The pin comes from `scripts/live/llamacpp-build.sh` at
`63f91fc8`: tag v0.9.0 must resolve exactly to
`cd959bae99ff4bed22bef2604f45241c09bbb8c8`. The build selects CUDA, Release,
the detected GPU architecture (`80-real` on the A100), disables dev/Connect/UI/
prebuilt UI, and builds `llama-server` with `nproc` jobs using the detected toolkit.
CUDA-toolkit wheels supply nvcc when the image does not. The TensorRT forced-PTX
preflight likewise targets the detected GPU rather than a fixed architecture.
The private toolkit includes the sparse/solver headers needed by torch extension
builds; nvcc uses the preflight's existing C++ compiler. Concurrent uv installers
share downloads with a 30-minute cache-lock timeout rather than failing at five.
Private toolkit bins also expose the VM's existing matched GCC/G++ pair for
managed engines' closed PATH; no system compiler or engine package is changed.
The two-model ISSUE225 fixture uses 8192-token contexts so its explicit 2 GiB
KV pools can admit a full request instead of inheriting a 32768-token context.

Models are downloaded with `hf` concurrently with installs. Supplied revision
pins are resolved to full commits; plan entries without revisions resolve once
and are locked in `models.lock.json` before download. No later attempt follows
`main` again. TensorFold archives use the plan's exact SHA-256 digests; the old
CapyCTL release verifies published SHA256SUMS and internal checksums.

## Approved unified-memory qualification

Measurements may run on an explicitly approved idle aarch64 host without
`--functional-only` or `--skip-setup`. Provision only the owner-approved engine
environments first, then supply `--engines FILE` with absolute read-only mappings
for every engine key, including explicit empty values for unavailable engines.
On aarch64, setup refuses an omitted engine mapping before installing tools;
it never creates or changes engine environments. Session tools, missing models
and the checksum-verified aarch64 old binary remain private setup artifacts.
Supply `OTELCOL` and `CUDA_HOME` as read-only overrides too.

GB10's unavailable global framebuffer counter is never converted to zero.
Samples use numeric NVML per-process CUDA allocations and label their source
`nvml_compute_process_allocations`, separately from physical framebuffer use.
Host `MemAvailable` and process RSS remain separately recorded.

If preparation already claimed the live marker, set
`V020_SESSION_NAME=HostAFull` when invoking `run.sh`; the existing marker must
contain exactly that name. The runner preserves exclusivity and removes only
its own marker after successful teardown. An unset name retains the default
refusal of any pre-existing marker.

This harness currently enrolls a co-resident host role for LC15/LC15U, not an
independent second machine. Rows whose catalog says remote observation is not
implemented remain explicit skips; supplying another host cannot certify them.

## Existing shared host: functional only

Never run setup on the shared unified-memory machine. Supply an untracked small
machine-specific mapping and use `--functional-only --skip-setup --engines FILE`.
The file is parsed as data, never sourced as shell code. Values may be quoted;
`~` expands on the remote machine. Example paths intentionally use `/home/me`:

```text
VLLM_032=/home/me/vllm-0.32.0-venv
VLLM_033=/home/me/vllm-0.33.0-venv
VLLM_034=
SGLANG_0522=/home/me/sglang-0.5.22-venv
SGLANG_0523=
SGLANG_EDITABLE=
LMCACHE=/home/me/vllm-0.32.0-lmcache-venv
LMCACHE_VLLM_034=
LMCACHE_SGLANG=
LLAMACPP=/home/me/llamacpp-v0.7.0/build/bin/llama-server
TENSORFOLD_126=/home/me/tensorfold-1.0.0
TENSORFOLD_115=/home/me/tensorfold-1.1.5
TENSORRT_LLM=/home/me/tensorrt-llm-1.2.1-venv
OTELCOL=/home/me/otelcol-0.164.0/otelcol
MODEL_CHAT=/home/me/models/qwen3-4b-instruct
MODEL_CHAT_GGUF=/home/me/models/qwen3-4b-gguf
MODEL_EMBED=/home/me/models/emb
MODEL_EMBED_GGUF=/home/me/models/emb-gguf
MODEL_EMBED8=/home/me/models/emb8
MODEL_CLEF=/home/me/models/clef-flash
MODEL_CLEF_GGUF=/home/me/models/clef-flash-gguf
MODEL_TENSORFOLD=/home/me/models/nemotron-lightning
CAPYCTL_OLD=/home/me/capyctl-0.1.12/bin/capyctl
```

Mappings name actual existing versions; a 1.0.0 path does not certify 1.2.6.
The setup-generated VM map is the authoritative example of all available keys.
An empty mapping means unavailable, not permission to install. Engine/model
symlinks in the private model store never modify the mapped paths. Use an
aarch64 0.2.0 binary for an aarch64 host. Additional engine-version mappings can
be selected by the operator; the harness does not infer an upgrade permission.

Measurement rows are `skip: measurement not allowed on this host`. Discrete-only
rows are `skip: needs a discrete GPU`; unified-only rows run only on unified
hardware. Functional serving still validates lifecycle and answers, but never
substitutes those observations for timing/memory qualification. A session marker
`~/LIVE-SESSION-IN-USE.txt` prevents overlap and is removed after verified cleanup.
Only recorded CapyCTL roles are signalled; engine cleanup goes through CapyCTL.
The final GPU PID set must be a subset of the pre-session set, preserving the
owner's pre-existing training processes. Failure leaves the marker and refuses
purge for investigation.

SGLang's managed wrapper passes native `ServerArgs` keywords through a protected
descriptor, not an engine CLI. ISSUE236 therefore observes the launch-scoped
resolved dictionary (including the effective graph backends), without opening
protected descriptors or reading engine keys.
The harness's logical `wake` operation uses `capyctl start deployment`, the
supported CLI command for returning a parked deployment to ready.
Cache fixtures explicitly request token-ID lists from Transformers; dictionary
return defaults must not be mistaken for a two-token prompt.
Wake canary evidence follows the durable current-launch `wake_canaries` reference
and the latest completed `restored` journal entry. EM4 is different: ADR 0036
explicitly requires a restart comparison verdict event; a fresh reference alone
does not pass that row.
Embedding requests checkpoint their input and sanitized response before contract
assertions, so a failed request retains its actual HTTP error. Multi-engine rows
checkpoint each available engine before an unavailable installation skips the row.
Failed managed starts checkpoint sanitized engine diagnostics before deployment
cleanup, alongside the failed CLI operation.

## Evidence and verdicts

`summary.txt` gives exactly one `pass`, `fail` or `skip` plus reason per row;
`summary.json` also includes priority, acceptance criteria and time budget.
`evidence/result-ROW.json` contains the observations for each completed row,
including actual numbers for measurement rows. Missing rows fail. An earlier
failure or skip cannot be hidden by a later pass. `selected_pass` concerns only
selected rows; `qualified` requires every catalog row to pass, including P4.

P4 protocols not implemented are explicitly skipped with their missing protocol
named. EM8 throughput/large-response evidence is still collected when no public
retrieval set is pinned; its retrieval half is `skip: retrieval set not pinned`,
not a retrieval-score claim. Full TF3 comparison needs the optional 0.6.6 baseline.
LMCache `required` can test uncertified engine-driven transport without silently
changing the product's qualification table.

The SHA256SUMS manifest covers durable evidence and settings, excluding itself
and symlinks. `settings.json` stores the binary hash and nonsecret flags, never
SSH targets, full command lines, environment dumps or credentials. Raw remote
state/logs and credentials are not copied. Standalone credentials use the native
`name: value` format; server credentials are JSON. Only the two credential strings
are loaded for sanitization; metadata fields are excluded. No engine key is read.
The local transport completion records collection and purge separately from row success.

Run the new CPU helper tests and shell checks only:

```bash
python3 -m unittest discover -s scripts/live/v020 -p test_v020.py -v
shellcheck scripts/live/v020/run.sh scripts/live/v020/remote.sh scripts/live/v020/setup.sh
scripts/live/v020/run.sh --dry-run
```

Shellcheck 0.11.0 is the repository's pinned version. These checks demonstrate
harness behavior, not successful native-engine qualification.

## Row acceptance and budgets

Budgets are maximum seconds per row, not elapsed-time promises.

| Row | Priority | Budget (s) | Pass criterion / explicit unimplemented protocol |
|---|---:|---:|---|
| ISSUE225 | P1 | 1500 | vLLM 0.32.0 and 0.33.0 each serve two co-resident models, each rendered utilization <0.75, with positive explicit KV bytes |
| LC13 | P1 | 1000 | ADR0034 §2: 32k park/wake correctness, whole-chunk hits, salts, store race, footprint and wake bounds; uncertified required versus auto. |
| LC14 | P1 | 1100 | ADR0034 §2: 15 alternating-prompt park/wake cycles, off-reference answers, no held KV or server stops, covered charges and <=256MiB server GPU growth. |
| LC15 | P1 | 600 | ADR0034 §2: enrolled host through the control server, 32k park/wake/hit and passing wake canary. |
| LC16 | P1 | 900 | ADR0034 §2: five cold/engine-evicted/post-park trials at 8k/16k/32k; wake <=1.1x off, cold TTFT <=1.1x off, 32k post-wake TTFT <=0.25x off. |
| ISSUE236 | P1 | 1200 | SGLang 0.5.22 and 0.5.23 answer with prefill disabled and decode native in managed native ServerArgs settings |
| ISSUE235 | P1 | 1500 | Current server/current host refuse editable SGLang before launch; a 0.1.12 server refuses a 0.2.0 host with host_version_newer_than_server (ADR 0017 §2). |
| ISSUE240 | P1 | 1800 | 48 durable in-flight leases coexist on a managed engine; traffic survives server-first then host upgrade from 0.1.12 to 0.2.0 |
| EM1 | P2 | 900 | Qwen3-Embedding-4B BF16/Q8_0: native 2560 dimensions, ordered batch, API/certification/LMCache evidence and memory/start/charge. |
| EM8 | P2 | 1800 | Embedding latency/load matrix and pinned retrieval; Qwen8B 2048-input float/base64 router memory bound. |
| OT8 | P2 | 3000 | 200 requests per tracing arm/engine, phase arrays and throughput; SGLang trace levels 1 and 3. |
| TF2 | P2 | 900 | TensorFold cold/warm readiness, full-context eight-stream device/process/host memory and startup sizing. |
| TF3 | P2 | 1200 | TensorFold request options, drafted/no-draft greedy SHA equivalence and 1/8 stream baseline throughput. |
| TR3 | P2 | 3000 | TensorRT cold/three warm starts, 30-minute mixed traffic and unchanged block-rounded pool beside 20GiB allocation. |
| TR3-140RC3 | P2 | 3000 | TensorRT-LLM 1.4.0rc3: full TR3 protocol after tiny CUDA kernel preflight. |
| P3-vllm033 | P3 | 600 | Register, ready, collected and SSE chat, verified stop; deep park/wake on vLLM/SGLang. |
| P3-sglang0522 | P3 | 600 | Register, ready, collected and SSE chat, verified stop; deep park/wake on vLLM/SGLang. |
| P3-sglang0523 | P3 | 600 | Register, ready, collected and SSE chat, verified stop; deep park/wake on vLLM/SGLang. |
| P3-llamacpp090 | P3 | 600 | Register, ready, collected and SSE chat, verified stop; deep park/wake on vLLM/SGLang. |
| P3-tensorfold126 | P3 | 600 | Register, ready, collected and SSE chat, verified stop; deep park/wake on vLLM/SGLang. |
| P3-TENSORRT_LLM_121 | P3 | 600 | Register, ready, collected and SSE chat, verified stop; deep park/wake on vLLM/SGLang. |
| P3-TENSORRT_LLM_140RC3 | P3 | 600 | Register, ready, collected and SSE chat, verified stop; deep park/wake on vLLM/SGLang. |
| P3-decisions-sglang0523 | P3 | 600 | Clef-Flash becomes ready and answers /v1/systemone, then stops with verified exit. |
| P3-decisions-llamacpp090 | P3 | 600 | Clef-Flash becomes ready and answers /v1/systemone, then stops with verified exit. |
| LC13U | P4 | 1000 | ADR0034 §2: 32k park/wake correctness, whole-chunk hits, salts, store race, footprint and wake bounds; uncertified required versus auto. GB10 deep-parking counterpart. |
| LC14U | P4 | 1100 | ADR0034 §2: 15 alternating-prompt park/wake cycles, off-reference answers, no held KV or server stops, covered charges and <=256MiB server GPU growth. GB10 deep-parking counterpart. |
| LC15U | P4 | 600 | ADR0034 §2: enrolled host through the control server, 32k park/wake/hit and passing wake canary. GB10 deep-parking counterpart. |
| LC16U | P4 | 900 | ADR0034 §2: five cold/engine-evicted/post-park trials at 8k/16k/32k; wake <=1.1x off, cold TTFT <=1.1x off, 32k post-wake TTFT <=0.25x off. GB10 deep-parking counterpart. |
| EM3 | P4 | 1 | not implemented: Needs pinned 100-sentence cross-engine fixture and concurrent 64-input canary protocol; ordinary request success cannot establish tolerance. |
| EM5 | P4 | 1 | not implemented: Needs client disconnect/deadline injection and lease/engine-connection settlement observation, unavailable through collected Context.request. |
| EM6 | P4 | 1 | not implemented: Needs separately enrolled host ingress and engine-side request-header observation. |
| EM7 | P4 | 1 | not implemented: Needs enforced mutually exclusive memory grants and pool-switch evidence, not two independently fitting deployments. |
| OT1 | P4 | 1 | not implemented: Needs owned-process socket and sanitized environment-name inspection to prove tracing is absent, not just successful requests. |
| OT2 | P4 | 1 | not implemented: Needs loopback collector and trace/span parent/request-id correlation including decisions. |
| OT3 | P4 | 1 | not implemented: Needs collector capture including engine spans and engine validation-error sentinel coverage. |
| OT4 | P4 | 1 | not implemented: Needs collector activation links and phase timing comparison within 1 ms. |
| OT5 | P4 | 1 | not implemented: Needs engine and router span parentage plus engine-side header observation. |
| OT6 | P4 | 1 | not implemented: Needs independently configured server/host collectors and ingress span correlation. |
| OT7 | P4 | 1 | not implemented: Needs controlled collector outage/restart, dropped-span counters and OT8 noise baseline. |
| OT9 | P4 | 1 | not implemented: Needs collector evidence of CapyCTL spans present and engine spans absent under retention zero. |
| OT10 | P4 | 1 | not implemented: Needs approved SDK-free environment and static package-shape inspection; installed environments must not be modified. |
| TF4 | P4 | 1 | not implemented: Needs admission pressure fixture, scratch default-reserve launch and all native refusal cases, not only capability refusals. |
| TF5 | P4 | 1 | not implemented: Needs eight live streams, zero-gauge drain proof and a deliberately busy 30-second stop bound. |
| TF6 | P4 | 1 | not implemented: Needs client-cut protocol with 100 ms gauges, six repeats and 32-cancellation restart proof. |
| TF7 | P4 | 1 | not implemented: Needs enrolled host, per-pool metrics/histograms and cancelled-request remote settlement. |
| TF8 | P4 | 1 | not implemented: Needs constrained mutually exclusive TensorFold/vLLM grants and measured fresh-launch wake canary. |
| TF9 | P4 | 1 | not implemented: Requires approved TensorFold 0.6.6 environment, absent from fresh VM recipe. |
| TF10 | P4 | 1 | not implemented: Needs listener enumeration after readiness and a controller re-proof trigger; ownership PID alone is insufficient. |
| TF12 | P4 | 1 | not implemented: Needs direct health progression and startup/device memory metrics for 1.1.5, plus reserved idle-unload refusal. |
| TF13 | P4 | 1 | not implemented: Needs in-stop health 503/draining observation, signal-to-exit timing and busy bound fixture. |
| TF14 | P4 | 1 | not implemented: Needs TF6 cancellation protocol and TF7 enrolled-host metrics/settlement on 1.1.5. |
| TR1 | P4 | 1 | not implemented: Needs wheel provenance, non-executing detection audit, CUDA/OpenMPI companion evidence and no-CUDA-context registration observation. |
| TR2 | P4 | 1 | not implemented: Needs direct version/readiness step evidence, rendered YAML and both tool-call/reasoning parser assertions; P3 chat is not equivalent. |
| TR4 | P4 | 1 | not implemented: Needs the single load-reader's burst/lull observations, not direct metrics polling that starts another reader. |
| TR5 | P4 | 1 | not implemented: Needs client-cut injection, engine abort timing, uncertain leases and 32-cut single-restart proof. |
| TR6 | P4 | 1 | not implemented: Needs per-process memory disappearance, five fresh-launch cycles and exact deep refusal code, beyond P3 verified stop. |
| TR7 | P4 | 1 | not implemented: Needs complete pre-launch refusal matrix including reserved LLM API nested keys and exact refusal codes. |
| TR8 | P4 | 1 | not implemented: Needs cold-start outbound sockets sampled each second and sanitized telemetry/offline environment-name assertions. |
| TR9 | P4 | 1 | not implemented: Needs owned-process SIGKILL, process-group/listener proof and release-order evidence. |
| TR10 | P4 | 1 | not implemented: Needs authorized unmanaged MPI baseline with process/session/listener inventory; managed throughput alone is insufficient. |
| TR11 | P4 | 1 | not implemented: Needs enrolled host ingress and TR5's cancellation settlement protocol. |
| LC12 | P4 | 1 | not implemented: Needs historical lmcache_driven rendering and GB10 fabric-handle readout; fresh VM is discrete and must not alter existing GB10 environments. |
| LC17 | P4 | 1 | not implemented: Needs isolated host_kv_limit configurations and LMCache token-hit/pool-charge evidence across capped and explicit pools. |
| LC18 | P4 | 1 | not implemented: Needs build rendering engine-driven restart transfer and 15-cycle LMCache hit, held-KV and stop-bound observations. |
| EM2 | P4 | 1200 | Full embedding input/encoding/dimension/error contract on vLLM, SGLang and llama.cpp. |
| EM4 | P4 | 1200 | Restart each embeddings deployment and require a journaled passing reference canary. |
| EM9 | P4 | 900 | Five deep embedding park/wake cycles and a parked-arrival wake on each Python engine; functional evidence without memory measurements. |
| TF1 | P4 | 120 | TensorFold 1.2.6 registration, archive checksums and native capabilities. |
| TF11 | P4 | 120 | TensorFold 1.1.5 registration beside 1.2.6, archive checksums and native capabilities. |
| OT11 | P4 | 300 | Non-loopback engine tracing and reserved profile argument/environment refusals. |
