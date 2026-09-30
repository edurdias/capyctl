# Larger-model qualification on host-a

Started 2026-09-12. **4B and 14B passed zero-swap tests; 27B passed a bounded-swap diagnostic, not zero-swap qualification.**
Do not infer larger-model support from a downloaded checkpoint or a recognized
architecture. host-b is reserved by its owner and is not used in this run.

## Scope and fixed recipe

Qualify standalone startup, authenticated routed inference, three level-2
park/reload cycles, and stale-generation rejection for each checkpoint. Keep
BF16, 4096 context, 16 GiB KV, eager safetensors loading, and the existing
localhost-only development profile. Do not upgrade the installed runtime in
place. The initial runtime is vLLM 0.29.0, torch 2.13.0, Transformers 5.17.0.

| Checkpoint | Revision | Safetensors bytes | Status |
|---|---|---:|---|
| Qwen3-4B-Instruct-2507 | Existing qualified local checkpoint | Previously measured 7.49 GiB | Baseline passed |
| Qwen/Qwen3-14B | `40c069824f4251a91eefaf281ebe4c544efd3e18` | 29,536,665,640 | Checksum verified; standalone qualification passed |
| Qwen/Qwen3.8-27B | `1d4bf0f2ff6012fd82039f2fa52739d0dd7c60c0` | 55,563,006,776 | All 32 repository files checksum verified; bounded-swap diagnostic passed |

File sizes are from Hugging Face repository metadata, not runtime memory
measurements. The 27B config resolves as `Qwen3_5Config`, and the installed
vLLM registry includes `Qwen3_5ForConditionalGeneration`. Neither check proves
that startup, hybrid-state restoration, or multimodal inference works.

## Measurement contract

The request is `Say 'live' and nothing else.`, `max_tokens: 32`, temperature
zero, and `chat_template_kwargs.enable_thinking: false`. The test requires
exact `live` after trimming whitespace, both initially and after each reload.
An unexpected answer is a qualification failure requiring inspection, not
proof by itself that weights are corrupt.

`LIVE-STARTUP` measures controller Start to Ready and to a fully read,
validated non-streaming routed response. The latter includes local router
setup. `LIVE-METRIC` measures each park, wake to Ready, and wake to validated
full response. None of these is TTFT. Process startup is not an NVMe-cold-cache
measurement: no cache-drop operation is performed.

This workload differs from the earlier 8-token test: it adds an initial
inference and explicit generation controls. Its 4B baseline is the comparison
point for larger models; it does not replace the measurements in
[the eager-loader optimization report](deep-wake-optimization.md).

## Completed measurements

| Checkpoint | Startup response (s) | Wake response samples (s) | Wake median (s) | Minimum available (GiB) | Swap |
|---|---:|---|---:|---:|---|
| 4B | 27.583 | 7.976, 8.585, 8.544 | 8.544 | 77.60 | 0 |
| 14B | 73.256 | 25.600, 25.840, 26.202 | 25.840 | 55.78 | 0 |
| 27B (diagnostic) | 234.911 | 75.449, 82.760, 88.336 | 82.760 | 29.97 | 36 KiB peak |

27B's row uses the user-approved diagnostic swap allowance below; it is not
equivalent to the zero-swap rows. Compilation/cache state also differs between
first startups, so these are observed process-start costs, not isolated model-size scaling.

4B startup Ready: 27.485s. Engine reload samples: 6.94, 7.56, 7.51s.
Engine-reported park release: 24.54, 25.43, 25.40 GiB. All four routed answers
were `live`, and stale-generation dispatch was rejected. After cleanup,
MemAvailable was 126,263,517,184 bytes and swap remained zero.

14B startup Ready: 72.990s. Engine reload samples: 23.68, 23.95, 24.33s.
Engine-reported park release: 44.33, 46.41, 46.38 GiB. All four routed answers
were `live`, and stale-generation dispatch was rejected. After cleanup no vLLM
process remained; MemAvailable was 126,105,686,016 bytes with zero swap.
The pinned checkpoint verification checked all 18 repository files successfully;
extra local download-cache files were reported separately. Reload logs include
`RotaryEmbedding: Failed to load weights` warnings despite passing this smoke
test. This small exact-answer workload does not prove broad output equivalence.

Host memory is sampled every 250ms; available memory includes reclaimable
cache. Peak host use is MemTotal minus minimum MemAvailable, not a precise
allocation high-water mark. Downloads are paused or finished during timing.

