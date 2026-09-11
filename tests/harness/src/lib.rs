//! Conformance suite for engine adapters and launchers.
//!
//! [`run_conformance`] exercises the core behavioral contracts from the
//! design (readiness gating, the deep-park policy gate, cancellation
//! uncertainty, and handle ownership) against ANY [`EngineAdapter`] /
//! [`Launcher`] pair. The F1/F2 real adapters run this suite in addition to
//! their own tests; the fake engine in `mllm-adapters::fake` is the
//! reference implementation these checks were developed against.

use mllm_adapters::{
    EngineAdapter, Launcher, MemberRef, OwnedHandle, ParkLevel, Readiness, RequestRef,
};

/// The outcome of a single conformance check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    /// Stable identifier for the check (used by CI to filter/track).
    pub name: &'static str,
    /// True when the adapter conforms to the checked invariant.
    pub passed: bool,
    /// Human-readable explanation, always populated (pass detail or failure reason).
    pub detail: String,
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
/// and never asserts engine-specific behavior — only the contract invariants:
///
/// 1. `readiness_gating` — the adapter must be able to report a definitive
///    readiness state (`Initializing` or `Ready`), never a fabricated success
///    on an error path.
/// 2. `park_policy_gate` — level-1 park is never behind the experimental
///    policy gate; level-2 park is either gated (`PolicyDenied`), explicitly
///    allowed, or reconcilably uncertain — but never reports an unrelated
///    non-gate error for a permitted request.
/// 3. `cancellation_uncertainty` — cancellation without `require_ack` must
///    never report `Acknowledged`; uncertainty or a real error only.
/// 4. `handle_ownership` — the launcher must never claim `Valid` for a
///    handle it did not spawn (PID-reuse defense, design §8).
pub async fn run_conformance(
    adapter: &dyn EngineAdapter,
    launcher: &dyn Launcher,
) -> Vec<CheckResult> {
    let member = MemberRef { deployment_id: "conformance".into(), member_id: "probe".into() };
    let req = RequestRef { id: "conformance-probe".into() };

    vec![
        check_readiness_gating(adapter, &member).await,
        check_park_policy_gate(adapter, &member).await,
        check_cancellation_uncertainty(adapter, &member, &req).await,
        check_handle_ownership(launcher),
    ]
}

async fn check_readiness_gating(adapter: &dyn EngineAdapter, member: &MemberRef) -> CheckResult {
    match adapter.check_readiness(member).await {
        // Readiness has exactly two variants; both are contract-valid.
        Ok(r @ (Readiness::Initializing | Readiness::Ready)) => CheckResult {
            name: "readiness_gating",
            passed: true,
            detail: format!("adapter reports {r:?}"),
        },
        Err(e) => CheckResult {
            name: "readiness_gating",
            passed: false,
            detail: format!("adapter cannot report readiness: {e:?}"),
        },
    }
}

async fn check_park_policy_gate(adapter: &dyn EngineAdapter, member: &MemberRef) -> CheckResult {
    // Invariant 1: level-1 park is not experimental — the gate must not apply.
    let l1 = adapter.park(member, ParkLevel::One).await;
    let l1_ok = !matches!(l1, Err(mllm_adapters::AdapterError::PolicyDenied));

    // Invariant 2: level-2 park is either gated, opted-in, uncertain, or
    // unsupported — but never an unqualified success on a *denied* policy
    // path the caller cannot distinguish. Any of these outcomes conforms:
    // a real gate (PolicyDenied), an opted-in host (Ok), an ambiguous
    // outcome (Uncertain), or a genuinely unsupported capability.
    let l2 = adapter.park(member, ParkLevel::Two).await;
    let l2_ok = matches!(
        l2,
        Ok(_)
            | Err(mllm_adapters::AdapterError::PolicyDenied)
            | Err(mllm_adapters::AdapterError::Uncertain(_))
            | Err(mllm_adapters::AdapterError::UnsupportedCapability)
    );

    CheckResult {
        name: "park_policy_gate",
        passed: l1_ok && l2_ok,
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
            passed: false,
            detail: "cancel without ack reported Acknowledged — fabricated success".into(),
        },
        other => CheckResult {
            name: "cancellation_uncertainty",
            passed: true,
            detail: format!("cancel without ack: {other:?}"),
        },
    }
}

