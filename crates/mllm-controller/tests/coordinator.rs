use mllm_controller::coordinator::permits_send;
use mllm_store::candidate_creation::initialize::ArmResult;

#[test]
fn recorded_intent_is_not_replay_permission() {
    assert!(permits_send(&ArmResult::New {
        step_id: "step".into(),
    }));
    assert!(!permits_send(&ArmResult::AlreadyRecorded));
}
