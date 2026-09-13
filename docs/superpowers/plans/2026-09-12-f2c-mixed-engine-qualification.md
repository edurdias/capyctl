# F2C Mixed-Engine Qualification Implementation Plan

**Goal:** Produce repeatable numerical evidence for Q1–Q11, including concurrent vLLM/SGLang serving and retained-runtime warm switching on host-a.

**Architecture:** A bounded local runner uses only the product management and inference APIs. Separate preflight, measurement, fault injection, and report generation. Fail closed on unsafe pressure or uncertain ownership; preserve evidence rather than retrying lifecycle effects.

**Tech Stack:** Rust/Tokio/reqwest test harness, local fake servers for deterministic tests, protected JSONL metadata artifacts, `/proc` and NVIDIA observations during separately authorized live runs.

**Spec:** [F2 design](../../design/milestones/f2-sglang-design.md), §8 Q1–Q11; all A1/A2/A3/B plans in this directory.

F2C is the sole live executor and evidence owner. B documents adapter readiness
and references these results; it does not repeat single-engine or mixed-engine runs.

## Global Constraints

- “Start live testing with small models for repeatability.”
- “Use conservative constrained managed budgets to exercise pressure without deliberately exhausting physical memory.”
- “Larger model recipes are qualified separately.”
- “HTTP success alone is insufficient.”
- “No wake-time or throughput improvement is guaranteed.”
- Only host-a. Never access host-b. No reboot, driver change, broad cleanup, or engine installation on inferred permission.
- Preserve the unrelated CLI live-interactive test. No checkpoint deletion or new large model download.

---

## 1. Fixed recipes and guardrails

Begin with two deployments of the existing Qwen3-4B-Instruct-2507 checkpoint:
one using the F1 vLLM 0.29.0 recipe, one the F2B SGLang v0.5.16 recipe. Fingerprint
local checkpoint files and installed software before use. Do not equate the same
checkpoint with the same tokenizer/template, memory footprint, or generation output.
Use distinct route names and private endpoints. Keep 14B/27B outside this exit run;
earlier smoke results are not mixed-engine qualification.

Guardrails are fixed before launching:

| Control | Value / rule |
|---|---|
| Physical domain | One unified system-memory domain; measured usable MemTotal |
| Protected available memory | `max(16 GiB, ceil(MemTotal/5))` |
| Managed ceiling | `min(96 GiB, MemTotal - protected headroom)` |
| Observation period / maximum age | 250 ms / 2 s |
| Swap baseline | Stable for 30 s before start; record existing use, never assume zero |
| Abort on swap growth | 64 MiB above baseline, or sustained growth in three consecutive samples |
| Abort on pressure | Any available-memory sample below protected headroom; stop new admissions immediately |
| Initial single-engine qualification reservation | 48 GiB for every phase until a smaller phase bound is qualified |
| Peak-bound safety margin | `max(2 GiB, ceil(measured attributable peak/4))`, added to attributable peak |
| Single-engine cold deadline | 600 s |
| Warm operation deadline | 300 s |
| Drain deadline | 60 s; no implicit kill on expiry |
| Inference response deadline | 120 s, including queue/activation |
| Test request settings | temperature 0, max_tokens 32; context ≤4096; no unbounded prompts |
| Backend concurrency | At most 8 per deployment, 16 total during coexistence |
| Warm repetitions | 10 complete A→B→A cycles after preparation |
| Restart/fault repetitions | 3 per selected live case; deterministic races 100 iterations |
| Evidence bounds | 100 MiB total metadata per run; no prompt/secret logging; abort recording cleanly at bound |

The initial 48-GiB bound is a conservative *qualification grant*, not a proven
footprint. Check checkpoint weights, explicit KV bound, fixed execution settings,
and runtime loading strategy support this bound before launch. If they cannot,
record `estimate_unbounded`; do not discover an unknown peak by exhausting memory.
Measure one engine at a time under its bound and independent host pressure checks.

Do not subtract global free-memory changes and call the result per-owner release.
Combine qualified allocator/worker observations, complete process identities,
explicit retained allocations, and conservative overhead. If attribution cannot
justify a smaller phase bound, keep 48 GiB; do not manufacture a pressure scenario
by shrinking the parked reservation. A recipe is qualified only when its bounds
cover every measured repetition plus the fixed margin and all external headroom
checks remain satisfied. An observed overshoot invalidates the recipe.

Pressure-case ceiling is selected from measured qualified footprints, not guessed:
choose the smallest whole GiB that admits every intended park-before-wake step,
but denies the direct wake with the victim Ready. It must not exceed the safe
managed ceiling. If no such interval exists, this pair cannot demonstrate Q6;
record the missing gate and choose a separately reviewed recipe, not unsafe limits.

On a guardrail breach: close new admission, preserve peak/uncertain accounting,
stop further test steps, and collect bounded diagnostics. Terminate only owned
test runtimes under the run's explicit abort-cleanup authority and verified
identity checks. Never kill unrelated processes or clear reservations to proceed.
Warm-test failures and emergency cleanup are reported failures, not successful switches.

