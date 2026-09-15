//! Conformance suite for engine adapters and launchers.
//!
//! [`run_conformance`] exercises the core behavioral contracts from the
//! design (readiness gating, the deep-park policy gate, cancellation
//! uncertainty, and handle ownership) against ANY [`EngineAdapter`] /
//! [`Launcher`] pair. The F1/F2 real adapters run this suite in addition to
//! their own tests; the fake engine in `mllm-adapters::fake` is the
//! reference implementation these checks were developed against.

pub mod f2_pressure;
pub mod f2_monitor;
pub mod f2_metrics;
pub mod f2_correctness;
pub mod f2_timing;

use mllm_adapters::{
    AdapterError, EngineAdapter, HandleStatus, Launcher, MemberRef, OwnedHandle, ParkLevel, Phase,
    Readiness, RenderedCommand, RequestRef,
};
use std::time::Duration;

/// Severity of a single conformance check outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    /// The adapter demonstrably conforms to the invariant.
    Pass,
    /// The invariant could not be verified (e.g. the engine is briefly
    /// unreachable) — not a conformance failure, but recorded for triage.
    Warn,
    /// The adapter demonstrably violates the invariant.
    Fail,
}

/// The outcome of a single conformance check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    /// Stable identifier for the check (used by CI to filter/track).
    pub name: &'static str,
    /// Pass / warn / fail — see [`CheckStatus`].
    pub status: CheckStatus,
    /// Human-readable explanation, always populated.
    pub detail: String,
}

impl CheckResult {
    /// True only when the check demonstrably passed (warns are not passes).
    pub fn passed(&self) -> bool {
        self.status == CheckStatus::Pass
    }
}

/// Benign probe command used by launcher checks. Fake launchers ignore the
/// command; real launchers under conformance must be able to spawn it.
fn probe_command() -> RenderedCommand {
    RenderedCommand { argv: vec!["true".into()], env: Default::default() }
}

/// Runs the core conformance checks over any adapter + launcher pair.
///
/// Note: this is `async` (rather than a plain `fn`) because every trait
/// method is async; a synchronous entry point would need to build its own
/// runtime and would panic when called from inside one (which is exactly how
/// F1/F2 run it, from async tests). Callers: `run_conformance(&adapter,
/// &launcher).await`.
///
/// The suite never mutates host state beyond what the adapter itself does,
/// and never asserts engine-specific behavior — only the contract invariants.
/// Results are three-state: only demonstrable contract violations FAIL;
/// unverifiable invariants (e.g. `Err(Uncertain)` from a briefly unreachable
/// engine) WARN so triage can distinguish "broken" from "unknown":
///
/// 1. `readiness_gating` — a Ready claim must be corroborated by observed
///    engine state; liveness (`Initializing`) is never readiness. An
///    `Err(Uncertain)` probe warns (engine may be briefly reachable-later),
///    other probe errors fail.
/// 2. `park_policy_gate` — level-1 park is never behind the experimental
///    policy gate; level-2 park is either gated (`PolicyDenied`), explicitly
///    allowed, or reconcilably uncertain — but never reports an unrelated
///    non-gate error for a permitted request.
/// 3. `cancellation_uncertainty` — cancellation without `require_ack` must
///    never report `Acknowledged`; uncertainty or a real error only.
/// 4. `handle_ownership` — the launcher must never claim `Valid` for a
///    handle it did not spawn or one that was terminated (PID-reuse
///    detection, design §8): spawn → terminate → respawn must leave the old
///    handle `StaleReused` (reuse) or `Gone` (fresh pid), never `Valid`.
///
/// How the adapter's deep-park policy gate is scoped (F1 design §7).
///
/// `Level2Only`: only the experimental level-2 park is behind the gate
/// (the F0 fake's semantics; level-1 restart-level park is ungated).
/// `ProfileGated`: the opt-in gates the whole profile — both sleep levels
/// require it (real vLLM adapters: development mode is on from launch).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParkGateMode {
    Level2Only,
    ProfileGated,
}