fn check_handle_ownership(launcher: &dyn Launcher) -> CheckResult {
    // A handle to a process this launcher never spawned must not verify as
    // Valid — that is the PID-reuse / ownership hazard.
    let stranger = OwnedHandle { pid: u32::MAX, start_identity: 0 };
    match launcher.verify_handle(&stranger) {
        mllm_adapters::HandleStatus::Valid => CheckResult {
            name: "handle_ownership",
            passed: false,
            detail: "launcher claimed ownership of a never-spawned handle".into(),
        },
        other => CheckResult {
            name: "handle_ownership",
            passed: true,
            detail: format!("unspawned handle verifies as {other:?}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mllm_adapters::fake::{FakeEngine, FakeLauncher};

    #[tokio::test]
    async fn fake_engine_passes_full_conformance_suite() {
        let adapter = FakeEngine::new();
        let launcher = FakeLauncher::new();
        let results = run_conformance(&adapter, &launcher).await;
        assert_eq!(results.len(), 4);
        for r in &results {
            assert!(r.passed, "check {} failed: {}", r.name, r.detail);
        }
    }

    #[tokio::test]
    async fn cancellation_uncertainty_catches_fabricated_success() {
        struct BadAdapter;
        #[async_trait::async_trait]
        impl EngineAdapter for BadAdapter {
            async fn inspect(&self, _: &MemberRef) -> Result<mllm_adapters::EngineState, mllm_adapters::AdapterError> {
                Err(mllm_adapters::AdapterError::Uncertain("bad".into()))
            }
            async fn render_plan(&self, _: &mllm_adapters::PlanInput) -> Result<mllm_adapters::RenderedCommand, mllm_adapters::AdapterError> {
                Err(mllm_adapters::AdapterError::UnsupportedCapability)
            }
            async fn check_readiness(&self, _: &MemberRef) -> Result<Readiness, mllm_adapters::AdapterError> {
                Ok(Readiness::Ready)
            }
            async fn prepare_park(&self, _: &MemberRef) -> Result<mllm_adapters::Quiescence, mllm_adapters::AdapterError> {
                Ok(mllm_adapters::Quiescence { quiescent: true })
            }
            async fn park(&self, _: &MemberRef, _: ParkLevel) -> Result<mllm_adapters::ParkOutcome, mllm_adapters::AdapterError> {
                Err(mllm_adapters::AdapterError::PolicyDenied)
            }
            async fn restore(&self, _: &MemberRef) -> Result<mllm_adapters::RestoreOutcome, mllm_adapters::AdapterError> {
                Ok(mllm_adapters::RestoreOutcome::Restored)
            }
            async fn reload_weights(&self, _: &MemberRef) -> Result<mllm_adapters::ReloadOutcome, mllm_adapters::AdapterError> {
                Ok(mllm_adapters::ReloadOutcome::Reloaded)
            }
            async fn observe_work(&self, _: &MemberRef) -> Result<mllm_adapters::WorkObservation, mllm_adapters::AdapterError> {
                Ok(mllm_adapters::WorkObservation::Idle)
            }
            async fn cancel_work(
                &self,
                _: &MemberRef,
                _: &RequestRef,
                _: bool,
            ) -> Result<mllm_adapters::CancellationOutcome, mllm_adapters::AdapterError> {
                // Contract violation: fabricated success without an ack.
                Ok(mllm_adapters::CancellationOutcome::Acknowledged)
            }
        }

        let launcher = FakeLauncher::new();
        let results = run_conformance(&BadAdapter, &launcher).await;
        let cancel = results.iter().find(|r| r.name == "cancellation_uncertainty").unwrap();
        assert!(!cancel.passed);
        // The rest of the bad adapter's checks still pass.
        let readiness = results.iter().find(|r| r.name == "readiness_gating").unwrap();
        assert!(readiness.passed);
    }
}
