# F1 Merge — Open Items for Review (2026-09-12 night run)

**Merged:** `master` at `ef094c6` — F1 first vLLM path. 172 tests passing,
clippy `--all-targets -D warnings` clean. Simulator tier fully green; live
restart-only qualification PASSED on the DGX Spark.

## Open items (ordered by priority)

### 1. Park/reload + switching live evidence — RESUME WHEN SPARK RETURNS
The Spark (`user@host-b`) became unreachable (~04:00 EDT, ssh timeout,
suspected reboot/network change) mid-qualification. Resumable state:
- Everything is staged on the Spark (`~/mllm` built, `~/mllm-qual/live-run.sh`
  ready, vLLM 0.29.0 + Qwen3-4B checkpoint installed).
- **Before resuming, sync the merge**: `rsync -a --delete ... master → ~/mllm`
  — the fix wave (334ebf8) landed after the last sync, and it fixes two
  Critical defects (join lost-wake, per-member park state) that sit exactly on
  the pending stages.
- Stages: (a) restart-only switch A→B→A via the switch engine; (b) park/wake
  ×3 under the opt-in with `--enable-sleep-mode`; (c) ambiguous-park failure
  injection live; (d) T21 default-denial live check. Owner decision binding:
  park/reload is core functionality — recipe revisions continue until clean.
- Runbook: `docs/runbooks/spark-qualification-f1.md` §4 has the pending stages.

### 2. Park-based switching needs per-deployment ports (F4)
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

- 172 tests passing (`cargo test --workspace`), clippy `--all-targets -D
  warnings` clean, Cargo.lock tracked.
- Final whole-branch review: 2 Critical + 6 Important findings — ALL
  ADDRESSED in `334ebf8` with regression tests targeting the exact failure
  interleavings (join lost-wake, per-member park, double generation bump,
  orphan termination on timeout, stream-safe timeouts, router-tier wake join).
- Live-tier claims live ONLY in `docs/runbooks/spark-qualification-f1.md`.