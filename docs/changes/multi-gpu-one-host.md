# Status: One model across several GPUs of one host (ADR 0035), implemented; live qualification incomplete — 2026-10-26 (branch `feat/multi-gpu-one-host`)

ADR 0035 (owner decisions 2026-10-25) is implemented for 0.1.9. A deployment with
`topology: {tensor_parallel: N}` and no `placement.hosts` runs as one instance on N
identical discrete GPUs of one host, on an enrolled host and on standalone; `devices` may
pin exactly N GPUs, and `topology: auto` lets CapyCTL choose N.

- **Configuration** (§1, §1a): a world size above 1 without two or more named hosts is one
  host, not a group; `pipeline_parallel` above 1 there, a unified host, TensorFold, custom
  engines and decision deployments are refused with their codes, and so are pinned GPUs that
  differ (`multi_gpu_mixed_devices`), a pinned count other than N
  (`topology_devices_mismatch`) and, on vLLM and SGLang, an N that does not divide the
  model's attention heads (`tensor_parallel_indivisible`). `auto` records a candidate table
  in the revision (each N's per-GPU and host-RAM charge), is refused beside named hosts,
  `devices`, `resources`, `memory.request` and `memory.startup`, and narrows to N = 1 under a
  stated parking tier or `lmcache: required`.
- **Accounting** (§3): one owner, one allocation per chosen GPU's `device` domain (ADR 0028's
  rank share with `T = N`, KV cache, margin, 1.25 GiB CUDA context, 1 GiB communication
  allowance, both `derived` until measured) and the system domain (4 GiB plus 3 GiB per rank
  after the first). Until a park measures it, each GPU's parked charge is those two
  allowances (2.25 GiB), what a sleeping rank keeps on its GPU; one GPU keeps 1 GiB. Every
  refusal for memory or GPU count names the `tensor_parallel` that would fit.
- **Placement** (§2, §1a): sets of N identical GPUs, the largest smallest headroom first;
  eviction by the smallest union of victims, a multi-GPU victim whole; the chosen set and N
  recorded with the instance (new forward-only schema version) and kept through parks and
  wakes; `auto` chooses N from the cards' managed limits, never their free memory, and
  journals `topology_auto_changed` when a later start chooses another N. Measured peaks and
  parked residue are kept per N.
- **Readings and protocol** (§8, §11): `nvidia-smi --query-compute-apps=pid,gpu_uuid,used_memory`,
  one reading per (process, GPU), each GPU's domain credited with the bytes on that GPU; an
  unknown UUID refuses the sample. `ProcessResidency.per_device`, `SingleLaunchPlan.devices`
  and the host capability `local_tensor_parallel` (an older host is refused
  `host_capability_missing:local_tensor_parallel` before anything is sent).
- **Launch** (§4): vLLM `--tensor-parallel-size N --distributed-executor-backend mp`, SGLang
  `tp_size` N with `base_gpu_id` 0 and `gpu_id_step` 1, llama.cpp `--split-mode layer`,
  `--tensor-split` and `--main-gpu 0` with `NCCL_SOCKET_IFNAME=lo`, each on the GPUs' UUIDs in
  rank order; the frozen plans pin N and a drift refuses the launch (`tp_drift:<field>`); the
  single-host loopback pin applies unchanged.
- **Ownership and failure** (§5–§7): rank processes are named at readiness from per-GPU
  readings (`rank-N`; `tp_rank_unobserved` at the initialize timeout) and watched as engine
  processes; one rank's exit fails and stops the instance (`engine_rank_exited`) with nothing
  released before every process is gone; ADR 0028's stall rule covers every N > 1 instance
  (`tp_stalled`); ADR 0031's listener check runs after readiness and at every re-proof
  (`engine_listener_exposed`). Status shows `collectives: loopback` and `TP auto (N)`.
