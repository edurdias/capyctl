pub mod fake;
pub mod sglang;
pub mod traits;
pub mod vllm;

pub use traits::*;

#[cfg(test)]
mod tests {
    use super::traits::*;
    use std::time::Duration;

    /// A minimal adapter that honors the contract: with no engine behind it,
    /// every outcome is uncertain — never a false success.
    struct NullAdapter;

    #[async_trait::async_trait]
    impl EngineAdapter for NullAdapter {
        async fn inspect(&self, _member: &MemberRef) -> Result<EngineState, AdapterError> {
            Err(AdapterError::Uncertain("null adapter".into()))
        }
        async fn render_plan(&self, _plan: &PlanInput) -> Result<RenderedCommand, AdapterError> {
            Err(AdapterError::UnsupportedCapability)
        }
        async fn check_readiness(&self, _member: &MemberRef) -> Result<Readiness, AdapterError> {
            Err(AdapterError::Uncertain("null adapter".into()))
        }
        async fn prepare_park(&self, _member: &MemberRef) -> Result<Quiescence, AdapterError> {
            Err(AdapterError::Uncertain("null adapter".into()))
        }
        async fn park(&self, _member: &MemberRef, _level: ParkLevel) -> Result<ParkOutcome, AdapterError> {
            Err(AdapterError::Uncertain("null adapter".into()))
        }
        async fn restore(&self, _member: &MemberRef) -> Result<RestoreOutcome, AdapterError> {
            Err(AdapterError::Uncertain("null adapter".into()))
        }
        async fn reload_weights(&self, _member: &MemberRef) -> Result<ReloadOutcome, AdapterError> {
            Err(AdapterError::Uncertain("null adapter".into()))
        }
        async fn observe_work(&self, _member: &MemberRef) -> Result<WorkObservation, AdapterError> {
            Err(AdapterError::Uncertain("null adapter".into()))
        }
        async fn cancel_work(
            &self,
            _member: &MemberRef,
            _req: &RequestRef,
            _require_ack: bool,
        ) -> Result<CancellationOutcome, AdapterError> {
            // The contract: no ack → uncertainty, never success.
            Ok(CancellationOutcome::Uncertain)
        }
    }

    fn member() -> MemberRef {
        MemberRef { deployment_id: "d-1".into(), member_id: "m-1".into() }
    }

    #[tokio::test]
    async fn cancel_without_ack_is_uncertain_never_success() {
        let adapter = NullAdapter;
        let req = RequestRef { id: "r-1".into() };
        let outcome = adapter.cancel_work(&member(), &req, false).await.unwrap();
        assert_eq!(outcome, CancellationOutcome::Uncertain);
    }

    #[tokio::test]
    async fn null_adapter_errors_are_uncertain_for_inspect() {
        let adapter = NullAdapter;
        assert!(matches!(
            adapter.inspect(&member()).await,
            Err(AdapterError::Uncertain(_))
        ));
    }

    #[test]
    fn payload_types_carry_the_fields_task_9_reads() {
        let state = EngineState { phase: Phase::Ready, retained_bytes: 1024, build_fingerprint: None };
        assert_eq!(state.retained_bytes, 1024);
        assert_eq!(state.phase, Phase::Ready);

        let handle = OwnedHandle { pid: 4242, start_identity: 0xdead_beef_u128 };
        assert_eq!(handle.pid, 4242);
        assert_eq!(handle.start_identity, 0xdead_beef_u128);

        assert_eq!(HandleStatus::Valid, HandleStatus::Valid);
        assert_eq!(HandleStatus::StaleReused, HandleStatus::StaleReused);
        assert_eq!(HandleStatus::Gone, HandleStatus::Gone);

        let q = Quiescence { quiescent: true };
        assert!(q.quiescent);
    }

    #[test]
    fn launcher_contract_is_object_safe_and_matches_handle_status() {
        fn assert_object_safe<T: Launcher + ?Sized>() {}
        assert_object_safe::<dyn Launcher>();

        struct NullLauncher;
        impl Launcher for NullLauncher {
            fn spawn(&self, _cmd: &RenderedCommand) -> Result<OwnedHandle, LauncherError> {
                Err(LauncherError::SpawnFailed("null launcher".into()))
            }
            fn terminate(&self, _h: &OwnedHandle, _grace: Duration) -> Result<ExitReport, LauncherError> {
                Err(LauncherError::TerminateFailed("null launcher".into()))
            }
            fn verify_handle(&self, _h: &OwnedHandle) -> HandleStatus {
                HandleStatus::Gone
            }
        }

        let l = NullLauncher;
        let cmd = RenderedCommand { argv: vec!["engine".into()], env: Default::default() };
        assert!(l.spawn(&cmd).is_err());
        let h = OwnedHandle { pid: 1, start_identity: 1 };
        assert_eq!(l.verify_handle(&h), HandleStatus::Gone);
        assert!(l.terminate(&h, Duration::from_secs(1)).is_err());
    }
}