pub async fn run_conformance(
    adapter: &dyn EngineAdapter,
    launcher: &dyn Launcher,
    gate_mode: ParkGateMode,
) -> Vec<CheckResult> {
    let member = MemberRef { deployment_id: "conformance".into(), member_id: "probe".into() };
    let req = RequestRef { id: "conformance-probe".into() };

    vec![
        check_readiness_gating(adapter, &member).await,
        check_park_policy_gate(adapter, &member, gate_mode).await,
        check_cancellation_uncertainty(adapter, &member, &req).await,
        check_handle_ownership(launcher),
    ]
}

/// Readiness probe interval between the two gating probes.
const READINESS_REPROBE_DELAY: Duration = Duration::from_millis(20);

/// Checks that readiness claims are gated by actual liveness.
///
/// Failure modes detected:
/// - The adapter claims [`Readiness::Ready`] while its own [`EngineAdapter::inspect`]
///   reports a non-Ready phase — a success claim contradicting observed state
///   (fabricated readiness).
/// - The readiness probe fails with something other than
///   [`AdapterError::Uncertain`] (readiness must be reportable; only
///   uncertainty is a legitimate non-answer).
///
/// `Err(Uncertain)` warns instead of failing: a real adapter whose engine is
/// briefly unreachable legitimately returns uncertainty, which the caller
/// must reconcile — that is contract-conformant behavior, not a violation.
async fn check_readiness_gating(adapter: &dyn EngineAdapter, member: &MemberRef) -> CheckResult {
    let fail = |detail: String| CheckResult {
        name: "readiness_gating",
        status: CheckStatus::Fail,
        detail,
    };
    let warn = |detail: String| CheckResult {
        name: "readiness_gating",
        status: CheckStatus::Warn,
        detail,
    };

    match adapter.check_readiness(member).await {
        // Liveness correctly reported as not-ready. Probe again to observe
        // the transition (or its absence — a still-initializing engine is
        // conformant; liveness must not be promoted to readiness).
        Ok(Readiness::Initializing) => {
            tokio::time::sleep(READINESS_REPROBE_DELAY).await;
            match adapter.check_readiness(member).await {
                Ok(Readiness::Ready) => CheckResult {
                    name: "readiness_gating",
                    status: CheckStatus::Pass,
                    detail: "observed Initializing -> Ready transition".into(),
                },
                Ok(Readiness::Initializing) => CheckResult {
                    name: "readiness_gating",
                    status: CheckStatus::Pass,
                    detail: "Initializing on both probes; no premature Ready claim".into(),
                },
                Err(AdapterError::Uncertain(d)) => warn(format!(
                    "observed Initializing; second probe uncertain ({d}) — transition unverifiable"
                )),
                Err(e) => warn(format!(
                    "observed Initializing; second probe errored ({e:?}) — transition unverifiable"
                )),
            }
        }
        // A Ready claim must be corroborated by the adapter's own state
        // observation, otherwise it may be fabricated (ready without ever
        // having been live).
        Ok(Readiness::Ready) => match adapter.inspect(member).await {
            Ok(state) if state.phase == Phase::Ready => CheckResult {
                name: "readiness_gating",
                status: CheckStatus::Pass,
                detail: "Ready claim corroborated by observed engine phase".into(),
            },
            Ok(state) => fail(format!(
                "claimed Ready but inspect reports phase {:?} — fabricated readiness",
                state.phase
            )),
            Err(e) => warn(format!(
                "claimed Ready; engine state unverifiable ({e:?}) — cannot corroborate"
            )),
        },
        Err(AdapterError::Uncertain(d)) => warn(format!(
            "readiness uncertain ({d}) — engine may be briefly unreachable; not a conformance failure"
        )),
        Err(e) => fail(format!("readiness probe failed definitively: {e:?}")),
    }
}

