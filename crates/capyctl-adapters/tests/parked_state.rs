use capyctl_adapters::ParkPolicy;
use capyctl_adapters::{EngineAdapter, MemberRef, ParkLevel};
use capyctl_testkit::FakeEngine;

fn member() -> MemberRef {
    MemberRef {
        deployment_id: "d".into(),
        member_id: "m".into(),
    }
}

#[tokio::test]
async fn parked_engine_reports_parked_phase_not_ready() {
    let e = FakeEngine::new().with_policy(ParkPolicy::Enabled);
    e.park(&member(), ParkLevel::Two).await.unwrap();
    let st = e.inspect(&member()).await.unwrap();
    assert!(matches!(st.phase, capyctl_adapters::Phase::Parked));
    assert!(st.build_fingerprint.is_some());
    // readiness must not claim Ready for a parked engine:
    let rd = e.check_readiness(&member()).await.unwrap();
    assert!(!matches!(rd, capyctl_adapters::Readiness::Ready));
}

#[tokio::test]
async fn restore_returns_to_ready_with_fingerprint() {
    let e = FakeEngine::new().with_policy(ParkPolicy::Enabled);
    e.park(&member(), ParkLevel::Two).await.unwrap();
    e.restore(&member()).await.unwrap();
    let st = e.inspect(&member()).await.unwrap();
    assert!(matches!(st.phase, capyctl_adapters::Phase::Ready));
}