## 2. Request correctness and timing contract

Use fixed public test prompts requesting exact distinct marker strings, with no
private data. Baseline each engine independently; a model failing its deterministic
marker corpus does not supply valid routing/cache evidence. Also verify the public
route, backend binding, checkpoint/recipe, and operation identity; textual equality
alone cannot distinguish two deployments using the same checkpoint.

Each post-wake wave includes repeated-prefix prompts and fresh marker prompts.
Validate exact normalized marker content, no other request's marker, valid finish
reason, and proper terminal protocol. Streams must reconstruct the same expected
content with no lost/reordered chunks. A 200 with malformed/truncated content fails.

Timing fields use monotonic timestamps within a process: request accepted, queue
enter/leave, activation start/end, backend dispatch, first token, backend terminal,
delivery end. Record cold initialization separately from warm allocation restore,
disk reload, cache reset, and model probe. Queue and activation intervals may overlap;
report them separately, not as an additive latency decomposition.

Report sample count, minimum, median, p95 (nearest rank), maximum, failures, and
timeouts for each case/engine/mode. No latency-improvement gate is imposed; compare
direct steady-state, routed steady-state, cold activation, and warm restoration
without conflating engine time, scheduler wait, and cache conditions.

## 3. Tasks

### Task 1: Build a dry-run-safe qualification runner

**Files:** Create `tests/harness/src/f2.rs`, `tests/harness/tests/f2_runner.rs`,
`tests/harness/src/bin/f2-qualify.rs`; modify harness `Cargo.toml`/`src/lib.rs`.
**Interfaces:** CLI requires explicit `--management-url`, `--inference-url`,
`--credential-file`, `--output-dir`, and `--host host-a` for live mode.
Default mode prints/checks the manifest and makes no engine-mutating requests.
`--execute` starts an approved run; `--allow-owned-abort-cleanup` explicitly grants
bounded cleanup of runtimes created by that run, never preexisting deployments.

- [ ] Add this guard test before the runner implementation:

```rust
pub fn live_host_allowed(host: &str) -> bool { host == "host-a" }
#[test]
fn live_runner_rejects_other_hosts() {
    assert!(live_host_allowed("host-a"));
    assert!(!live_host_allowed("host-b"));
    assert!(!live_host_allowed("host-a.example.invalid"));
}
```

- [ ] Run harness runner tests RED. Validate URL destinations independently of
  the label: trusted local inventory must establish host-a identity; a user-provided
  host string alone is not authorization. Refuse redirects and mismatched host
  identities. Unit tests use explicit fake mode with loopback servers.
- [ ] Persist a manifest before effects: source commit/dirty diff fingerprint,
  build/checkpoint/recipe/hardware identities, budgets, counts, deadlines, credential
  references only, and requested cases. Refuse overwriting an existing run directory.
  Record monotonic events and request outcome metadata with bounded buffering.
- [ ] Begin first qualification through A3 `POST /management/v1/qualification-runs`.
  Submit the reviewed digest, exact host revision, bounded manifest, conservative
  phase grants, deadlines, and explicit owned-abort permission. Candidate commands
  use only run-scoped actions/inference endpoints; ordinary routes remain closed.
  Reuse request keys after transport loss without replaying backend inference.
  No direct engine-control shortcut or manufactured Qualified record.
- [ ] Add dry-run zero-mutation, invalid-host, output-collision, secret-redaction,
  guardrail-abort, and missing-fingerprint tests. Run GREEN; commit
  `test: add bounded mixed-engine qualification runner`.

### Task 2: Automate deterministic contract and failure cases

**Files:** Harness `f2.rs`, `tests/f2_runner.rs`; existing store/controller/router
named integration tests from A2d/A3/B.

- [ ] Drive fake engines through product API/CLI with the same scenario runner
  used for live work. Test all Q1–Q10 management/control-plane assertions before
  GPU runs. Repeat independent-connection claim/dispatch races 100 times.
- [ ] Fault injection points: arm-before-send, applied-before-reply, reload false,
  flush false, worker disappearance, PID reuse, store commit failure, controller
  loss, client disconnect, full stream sink, expired observation, changed recipe.
  Assert gate/resource/endpoint/lease invariants after every injected fault.
- [ ] Add percentile helper with exact tests:

```rust
pub fn nearest_rank(sorted: &[u64], percentile: usize) -> Option<u64> {
    if sorted.is_empty() || percentile == 0 || percentile > 100 { return None; }
    let rank = sorted.len().checked_mul(percentile)?.checked_add(99)? / 100;
    sorted.get(rank - 1).copied()
}
#[test]
fn p95_uses_nearest_rank_without_interpolation() {
    let samples: Vec<u64> = (1..=20).collect();
    assert_eq!(nearest_rank(&samples, 95), Some(19));
    assert_eq!(nearest_rank(&[], 95), None);
}
```

- [ ] Run harness/controller/router/store named tests to GREEN. Exclude unrelated
  live-interactive target. Commit `test: cover mixed-engine failure invariants`.

### Task 3: Perform read-only live preflight and single-engine qualification