async fn check_park_policy_gate(
    adapter: &dyn EngineAdapter,
    member: &MemberRef,
    mode: ParkGateMode,
) -> CheckResult {
    // Gate-mode invariant: under `Level2Only` the level-1 park must never be
    // gated (restart-level park is not experimental); under `ProfileGated`
    // the adapter gates the whole profile, so a level-1 `PolicyDenied` under
    // a denied policy is the CORRECT outcome (F1 design §7: the opt-in
    // gates the profile, not just the operations).
    let l1 = adapter.park(member, ParkLevel::One).await;
    let l1_ok = match mode {
        ParkGateMode::Level2Only => !matches!(l1, Err(AdapterError::PolicyDenied)),
        ParkGateMode::ProfileGated => matches!(
            l1,
            Ok(_)
                | Err(AdapterError::PolicyDenied)
                | Err(AdapterError::Uncertain(_))
                | Err(AdapterError::UnsupportedCapability)
        ),
    };

    // Invariant 2: level-2 park is either gated, opted-in, uncertain, or
    // unsupported — but never an unqualified success on a *denied* policy
    // path the caller cannot distinguish. Any of these outcomes conforms:
    // a real gate (PolicyDenied), an opted-in host (Ok), an ambiguous
    // outcome (Uncertain), or a genuinely unsupported capability.
    let l2 = adapter.park(member, ParkLevel::Two).await;
    let l2_ok = matches!(
        l2,
        Ok(_)
            | Err(AdapterError::PolicyDenied)
            | Err(AdapterError::Uncertain(_))
            | Err(AdapterError::UnsupportedCapability)
    );

    CheckResult {
        name: "park_policy_gate",
        status: if l1_ok && l2_ok { CheckStatus::Pass } else { CheckStatus::Fail },
        detail: format!("level-1: {l1:?}; level-2: {l2:?}"),
    }
}

async fn check_cancellation_uncertainty(
    adapter: &dyn EngineAdapter,
    member: &MemberRef,
    req: &RequestRef,
) -> CheckResult {
    // The core "no false success" contract: without requiring an ack, the
    // adapter must not claim the cancellation was acknowledged.
    match adapter.cancel_work(member, req, false).await {
        Ok(mllm_adapters::CancellationOutcome::Acknowledged) => CheckResult {
            name: "cancellation_uncertainty",
            status: CheckStatus::Fail,
            detail: "cancel without ack reported Acknowledged — fabricated success".into(),
        },
        other => CheckResult {
            name: "cancellation_uncertainty",
            status: CheckStatus::Pass,
            detail: format!("cancel without ack: {other:?}"),
        },
    }
}

