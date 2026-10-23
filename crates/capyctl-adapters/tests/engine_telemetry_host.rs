//! Owner decision 2026-10-22: the host's `engine_telemetry` holds for every
//! launch from the role's start. Its own test binary, because the setting is
//! process-wide and every other test reads its default.
use capyctl_adapters::engine_telemetry::{
    apply, host_setting, set_host_setting, EngineTelemetry, DO_NOT_TRACK, VLLM,
};
use std::collections::BTreeMap;

// T21 (owner decision 2026-10-22): `off` until a role says otherwise; a role
// that allows usage reporting gets launches without the opt-outs, and one
// that turns it off again gets them back.
#[test]
fn the_role_setting_reaches_every_launch() {
    let launch = |opt_outs| {
        let mut env = BTreeMap::new();
        apply(&mut env, opt_outs, host_setting());
        env
    };
    assert_eq!(host_setting(), EngineTelemetry::Off);
    assert_eq!(launch(VLLM).len(), 3);
    assert_eq!(launch(DO_NOT_TRACK)["DO_NOT_TRACK"], "1");
    set_host_setting(EngineTelemetry::Allow);
    assert_eq!(host_setting(), EngineTelemetry::Allow);
    assert!(launch(VLLM).is_empty());
    assert!(launch(DO_NOT_TRACK).is_empty());
    set_host_setting(EngineTelemetry::Off);
    assert_eq!(launch(VLLM)["VLLM_NO_USAGE_STATS"], "1");
}
