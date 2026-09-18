//! Embedded host supervision (F0): runs the fake engine adapter and fake
//! launcher in-process against the controller's requests.
//!
//! No enrollment, no network: the standalone deployment graph is
//! server + host over one store. The host carries the deep-park security
//! gate at its default [`ParkPolicy::Disabled`] — experimental level-2 park
//! and weight reload are deterministically denied unless a later phase
//! explicitly opts the host in (design §9.1).

use std::sync::Arc;

use mllm_adapters::fake::{FakeEngine, FakeLauncher, ParkPolicy};

pub mod doctor;
pub mod memory;
use mllm_adapters::{EngineAdapter, Launcher};

/// An embedded, supervised host in F0: fake engine + fake launcher,
/// both shared behind arcs so the controller can execute against them.
#[derive(Debug)]
pub struct Host {
    engine: Arc<FakeEngine>,
    launcher: Arc<FakeLauncher>,
}

impl Host {
    /// Boot an embedded host with the default (denied) deep-park policy.
    pub fn new() -> Self {
        Self {
            engine: Arc::new(FakeEngine::new()),
            launcher: Arc::new(FakeLauncher::new()),
        }
    }

    /// The engine-side participant the controller executes operations against.
    pub fn adapter(&self) -> Arc<dyn EngineAdapter> {
        self.engine.clone()
    }

    /// The process-side participant used to spawn and terminate engines.
    pub fn launcher(&self) -> Arc<dyn Launcher> {
        self.launcher.clone()
    }

    /// The host's deep-park policy gate (F0 default: [`ParkPolicy::Disabled`]).
    pub fn park_policy(&self) -> ParkPolicy {
        ParkPolicy::Disabled
    }
}

impl Default for Host {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mllm_adapters::{AdapterError, MemberRef, ParkLevel};

    #[tokio::test]
    async fn embedded_host_denies_experimental_deep_park_by_default() {
        let host = Host::new();
        assert_eq!(host.park_policy(), ParkPolicy::Disabled);
        let member = MemberRef { deployment_id: "d-1".into(), member_id: "m-1".into() };
        let err = host
            .adapter()
            .park(&member, ParkLevel::Two)
            .await
            .unwrap_err();
        assert!(matches!(err, AdapterError::PolicyDenied));
    }

    #[tokio::test]
    async fn embedded_host_runs_the_fake_lifecycle_pieces() {
        let host = Host::new();
        let member = MemberRef { deployment_id: "d-1".into(), member_id: "m-1".into() };
        // Startup is not readiness: inspect before any readiness check shows Startup.
        assert_eq!(host.adapter().inspect(&member).await.unwrap().phase, mllm_adapters::Phase::Startup);
        let cmd = host
            .adapter()
            .render_plan(&mllm_adapters::PlanInput {
                deployment_id: "d-1".into(),
                member_id: "m-1".into(),
                park_level: None,
                engine_api_key: None,
            })
            .await
            .unwrap();
        let handle = host.launcher().spawn(&cmd).unwrap();
        assert_eq!(
            host.launcher().verify_handle(&handle),
            mllm_adapters::HandleStatus::Valid
        );
        assert_eq!(
            host.adapter().check_readiness(&member).await.unwrap(),
            mllm_adapters::Readiness::Ready
        );
        let parked = host
            .adapter()
            .park(&member, ParkLevel::One)
            .await
            .unwrap();
        assert!(matches!(parked, mllm_adapters::ParkOutcome::Parked { .. }));
        assert_eq!(
            host.adapter().restore(&member).await.unwrap(),
            mllm_adapters::RestoreOutcome::Restored
        );
        let report = host
            .launcher()
            .terminate(&handle, std::time::Duration::from_secs(1))
            .unwrap();
        assert_eq!(report.exit_code, Some(0));
    }
}