- **Parking** (§9): vLLM and SGLang deep parking at N > 1 is built on per-GPU evidence and
  ships off in the support data (owner decision 3): until MG4 and MG5 pass, a multi-GPU
  deployment resolves `restart_only` and a stated `deep` or `host_backed` is refused
  `capability_missing:deep_park`. With it on, a vLLM park settles only when, after the
  engine's reply, a host reading of every chosen GPU shows the launch's own processes at or
  below that GPU's parked charge (above it for a wake), awaited for the group evidence
  window; a GPU with no fresh reading, a reading without per-GPU figures or a GPU on the
  wrong side of its charge leaves the step uncertain with every charge kept, and the store
  refuses to complete such a step on the engine's reply alone. SGLang's host reads each
  rank's own saver enrollment (`<binding>.rank<r>`, its map on its own GPU) and a park or
  wake settles only when every rank agrees; a missing or disagreeing rank is unknown. The
  controller's saver observer judges each device the same way. The parked residue is
  credited per GPU (each GPU its own bytes, never a process's sum over every GPU). LMCache
  is off for N > 1 (`multi_gpu_unqualified`).
- **Docs**: SPEC §7.2, §8.2, §9.1, §9.2, §9.4, §11, §15.1 and §20 (T46, MG1–MG14); ADR 0019,
  0027, 0028 and 0029 amendments; the deploy, parking, errors, engine flags and
  several-machines guides; the install and settings references. AGENTS.md records the rented
  machine's one session and its engine environments.
- **Live script set**: `scripts/live/multi-gpu/run.sh --ssh <user>@<host>` runs every row,
  MG1–MG14 and a download bandwidth test, on the rented two-GPU machine in one session,
  installing only vLLM 0.32.0, SGLang 0.5.22 and the llama.cpp v0.7.0 build; MG4–MG6, MG10
  and MG13 run on a qualification build with vLLM's and SGLang's `deep_park` at N > 1 turned
  on (the change MG4 and MG5 make when they pass). At the end it writes `verdict.md` under
  `target/live/multi-gpu/<run>/` and removes everything it created
  ([its README](../../scripts/live/multi-gpu/README.md)). `--dry-run` prints the whole plan
  and checks that this checkout's qualification edit flips exactly those two entries.

Tests (T46), new in `capyctl-config` (`tests/multi_gpu.rs`), `capyctl-store`
(`tests/multi_gpu_placement.rs`, the v58 migration, the per-GPU park and wake judgment and
the per-GPU residue), `capyctl-scheduler`, `capyctl-domain` (`local_tp`), `capyctl-agent`
(the per-rank saver read and each device-list refusal), `capyctl-controller` (the saver
observer per device), `capyctl-router`, `capyctl-protocol` and `capyctl-management` (the
`topology_auto_changed` event). Gates recorded before the live-script fixes:
`scripts/ci-local.sh --deep --ci-shape`, every step passed: unit 1708 passed,
0 failed (1 ignored); runtime Python 384 passed; core 1795 passed, 0 failed;
workspace 3833 passed, 0 failed (1 ignored). CPU and Fake-engine tests are not
qualification. The rented session below ran on two GPUs but did not establish
qualification. The complete live check remains required (ADR 0035 §14):

```bash
scripts/live/multi-gpu/run.sh --ssh <user>@<host>
```

MG1–MG14 and the bandwidth test; MG4 and MG5 decide whether vLLM's and SGLang's
`deep_park` is turned on at N > 1 (the first park at N > 1 also shows whether each GPU's
parked residue stays within its 2.25 GiB parked charge), and MG1–MG3, MG6–MG9 and MG13 must
pass before the release notes call the feature qualified.

## Interrupted qualification on two RTX A6000 GPUs, 2026-10-25

The session used two NVIDIA RTX A6000 48 GiB PCIe GPUs, driver 580.126.09,
vLLM 0.32.0, SGLang 0.5.22 and llama.cpp v0.7.0. Setup and measurement-script
fixes were committed through `7c54d1c8`; the engine binaries were not rebuilt
for each script-only change.

The remote runner reported `RUN_DONE` at 23:48:54 UTC, but the agent stopped
before copying the complete results or purging the machine. The rental was
automatically deleted at its spending cap on 2026-10-26 at 07:38:34 UTC.
No clean-home or empty-GPU check was obtained before deletion. The surviving
progress excerpts are not a substitute for the missing raw results, engine
logs and per-cycle evidence. Neither engine's multi-GPU parking qualification
flag is changed.

| Row | Surviving observation | Qualification consequence |
|---|---|---|
| BW | 61,085,172,367 bytes downloaded and verified in 562.1 s: 108.7 MB/s, 0.87 Gbit/s | Measurement retained |
| MG1 | vLLM reported all four parts passing; Ready in 91.8 s; 34,226 MiB used and 39,159 MiB charged on each GPU; 31 listeners, all loopback | Progress evidence retained; complete raw evidence missing |
| MG2 | SGLang reported all four parts passing; Ready in 186.3 s; 34,308 MiB used and 39,098 MiB charged on each GPU; 32 listeners, all loopback | Progress evidence retained; complete raw evidence missing |
| MG3 | llama.cpp answered the canary; used 6,460/6,098 MiB, charged 12,961 MiB per GPU; one loopback listener | The script accepted GPU readings instead of the required model-buffer log lines; that does not establish the documented fit criterion |
| MG4 | vLLM deep parking reported a pass; the truncated excerpt shows four cycles at 740 MiB parked and 34,238 MiB awake per GPU | The full five-cycle evidence was not recovered; keep qualification off |
| MG5 | SGLang deep parking failed to start | Cause unavailable without the missing engine log |
| MG6 | Both host-backed starts failed; the subsequent release check passed | Failed starts do not qualify host-backed parking |
| MG7 | Normal stops passed; both rank-kill checks failed. vLLM named rank 0 after rank 1 was killed; SGLang's excerpt had no rank-exit detail. Process disappearance and charge release were sampled at the same tick | Diagnose rank-exit reporting; same-tick samples alone cannot prove ordering |
| MG8 | Both engines reported `tp_stalled`, failed the instance and removed the engine processes | Progress evidence retained; full timing evidence missing |
| MG9 | SGLang reported 96%, 97% and 99% of direct throughput at C1/C4/C8. vLLM's direct launch rejected a CapyCTL-only argument | The argument-filter fix is committed; no successful vLLM comparison was recovered |
| MG10 | Switching failed at deployment or start | Cause unavailable without the missing CLI log |
| MG11 | Current-host check failed: start exit 0, no chat result. Older-host refusal reported a pass | Current-host path remains unqualified |
| MG12 | Both insufficient-GPU and pinned-count refusals passed | Refusal output retained |
| MG13 | Both automatic-topology checks and the wake check failed; the wake request returned 503 | Cause unavailable without the missing state and engine logs |
| MG14 | Status explained the device-memory refusal and what would fit; `start --wait` returned generic `capacity_blocked` instead | Preserve the specific refusal through the start path |

Local evidence (not committed) is under
`target/live/multi-gpu/2026-10-25-1719-multi-gpu/`: `bandwidth.txt`,
`preflight.txt`, `MG14-status-at-refusal.txt` and `recovered-progress.txt`.
The latter contains tool-output excerpts from multiple attempts, including
earlier failures; it is not a reconstructed `results.tsv` or a passing verdict.
The live-script commits also fix CUDA-wheel setup, CUDA discovery at engine
registration, charge and rank-identity readers, and direct-launch argument
reconstruction. Before another paid session, the runner must preserve evidence
incrementally and its completion must not depend on an agent remaining available.

### Rerun preparation, 2026-10-26

The owner authorized another qualification run and fixes for its findings.
`scripts/live/shade_session.py` runs the qualification command under an
independent user service, then requests deletion of the explicitly named
Shadeform instance on success, failure, interruption or a bounded deadline.
It records the command outcome and the provider's deletion receipt locally.
Provider-side automatic deletion remains a separate backup for loss of the
laptop or supervisor.

Local smoke runs exercised the actual supervisor with a simulated provider:
successful and failed commands, a deadline, and SIGTERM all ended with a
confirmed deletion receipt. A user-systemd-service smoke also passed, and
the user service authenticated to the real provider for a read-only inventory
query. These checks do not establish hardware qualification.

The runner now copies sanitized evidence during setup and after each row part,
retains each attempt separately, and refuses a passing verdict if any required
part or artifact is missing. A failed final copy prevents remote purge.
Rotated engine logs are included; MG3 requires the engine's actual per-GPU
model-buffer lines rather than substituting GPU readings. Twenty-two local checks,
including real-runner transport fixtures for success, row failure, missing
evidence, disconnection, failed final copy and purge recovery, passed. The full dry-run and
ShellCheck 0.11.0 also passed; these are not native-engine qualification.

Runtime corrections preserve the checkpoint's detailed memory refusal before
cold placement and report every recorded rank observed gone in one scan, without
inventing exit order or a signal for another process. Store, host-reporting,
protocol and CLI regression checks passed locally; MG7 and MG14 still require
the hardware rerun.

Review also caught an enrolled-server database path error and historical-result
selection that could reuse an earlier pass after an incomplete rerun. The
exporter now reads each role's actual database, treats a missing coordinator
database as failed collection, and requires completion and matching engine
fingerprints from the latest selected historical attempt. Purge recovery first
refuses an active runner, stops the remaining roles, and refreshes all exports
before copying and deleting anything. Regression cases failed before those
corrections and pass afterward. The integrated fast gate passed all eight steps.

The first supervised rental passed preflight but its collector timed out while
starting the detached setup command. A backgrounded shell AND-list retained
SSH's output pipe; a local process smoke reproduced that delay and confirmed
the corrected launch returns immediately. The supervisor deleted the rental
and retained setup logs and a provider receipt (reported cost $0.014).
No qualification row ran in that attempt.

The next rental passed MG3's managed startup, serving, accounting and listener
checks, but llama.cpp v0.7.0 suppressed the required allocation lines at default
verbosity. The owner chose a separate diagnostic replay: after the managed stop,
reuse the captured command and environment with only `--verbosity 4` added,
record its model buffers and stop it. The harness retains the diagnostic
separately; production logging is unchanged. A local process smoke proved capture,
managed-process exit, replay readiness and teardown; all 22 collector checks and
ShellCheck passed. Native diagnostic replay remains pending.

The supervised rerun retained startup logs and row evidence locally, including
vLLM's five successful deep-park cycles. SGLang's deep launch failed before
Python started: its CUDA 13 saver preload could not resolve the wheel's
`libcudart.so.13`. The runtime now derives the child loader path from CUDA runtime
wheel records within the selected installation. Fifteen runtime tests and a
native smoke with the installed saver in two fresh Python children passed;
full SGLang parking qualification remains pending.

MG6's unchanged 30B model requires about 94.1 GiB of host RAM for host-backed
parking. The harness now explicitly budgets 80% of host RAM for managed and
parked memory, retaining the 20% free reserve and normal admission checks.
MG13's two 4B comparison launches now declare 6 GiB of KV cache, enough for
their 32,768-token context, and failed prerequisites no longer lead to a wake
wait on a model that never started.

MG13 also exposed a product defect in the qualification build: a one-GPU
automatic choice dropped the measured tensor layout needed to reconstruct
larger candidates' host-backed charges. Exact snapshot validation then failed,
and the coordinator stopped. The automatic candidate table now retains that
layout. A focused regression failed before the fix and passed afterward in an
isolated source copy with the same parking switches as the rental build.
The shipped switches remain unchanged.

The interrupted rental's final collection succeeded, and the supervisor
confirmed deletion at a provider-reported cost of $1.95. This was not a
complete matrix pass. Its final verdict also rejected the qualification
role's journal: text redaction had inserted unescaped quotes into JSON-encoded
engine diagnostics. The collector now redacts decoded values before encoding,
including nested event payloads. A regression reproduced the JSON error before
the fix; all 23 collector checks pass afterward. An actual `snapshot` followed
by `sanitize-tree` smoke preserved parseable journal/event exports and removed
fixture credentials. The original evidence remains unchanged, so MG4 is not
qualified despite its five passing cycles. ShellCheck and the revised full
dry-run passed; CPU checks do not establish native-engine qualification.

The corrected source passed `scripts/ci-local.sh` and the complete
`scripts/ci-local.sh --deep --ci-shape` gate, including the core and full
workspace suites. Scoped reviews of the snapshot reconstruction, saver loader,
fixtures and structured redaction found no remaining actionable defects.
A further native run remains required. After a temporary capacity shortage,
the provider again offered a qualifying two-A6000 machine with 128 GB RAM.
The PR remains draft and the shipped multi-GPU parking switches remain off.

The next full run passed the llama.cpp allocation diagnostic and retained
parseable journals. It exposed a SGLang enrollment defect: 0.5.22 stores its
tensor-parallel rank in the process-local runtime context, not the Scheduler.
Enrollment now reads that context when the Scheduler has no rank field,
retaining the existing range check and refusal on unavailable evidence.
The rank-one regression failed before the fix; all 21 enrollment tests pass
after it. A smoke check against the installed SGLang context enrolled rank one
and served its authenticated observation socket. This is not yet proof of
native deep parking.

MG6 also exposed a harness race: process exit preceded controller cleanup,
so the next engine could start before the previous reservation was released.
The stop helper now waits for the stopped state and saves its status before
checking released charges or moving on. A delayed-cleanup shell smoke returned
too early before the change and waited for settlement afterward.
MG10 separately failed with a changing process-group observation at launch;
that failure remains unresolved and the ownership checks remain unchanged.

MG13 chose the expected GPU counts and evicted the smaller model, but its
command differed from MG2 only in the required memory-saver switch: MG2 was
`restart_only`, MG13 `deep`. The owner clarified on 2026-10-26 that the
comparison must use identical residency. MG13 now compares exactly against
MG5's explicit TP 2 deep command, also captured in an MG13-only run.
MG10 and MG13 additionally require an observed parked state before testing
a wake; a successful polling command alone does not establish that state.

The run on `bb74a235` reached every row and finished with a failing verdict.
Final collection succeeded, exported JSON remained valid, and the supervisor
confirmed rental deletion at a provider-reported cost of $2.67. MG1–MG4,
MG7–MG9, MG12, MG14 and bandwidth passed; MG5, MG6, MG10 and MG13 failed as
described above. MG11 passed remote serving and older-host refusal; that run
did not collect MG1's full accounting, listener and startup-peak evidence on
the enrolled host. The harness now reuses the same serving checks remotely,
with the coordinator's recorded host process identities and reservations.
The strict verdict requires every added part and artifact. Missing ownership
or observed listeners cannot pass. Startup evidence must include positive
TP2 host and representative per-GPU peaks with measured provenance, matching
ADR 0035 §3's largest-card measurement rule. All 27 collector regressions,
ShellCheck and the MG11-only dry-run passed; the peak-check command also
accepted the retained native measurements for all three engines.
The rank and transition corrections are committed as `41dc56a4`; the full
corrected native matrix remains required.

MG10's retained evidence does not distinguish engine child churn from a
host mount-table change. Observation now records which check changed; for
membership changes it records the compared kernel identities, never command
arguments or environment. Retry bounds and fail-closed ownership are
unchanged. All 13 observation tests passed. A real, bounded CPU process-group
smoke observed 19 stable snapshots and refused eight changing snapshots,
with the corresponding membership diagnostics captured and the child reaped.

The next run, on `0b2f72a0`, passed five deep-park cycles on each engine and
three SGLang host-backed cycles. vLLM's first host-backed park exposed a
deadline mismatch: its CPU copy took 31.327 seconds, but the generic HTTP
control timeout expired at 30 seconds despite the persisted operation's
longer deadline. Persisted sleep and weight-copy wake calls now use the
remaining operation deadline, as checkpoint reload already does. Expiry
still leaves the outcome uncertain, retains accounting and prevents replay.
The regression failed at 30 seconds before the change; all 26 HTTP and
residency tests passed afterward, including expiry without replay.

MG10 woke the two-GPU model, answered its request and stopped the competing
single-GPU model, but its switch event omitted the GPU names required by
the row. Request-driven and explicit-start plans now retain their selected
GPU sets for the planned-switch diagnostic. The qualification predicate
is unchanged. Both corrections still require a native rerun.

That run completed with MG6, MG10 and MG11 failing; every other row,
including automatic topology and wake, passed. Final evidence collection
succeeded, exported JSON remained valid, and the supervisor confirmed
rental deletion at a provider-reported cost of $2.96. MG11's enrolled host
served, passed per-GPU accounting and loopback checks, and recorded positive
TP2 startup peaks, but the collector rejected its host-qualified domain
names. The collector now recognizes the exact domain prefix belonging to
each measurement's host and requires host and GPU peaks from the same host.
The new regression failed before the correction; all 28 collector tests
passed afterward, including rejection of mismatched and mixed-host peaks.
The real `peaks-check` command accepted MG11's retained native measurements
afterward. The original failed verdict is unchanged.

The deadline and switch-diagnostic corrections (`f1772030`, `b167f741`)
passed the complete `scripts/ci-local.sh --deep --ci-shape` gate, including
the core and full-workspace suites. Scoped reviews found no actionable
defects. These CPU checks do not establish native-engine qualification.


# Release note: Several GPUs

- **One model across several GPUs of one machine.** A deployment with
  `topology: {tensor_parallel: 2}` (or `topology: auto`, which picks the smallest number of
  GPUs whose share of the model fits each card) runs on that many identical GPUs of one
  machine, with vLLM, SGLang or llama.cpp: one model, one route, each card charged its share,
  and the engine's traffic between the cards kept on loopback (`collectives: loopback` in
  status). `devices` can pin the cards. Refusals for memory or GPU count name the
  `tensor_parallel` that would fit. For now such a model stops instead of parking, and it
  runs without LMCache; this is not yet qualified on two-GPU hardware
  ([One model on several GPUs](../guide/deploy.md#one-model-on-several-gpus)).
