use capyctl_controller::coordinator::permits_send;
use capyctl_store::lifecycle::ArmResult;

#[test]
fn recorded_intent_is_not_replay_permission() {
    assert!(permits_send(&ArmResult::New {
        step_id: "step".into(),
    }));
    assert!(!permits_send(&ArmResult::AlreadyRecorded));
}
