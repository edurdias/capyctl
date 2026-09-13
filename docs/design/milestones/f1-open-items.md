# F1 Merge — Open Items for Review (2026-09-12 night run)

## Current closeout (2026-09-12)

**F1 closed by owner direction for mainline integration.** F2 SGLang is the
active next milestone. Closure accepts the bounded F1 evidence below; it does
not erase the listed carryovers or broaden live qualification claims.

The merge record below is historical. Subsequent live 4B concurrency exposed a
controller bug: Ready was published before activation work completed. The
controller now retains Starting/Waking through readiness/restoration, and the
vLLM adapter requires a confirmed post-reload cache reset. The reproduced
workload passed 32/32 exact routed responses, including three eight-request
automatic wake bursts, with one reload each and an unchanged engine PID.
See [full comparison and limits](../../runbooks/spark-model-size-qualification.md#fix-and-routed-concurrency-retest).

This does not qualify arbitrary concurrency, streaming after this fix, or
14B/27B after this fix. The earlier 14B smoke run passed; 27B passed only under
an explicitly relaxed diagnostic swap budget. Idle GPU utilization reporting
remains unresolved. Inference during an explicit administrative start may
return an activation error because that operation is outside the router join.

Only host-a is currently authorized; the references to host-b below record
earlier runs, not current permission. The next formal milestone is **F2 SGLang**
per SPEC §18. Management/API wiring and real reservations below are carryovers
to scope explicitly, not a replacement for that milestone's exit gate.

The localhost interactive lab is temporary test tooling, not the management
API. Its Rust harness and private helper scripts remain machine-local and are
not included in the retained production-fix commit.

**Merged:** `master` at `ef094c6` — F1 first vLLM path. 172 tests passing,
clippy `--all-targets -D warnings` clean. Simulator tier fully green; live
restart-only qualification PASSED on the DGX Spark.

## Open items (ordered by priority)

### 1. Park/reload + switching live evidence — COMPLETE (2026-09-12)
All four pending stages passed on `host-a` / `host-b`: stock A→B→A,
three level-2 park/reload cycles with authenticated routed inference after
every reload, real lost-ack park injection, and T21 denial before engine spawn.
The recipe remains vLLM 0.29.0 + Qwen3-4B, with 16 GiB KV and 4096 context.
Commands, results, failed probes, timings, and machine-local evidence pointers
for those four stages are recorded in `docs/runbooks/spark-qualification-f1.md` §4.
Later model-size and concurrency evidence is in the separate runbook linked above.

### 2. Park-based switching needs per-deployment ports (rescheduled to F2)

Owner-approved F2 scope brings port allocation forward for retained parked processes
and concurrent mixed-engine serving. See [F2 design](f2-sglang-design.md). The F4
references in the historical record below describe the earlier scheduling decision.

The live A→B→A alternation uses stop-based release because a parked process
holds the engine port. F4's per-deployment port allocation unlocks
park-keep-alive switching (the real product mode). Noted in switch.rs comments.

### 3. Adapter residuals (F2 entry criteria)
- Per-deployment engine API key is NOT wired (`engine_api_key: None`,
  `api_key: None`) — plan listed it as a mitigation layer; loopback lab
  mitigates. Wire with the management surface.
- Engine log capture appends indefinitely (no rotation).
- args.rs odd-tail engine_args pass through unvalidated; "boolean flag
  followed by a value" shapes need care.
- Failed deployments have no Stop transition; live lost-ack qualification
  cleans up its own verified engine group explicitly. Automatic recovery from
  Failed remains unqualified.

### 4. Management surface (F2)
CLI `deploy`/`status`/lifecycle dispatch are `not_implemented` — the live
qualification drove the controller in-process (documented in the runbook).
F2's management API/CLI wiring covers it.

### 5. Reservations are conservative placeholders
The ledger charges a synthetic 4096-byte activation peak and never releases
rows (T32 semantics trivially satisfied). Real budget wiring against the
scheduler's `admit` is F2 work — the store schema and owner rows exist.

### 6. Scheduler admission order (F0 carryover)
Self-consistency checks (CategoryLimit/etc.) run before topology/freshness
checks — a stale host may surface exit 8 before exit 11. Defer to F1.5/F2
when real topology lands.

## Rulings made during F1 execution (user-visible decisions)

1. **Park depth from host policy** — deep park (level 2) under the opt-in;
   level 1 for restart-only hosts (level 1 frees nothing on unified memory).
2. **Admission check order** — resource-consistency checks before
   topology/freshness (plan tests are binding; deferred to F2).
3. **readiness_gating uses contradiction detection** (carried from F0).
4. **Switch release is stop-based at F1** — parked pools hold the port; park
   is validated by dedicated cycles, not mid-switch (per-deployment ports in
   F4 unlock park-switching).
5. **Unknown work observation drains with recorded uncertainty** (design §5
   drain liveness) — both in controller Step::Drain and the switch engine.
6. **Explicit `start` clears administrative suspension; router auto-activate
   never does** (T10 per SPEC §6.3 — the T10 test was updated to the SPEC
   reading).
7. **Gate boundary (owner-approved): the opt-in gates the vllm-sleep profile
   itself**, not just park/reload operations.
8. **Park/reload is core functionality** (owner decision): recipe revisions
   continue until validated — no downgrade, no deferral.

## Verification state

Latest qualification fix: `cargo test --workspace` reports 179 passed (five
live tests skip without the live environment); clippy is clean. The four named
live stages for item 1 ran independently on the Sparks; see the runbook.
Historical merge verification follows.

- 172 tests passing (`cargo test --workspace`), clippy `--all-targets -D
  warnings` clean, Cargo.lock tracked.
- Final whole-branch review: 2 Critical + 6 Important findings — ALL
  ADDRESSED in `334ebf8` with regression tests targeting the exact failure
  interleavings (join lost-wake, per-member park, double generation bump,
  orphan termination on timeout, stream-safe timeouts, router-tier wake join).
- Live-tier claims live ONLY in `docs/runbooks/spark-qualification-f1.md`.