**Files:** Run manifest and `docs/runbooks/f2-mixed-engine-qualification.md`.

- [ ] Confirm host-a only, no conflicting user workload, stable pressure, enough
  filesystem space for bounded logs, private credentials, installed exact pins,
  checkpoint identity, memory saver, driver/runtime compatibility, and safe
  phase estimates. Missing software/authority becomes a morning question;
  do not install, reboot, or download automatically.
- [ ] For each engine independently: 1 cold initialization; 32 nonstreaming and
  32 streaming correctness requests; 5 park/wake cycles with the same request
  corpus after each wake. Measure every phase and identity. Qualify the phase
  bounds using §1, then explicitly stop/clean up that test runtime before testing
  the other engine. Fresh cold means new process, not necessarily cold disk cache.
  Use candidate authority for this first run, keeping its conservative grant
  through park/wake. Submit `finish` only after every required case; trusted
  collectors validate evidence before promotion. Failed/expired runs remain
  unqualified and retain uncertain reservations. Stop/cleanup before creating
  ordinary bindings from promoted evidence, using the run's explicit `cleanup`
  action and frozen owned-runtime authority.
- [ ] Verify auth rejection for inference versus admin credentials and unauthorized
  health-generation paths. Confirm status/snapshot never wakes a parked engine.
  Check saver-enabled evidence and retained API/worker identity, not just HTTP.
- [ ] If any correctness, release, cache, security, or pressure check fails, retain
  the failed report and stop promotion. No mixed-engine launch using unqualified
  residue. Do not count diagnostic retries as original passing samples.

### Task 4: Prove coexistence, preparation, warm pressure switching, and requests

**Files:** Same run manifest, runner scenario definitions, runbook report.

- [ ] Prepare A then park A; prepare B then park B. Assert one cold initialization
  each, distinct retained endpoint/credentials, both parked, and a feasible later
  wake for either. Test partial failure separately; it must preserve completed A.
- [ ] Wake both under a ceiling admitting coexistence. Run 4 waves of 8 requests
  per engine (64 total), half streaming. Confirm actual overlap and no unnecessary
  park/stop. Repeat with two recipes of the same engine to prove routing isolation.
- [ ] Set the reviewed constrained managed ceiling from §1; do not change physical
  headroom. With A ready, run 10 A→B→A cycles. Each of the 20 wakes gets 8
  nonstreaming and 8 streaming requests (320 total). Assert unchanged complete
  runtime identities, no additional cold initialization, A release commit before
  B increase, and exact post-wake correctness. Report every failure and timeout.
  Change and restore ceilings only through authenticated
  `PUT /management/v1/hosts/host-a/resource-policy`, with the current
  `expected_revision` and stable idempotency key. Record both policy revisions;
  concurrent policy drift fails restoration rather than overwriting owner changes.
- [ ] From one parked target, issue 32 simultaneous routed requests plus an
  administrative start. Assert one durable wake, bounded queue ownership, no
  stale dispatch, and all successful content correct. Repeat 3 times. Then run
  opposing demand to both routes for 60 s with fixed 2-s admission windows;
  report waits/denials and prove neither route can reset its window indefinitely.
  Existing streams may delay switching; do not turn a fairness claim into a kill.
- [ ] Exercise impossible warm arrangement with a managed ceiling below the
  minimum safe sequence. Assert structured blocking owners/budgets and zero
  cold-stop fallback. Restore configuration through revision-aware API.

### Task 5: Live recovery, product operations, and evidence closeout

**Files:** Runbook, qualification records, planning-index Q-gate status.

- [ ] Run 3 repetitions each of controlled lost release reply, lost resume reply,
  and local controller restart using only owned test deployments. The fault
  proxy drops acknowledgements without changing engine commands. Assert no
  replay, no false Ready/release, explicit uncertainty, and qualified reconciliation
  or authorized cleanup. PID reuse and storage failures remain deterministic
  tests unless safe live reproduction is independently available.
- [ ] Through CLI/API demonstrate submission retry, revision conflict, explicit
  stop preventing request wake, update after cleanup, attachment with no lifecycle
  adoption, snapshot/event reconnect, expired cursor resnapshot, and redaction.
- [ ] Report Q1–Q11 in a table with case IDs, counts, artifact references, pass/fail,
  and limitations. Record unsupported recipe combinations. Mark F2 complete only
  when required automated and selected live gates pass for both engines.
- [ ] Compare direct/routed steady-state and cold/warm distributions with sample
  counts; identify cache conditions and controller accounting overhead. No claim
  that warm is faster is required or inferred. Larger-model testing is a separate
  subsequent qualification proposal, not an unrecorded extension of this run.
- [ ] Run `git diff --check`; commit sanitized runbook/evidence metadata only.
  Do not commit secrets, checkpoint data, or bulk raw logs.

## 4. Completion boundary

Implementation can pass deterministic tests while live qualification is blocked
on engine availability or owner-only changes. Report those as separate states.
Never label a skipped live test, simulated release, or source inspection as Q11
hardware evidence. Keep F2 open until the required selected recipes pass.