### 27B first attempt: guard abort, not a latency result

After the user rebooted host-a, the resumed download completed and pinned
verification checked all 32 repository files successfully. Extra local cache
files remained; no checkpoint files were missing. Before inference, GPU
utilization was 0%, power 4 W, available memory 126,964,813,824 bytes, swap zero.

The unchanged harness aborted at elapsed 137.748s when swap usage reached
307,200 bytes (300 KiB). Minimum available memory was 48,364,113,920 bytes
(45.04 GiB); the last sample had only 1,023,778,816 bytes completely free and
47,974,793,216 bytes cached. Model compilation and initial profiling completed,
but there was no initial routed response and no park/wake cycle. No 27B startup
or wake latency is qualified, and this abort does not establish an OOM or a
model-fit failure. The cause of the paging itself is not established.

Cleanup removed all visible model processes. The GPU returned to 0%, 4 W;
the earlier stuck 96% reading did not recur in this attempt. Available memory
returned above 118 GiB, but 300 KiB remained used in `/swap.img` (also reported
as SwapCached). The zero-swap preflight therefore prevents another run until
the host's swap state is cleared. No guard was relaxed, runtime upgraded,
cache dropped, or swap configuration changed by the agent.

Second attempt after the user cleared swap also aborted: one 4 KiB page of
swap triggered the same guard at 65.361s during weight loading (15/18 shards).
Minimum available memory was 57,145,016,320 bytes (53.22 GiB). There was no
startup response or wake cycle. Cleanup again left no model process and GPU
0%, 4 W. Clearing residual swap alone therefore did not resolve the abort.
No safety threshold was changed. Second-attempt evidence:
`<temporary-directory>` on host-a, copied to
`.context/model-qualification/qwen3.8-27b-attempt2/`.

First-attempt raw evidence: `<temporary-directory>` on host-a; local copy
`.context/model-qualification/qwen3.8-27b-attempt1/`. Earlier host `/tmp`
artifacts may have been removed by the reboot; their local copies remain.

### 27B bounded-swap diagnostic

The user approved a diagnostic allowing up to 64 MiB total host swap, retaining
the 16 GiB minimum available memory, 96 GiB initial available memory, 1200s
timeout, host lock, port check, and owned-process cleanup. The original remote
qualification monitor remains unchanged; the diagnostic monitor is separate.
Local monitor mode defaults to qualification (zero swap); explicit diagnostic
mode selects 64 MiB and reports `zero_swap_qualification_passed: 0`. Six local
tests passed, including opt-in validation and swap/memory/timeout boundaries.

The diagnostic passed the exact initial response and all three restored
responses, plus stale-generation rejection. Startup Ready was 234.430s;
reloads were 72.68, 79.79, 85.37s; park release was 68.83, 70.92, 71.88 GiB.
Initial swap was 4 KiB; peak total swap was 36 KiB (32 KiB growth). Minimum
available memory was 32,183,037,952 bytes. Wakes increased across these three
samples; this small run does not establish a trend's cause or tail latency.

After cleanup the GPU process list was empty and available memory recovered
to 126,976,229,376 bytes, but the GPU utilization reading returned to 96% at
18 W. The earlier aborts returned to 0%, 4 W. This narrows the reproduction
window but does not identify whether inference, park/wake, or shutdown causes
the persistent reading. It remains unresolved and is not a clean-idle result.

Evidence: `<temporary-directory>` on host-a; local copy
`.context/model-qualification/qwen3.8-27b-diagnostic/`.

## Safety and reproducibility

The monitor holds a host qualification lock and refuses an occupied engine
port. Initial available memory must be at least 96 GiB with no swap in use.
It aborts below 16 GiB available, on swap use, or after 1200s overall.
These lab guards do not implement product admission accounting.

A review of the inherited monitor found incomplete failure cleanup. The
updated monitor records sampled worker PID/starttime identities, pins matching
identities with Linux pidfds, sends TERM, waits, escalates to KILL, and requires
exit before releasing the lock. It never discovers descendants from an already
reaped root PID. Four focused tests cover TERM resistance, surviving workers
after group-leader exit, mismatched identities, and reused-root rejection;
all passed locally and on host-a. This cleanup change followed the 4B baseline;
the latency definitions and request remain unchanged.

