use super::*;

const CAPACITY: i64 = 128 * 1024 * 1024 * 1024;

/// Standalone must declare an engine installation, because a deployment can only be
/// qualified against a profile the host has published. Declaring none is what made
/// every start refuse.
#[test]
fn the_host_declares_exactly_one_engine_installation() {
    let host = host_policy("fake", "/bin/true", "fp-1", false, CAPACITY);
    let profiles = host["runtime_profiles"].as_object().expect("profiles object");
    assert_eq!(profiles.len(), 1, "one installation, named not anonymous");
    let profile = &profiles[STANDALONE_PROFILE];
    assert_eq!(profile["engine"], "fake");
    assert_eq!(profile["executable"], "/bin/true");
    assert_eq!(profile["build_fingerprint"], "fp-1");
}

/// Deep-park paths are the host's decision, not the adapter's (SPEC §9.1, T21), so
/// the profile must carry the opt-in rather than leaving it to be re-derived.
#[test]
fn experimental_controls_are_carried_by_the_profile() {
    for allowed in [false, true] {
        let host = host_policy("vllm", "/opt/vllm", "fp", allowed, CAPACITY);
        assert_eq!(
            host["runtime_profiles"][STANDALONE_PROFILE]["security"]["experimental_controls"],
            allowed
        );
    }
}

/// Limits come from observed capacity. An invented ceiling is how a host gets
/// overcommitted, so a larger machine must yield larger limits.
#[test]
fn limits_scale_with_observed_capacity() {
    let small = host_policy("fake", "/bin/true", "fp", false, 16 << 30);
    let large = host_policy("fake", "/bin/true", "fp", false, 128 << 30);
    let managed = |h: &Value| {
        h["resource_policy"]["domains"][DOMAIN]["managed_limit"]
            .as_str()
            .unwrap()
            .trim_end_matches('B')
            .parse::<i64>()
            .unwrap()
    };
    assert!(managed(&large) > managed(&small), "limits follow the host");
    assert_eq!(managed(&small), (16i64 << 30) / 100 * MANAGED_FRACTION);
}

/// Admission must fail before the host does, so the managed ceiling plus the free
/// reserve must fit inside observed capacity rather than counting the same bytes
/// twice. Asserted on the produced policy, not on the constants that built it.
#[test]
fn the_managed_ceiling_and_reserve_fit_inside_capacity() {
    let host = host_policy("fake", "/bin/true", "fp", false, CAPACITY);
    let bytes = |field: &str| {
        host["resource_policy"]["domains"][DOMAIN][field]
            .as_str()
            .unwrap()
            .trim_end_matches('B')
            .parse::<i64>()
            .unwrap()
    };
    assert!(
        bytes("managed_limit") + bytes("free_reserve") <= CAPACITY,
        "a ceiling that overlaps its reserve admits work the host cannot hold"
    );
    assert!(bytes("parked_limit") <= bytes("managed_limit"));
}

/// Admission compares a transition's true peak against the ceiling, so every phase
/// must be declared and the peak must be a transition rather than steady state.
#[test]
fn every_phase_is_declared_and_the_peak_is_a_transition() {
    let d = deployment_document("m", "m", "/models/m", CAPACITY);
    let resources = d["resources"].as_object().unwrap();
    for phase in ["cold", "ready", "parking", "parked", "wake"] {
        assert!(resources.contains_key(phase), "{phase} must be declared");
    }
    let bytes = |phase: &str| {
        resources[phase]["allocations"][0]["bytes"]
            .as_str()
            .unwrap()
            .trim_end_matches('B')
            .parse::<i64>()
            .unwrap()
    };
    assert!(bytes("cold") > bytes("ready"), "loading costs more than serving");
    assert!(bytes("wake") > bytes("ready"));
    assert!(bytes("parked") < bytes("ready"), "parked retains only residue");
}

/// A parked deployment holds no device; that is what makes parking reclaim anything.
#[test]
fn a_parked_deployment_holds_no_device() {
    let d = deployment_document("m", "m", "/models/m", CAPACITY);
    assert_eq!(
        d["resources"]["parked"]["devices"].as_array().unwrap().len(),
        0
    );
    assert_eq!(d["resources"]["ready"]["devices"].as_array().unwrap().len(), 1);
}

/// The deployment must name the profile it runs on, or there is nothing to qualify
/// it against.
#[test]
fn the_deployment_names_its_installation() {
    let d = deployment_document("m", "route-m", "/models/m", CAPACITY);
    assert_eq!(d["profile"], STANDALONE_PROFILE);
    assert_eq!(d["routes"][0], "route-m");
}
