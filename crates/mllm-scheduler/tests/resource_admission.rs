use mllm_domain::resources::{Allocation, DeviceClaim, PhaseFootprint, ResourcePhase, Sharing};
use mllm_scheduler::residency::*;

#[test]
fn shared_claims_can_overlap() {
    let shared = DeviceClaim { device: "gpu:0".into(), sharing: Sharing::Shared };
    let exclusive = DeviceClaim { device: "gpu:0".into(), sharing: Sharing::Exclusive };
    assert!(!claims_conflict(std::slice::from_ref(&shared), std::slice::from_ref(&shared)));
    assert!(claims_conflict(std::slice::from_ref(&shared), std::slice::from_ref(&exclusive)));
    assert!(claims_conflict(&[exclusive], &[shared]));
}

#[test]
fn invalid_and_duplicate_allocations_are_rejected() {
    let a = Allocation { domain: "system".into(), bytes: 10, host_kv_bytes: 11 };
    let mut f = PhaseFootprint {
        phase: ResourcePhase::Ready, allocations: vec![a], devices: vec![],
    };
    assert_eq!(validate_footprint(&f), Err(ResourceError::Invalid));
    f.allocations[0].host_kv_bytes = 0;
    f.allocations.push(f.allocations[0].clone());
    assert_eq!(validate_footprint(&f), Err(ResourceError::Invalid));
    f.allocations.pop();
    f.allocations[0].bytes = -1;
    assert_eq!(validate_footprint(&f), Err(ResourceError::Invalid));
}