Local audit artifacts: `.context/model-qualification/` (gitignored).
4B raw host evidence: `<temporary-directory>`; local copy:
`.context/model-qualification/baseline-4b/`.
14B raw host evidence: `<temporary-directory>`; local copy:
`.context/model-qualification/qwen3-14b/`.
The tracked test is `crates/capyctl-cli/tests/live_spark.rs`, staged on host-a as
`live_model_qual.rs` to preserve its prior qualification tests.

## Switching boundary

### Follow-up: 4B concurrent post-wake correctness failure

An interactive 4B lab later exercised eight simultaneous distinct marker
requests through capyctl. While Ready, all eight returned correct answers (median
0.210s). After parking, the same eight requests shared exactly one reload and
all returned HTTP 200 (median 7.025s), but only one answer was correct; seven
returned identical unrelated text. Engine PID remained unchanged and the
deployment returned to Ready. A sequential repeat of an affected prompt also
failed, while a fresh prompt succeeded.

This failure limits the earlier single-prompt smoke-test evidence: successful
`live` responses do not establish general post-wake correctness. Concurrent
post-wake operation failed on the original code. Machine-local evidence is recorded in
`.context/model-qualification/concurrency-4b.md`; host logs are under
`<temporary-directory>` on host-a.

### Fix and routed concurrency retest

The controller published `Ready` **before** executing restoration. The first
request waited for the activation operation, but concurrent followers saw Ready
and forwarded directly to vLLM while its weights were being restored. Cold
start had the same state-publication ordering before readiness polling.

A cache reset alone was insufficient: the live retest admitted seven requests
during wake, then refused reset because 14 cache blocks were held. It produced
seven incorrect HTTP 200 responses and one HTTP 500. Direct-backend reproduction
and recovery after a manual cache reset established cached-state involvement;
they did not establish an independent upstream vLLM defect.

The controller now stays Starting during readiness polling and Waking during
restoration, publishing Ready only after that work succeeds. The adapter also
requires an acknowledged post-reload prefix-cache reset before clearing its
parked flag. No installed vLLM code was changed.

Same Qwen3-4B model, eight simultaneous distinct marker requests, max_tokens16,
temperature0, non-streaming, through the real capyctl router on host-a:

| Burst | Original correctness | Fixed correctness | Original median | Fixed median |
| --- | --- | --- | --- | --- |
| Already Ready | 8/8 | 8/8 | 0.210s | 0.213s |
| Park → automatic wake, cycle 1 | 1/8 | 8/8 | 7.025s | 8.266s |
| Park → automatic wake, cycle 2 | Not run | 8/8 | — | 8.274s |
| Park → automatic wake, cycle 3 | Not run | 8/8 | — | 8.467s |

All 32 fixed responses were HTTP 200, exact answers, and the expected model.
Each parked burst caused exactly one reload; engine PID100032 was unchanged;
each burst ended Ready. No manual wake or cache reset was used in this retest.
Weight reloads themselves took 7.21, 7.24, 7.40s versus 5.56s in the original
failed burst. These small samples do not isolate reset overhead or establish
a performance regression; the original faster responses were mostly incorrect.
Minimum available memory through the check was 83,933,364,224 bytes and peak
total swap 36KiB, within the explicitly allowed 64MiB diagnostic budget.
This is not a zero-swap qualification.

The gated controller regression failed before the fix and passed afterward
for both start and wake. Workspace tests and all-targets clippy passed. The
durable `live_concurrent_park_reload` test compiles; the live evidence above was
collected with the equivalent Python probe against the interactive lab.
Evidence: `.context/model-qualification/concurrency-after-fix.jsonl` and
`readyfix-engine.log`; host `<temporary-directory>`.

This validates the reproduced 4B non-streaming workload, not arbitrary
concurrency levels, 14B/27B with this fix, streaming, or administrative-start
joining. An inference request during an explicit administrative start can
still receive an activation error rather than join that separate operation.
The persistent idle GPU utilization anomaly remains unresolved.

Genuine 4B/27B parked switching is not yet qualified. The current role wiring
binds one checkpoint and adapter, and the existing A/B switch stops the old
engine. Per-deployment checkpoint/port bindings and real memory reservations
are prerequisites; see [F1 open items](../design/milestones/f1-open-items.md).
A direct two-vLLM-process demonstration must not be reported as a capyctl
SwitchEngine qualification. Image requests, cold-storage reloads, memory
pressure, and tail latency are also outside the completed evidence above.