/// Handle ownership, including PID-reuse detection: spawn, terminate, respawn,
/// then verify the old handle can never come back as `Valid` — whether the
/// launcher reuses the PID (must be `StaleReused`) or mints fresh ones
/// (`Gone`). A launcher that reports `Valid` for a terminated handle is the
/// PID-reuse hazard the `start_identity` field exists to prevent (design §8).
fn check_handle_ownership(launcher: &dyn Launcher) -> CheckResult {
    // Invariant 1: a handle to a process this launcher never spawned must
    // not verify as Valid.
    let stranger = OwnedHandle { pid: u32::MAX, start_identity: 0 };
    if matches!(launcher.verify_handle(&stranger), HandleStatus::Valid) {
        return CheckResult {
            name: "handle_ownership",
            status: CheckStatus::Fail,
            detail: "launcher claimed ownership of a never-spawned handle".into(),
        };
    }

    // Invariant 2: after terminate + respawn, the old handle must not be Valid.
    let cmd = probe_command();
    let h1 = match launcher.spawn(&cmd) {
        Ok(h) => h,
        Err(e) => {
            return CheckResult {
                name: "handle_ownership",
                status: CheckStatus::Warn,
                detail: format!(
                    "stranger-handle check passed; could not exercise respawn path (spawn failed: {e:?})"
                ),
            };
        }
    };
    if let Err(e) = launcher.terminate(&h1, Duration::from_secs(1)) {
        return CheckResult {
            name: "handle_ownership",
            status: CheckStatus::Warn,
            detail: format!(
                "stranger-handle check passed; could not exercise respawn path (terminate failed: {e:?})"
            ),
        };
    }
    let h2 = match launcher.spawn(&cmd) {
        Ok(h) => h,
        Err(e) => {
            return CheckResult {
                name: "handle_ownership",
                status: CheckStatus::Warn,
                detail: format!(
                    "stranger-handle check passed; could not exercise respawn path (respawn failed: {e:?})"
                ),
            };
        }
    };

    match launcher.verify_handle(&h1) {
        HandleStatus::Valid => CheckResult {
            name: "handle_ownership",
            status: CheckStatus::Fail,
            detail: format!(
                "handle verified Valid after terminate + respawn (pid {}) — PID reuse undetected",
                h1.pid
            ),
        },
        reused @ (HandleStatus::StaleReused | HandleStatus::Gone) => {
            if launcher.verify_handle(&h2) == HandleStatus::Valid {
                CheckResult {
                    name: "handle_ownership",
                    status: CheckStatus::Pass,
                    detail: format!(
                        "old handle {reused:?} after terminate + respawn; new handle Valid"
                    ),
                }
            } else {
                CheckResult {
                    name: "handle_ownership",
                    status: CheckStatus::Fail,
                    detail: "freshly spawned handle does not verify Valid".into(),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mllm_adapters::fake::{FakeEngine, FakeLauncher};
    use mllm_adapters::{
        CancellationOutcome, EngineState, ExitReport, LauncherError, ParkOutcome, Quiescence,
        ReloadOutcome, RestoreOutcome, WorkObservation,
    };

    #[tokio::test]
    async fn fake_engine_passes_full_conformance_suite() {
        let adapter = FakeEngine::new();
        let launcher = FakeLauncher::new();
        let results = run_conformance(&adapter, &launcher, ParkGateMode::Level2Only).await;
        assert_eq!(results.len(), 4);
        for r in &results {
            assert!(r.passed(), "check {} did not pass: {}", r.name, r.detail);
        }
    }

    #[tokio::test]
    async fn pid_reuse_launcher_exercises_reuse_detection() {
        let adapter = FakeEngine::new();
        let launcher = FakeLauncher::new().with_pid_reuse();
        let results = run_conformance(&adapter, &launcher, ParkGateMode::Level2Only).await;
        let ownership = results.iter().find(|r| r.name == "handle_ownership").unwrap();
        assert!(ownership.passed(), "{}", ownership.detail);
        assert!(ownership.detail.contains("StaleReused"), "{}", ownership.detail);
    }

    /// An adapter that fabricates readiness: always claims Ready while its
    /// own inspect reports the engine is still starting up.
    struct FabricatedReadyAdapter;

    #[async_trait::async_trait]
    impl EngineAdapter for FabricatedReadyAdapter {
        async fn inspect(&self, _: &MemberRef) -> Result<EngineState, AdapterError> {
            Ok(EngineState { phase: Phase::Startup, retained_bytes: 0, build_fingerprint: None })
        }
        async fn render_plan(
            &self,
            _: &mllm_adapters::PlanInput,
        ) -> Result<mllm_adapters::RenderedCommand, AdapterError> {
            Err(AdapterError::UnsupportedCapability)
        }
        async fn check_readiness(&self, _: &MemberRef) -> Result<Readiness, AdapterError> {
            Ok(Readiness::Ready)
        }
        async fn prepare_park(&self, _: &MemberRef) -> Result<Quiescence, AdapterError> {
            Ok(Quiescence { quiescent: true })
        }
        async fn park(&self, _: &MemberRef, _: ParkLevel) -> Result<ParkOutcome, AdapterError> {
            Err(AdapterError::PolicyDenied)
        }
        async fn restore(&self, _: &MemberRef) -> Result<RestoreOutcome, AdapterError> {
            Ok(RestoreOutcome::Restored)
        }
        async fn reload_weights(&self, _: &MemberRef) -> Result<ReloadOutcome, AdapterError> {
            Ok(ReloadOutcome::Reloaded)
        }
        async fn observe_work(&self, _: &MemberRef) -> Result<WorkObservation, AdapterError> {
            Ok(WorkObservation::Idle)
        }
        async fn cancel_work(
            &self,
            _: &MemberRef,
            _: &RequestRef,
            _: bool,
        ) -> Result<CancellationOutcome, AdapterError> {
            Ok(CancellationOutcome::Uncertain)
        }
    }

    #[tokio::test]
    async fn fabricated_ready_adapter_fails_readiness_gating() {
        let adapter = FabricatedReadyAdapter;
        let launcher = FakeLauncher::new();
        let results = run_conformance(&adapter, &launcher, ParkGateMode::Level2Only).await;
        let readiness = results.iter().find(|r| r.name == "readiness_gating").unwrap();
        assert_eq!(readiness.status, CheckStatus::Fail);
        assert!(readiness.detail.contains("fabricated"), "{}", readiness.detail);
    }

    /// A real adapter whose engine is briefly unreachable: readiness probes
    /// return Uncertain. Conformant behavior — warn, don't fail.
    struct UnreachableAdapter;

    #[async_trait::async_trait]
    impl EngineAdapter for UnreachableAdapter {
        async fn inspect(&self, _: &MemberRef) -> Result<EngineState, AdapterError> {
            Err(AdapterError::Uncertain("engine unreachable".into()))
        }
        async fn render_plan(
            &self,
            _: &mllm_adapters::PlanInput,
        ) -> Result<mllm_adapters::RenderedCommand, AdapterError> {
            Err(AdapterError::UnsupportedCapability)
        }
        async fn check_readiness(&self, _: &MemberRef) -> Result<Readiness, AdapterError> {
            Err(AdapterError::Uncertain("engine unreachable".into()))
        }
        async fn prepare_park(&self, _: &MemberRef) -> Result<Quiescence, AdapterError> {
            Err(AdapterError::Uncertain("engine unreachable".into()))
        }
        async fn park(&self, _: &MemberRef, _: ParkLevel) -> Result<ParkOutcome, AdapterError> {
            Err(AdapterError::Uncertain("engine unreachable".into()))
        }
        async fn restore(&self, _: &MemberRef) -> Result<RestoreOutcome, AdapterError> {
            Err(AdapterError::Uncertain("engine unreachable".into()))
        }
        async fn reload_weights(&self, _: &MemberRef) -> Result<ReloadOutcome, AdapterError> {
            Err(AdapterError::Uncertain("engine unreachable".into()))
        }
        async fn observe_work(&self, _: &MemberRef) -> Result<WorkObservation, AdapterError> {
            Err(AdapterError::Uncertain("engine unreachable".into()))
        }
        async fn cancel_work(
            &self,
            _: &MemberRef,
            _: &RequestRef,
            _: bool,
        ) -> Result<CancellationOutcome, AdapterError> {
            Ok(CancellationOutcome::Uncertain)
        }
    }

    #[tokio::test]
    async fn uncertain_readiness_warns_not_fails() {
        let adapter = UnreachableAdapter;
        let launcher = FakeLauncher::new();
        let results = run_conformance(&adapter, &launcher, ParkGateMode::Level2Only).await;
        let readiness = results.iter().find(|r| r.name == "readiness_gating").unwrap();
        assert_eq!(readiness.status, CheckStatus::Warn);
        assert!(!readiness.passed());
    }

    /// A launcher that hands out the same handle for every spawn and always
    /// reports Valid — the PID-reuse hazard itself.
    struct ReuseObliviousLauncher;

    impl Launcher for ReuseObliviousLauncher {
        fn spawn(&self, _: &RenderedCommand) -> Result<OwnedHandle, LauncherError> {
            Ok(OwnedHandle { pid: 7, start_identity: 1 })
        }
        fn terminate(&self, _: &OwnedHandle, _: Duration) -> Result<ExitReport, LauncherError> {
            Ok(ExitReport { pid: 7, exit_code: Some(0), signal: None, killed: true })
        }
        fn verify_handle(&self, h: &OwnedHandle) -> HandleStatus {
            if h.pid == 7 {
                HandleStatus::Valid
            } else {
                HandleStatus::Gone
            }
        }
    }

    #[tokio::test]
    async fn reuse_oblivious_launcher_fails_handle_ownership() {
        let launcher = ReuseObliviousLauncher;
        let result = check_handle_ownership(&launcher);
        assert_eq!(result.status, CheckStatus::Fail);
        assert!(result.detail.contains("PID reuse undetected"), "{}", result.detail);
    }
}
