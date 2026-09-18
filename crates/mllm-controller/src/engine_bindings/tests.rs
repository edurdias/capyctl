use super::*;
use mllm_adapters::vllm::args::render_command;
use mllm_config::effective::resolve_effective;
use mllm_domain::resources::MemoryObservation;
use mllm_store::Store;
use serde_json::{json, Value};

/// Builds a real `InitializeWork` for a vLLM deployment by driving the actual
/// store lifecycle: `ProfileBindings::spec` reads the binding's own frozen
/// profile (Spec §3), so a fixture assembled any other way would not exercise
/// the path production actually takes. `deep_park` is `"enabled"` or
/// `"disabled"` written into the host's runtime profile before admission, or
/// `None` for a host file that does not mention the switch at all.
fn vllm_work(deep_park: Option<&str>) -> InitializeWork {
    let store = Store::open_in_memory().expect("open in-memory store");
    let session = store
        .begin_coordinator_session()
        .expect("begin coordinator session");

    let source: Value = serde_json::from_str(include_str!(
        "../../../mllm-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .expect("fixture JSON parses");
    let mut host = source["input"]["host"].clone();
    match deep_park {
        Some(value) => {
            host["runtime_profiles"]["local"]["security"]["deep_park"] = json!(value);
        }
        None => {
            host["runtime_profiles"]["local"]["security"]
                .as_object_mut()
                .expect("the fixture profile carries a security section")
                .remove("deep_park");
        }
    }
    let mut deployment = source["input"]["deployment"].clone();
    if deep_park == Some("disabled") {
        // SPEC §3: a parking residency on a profile that disables deep park is
        // refused at admission (`core.rs`'s `UnsupportedCombination`), so the
        // disabled fixture asks for the residency deep park does not gate.
        deployment["residency"] = json!("restart_only");
    }

    let policy = resolve_effective(&deployment, &host)
        .expect("fixture resolves")
        .host;
    let observations: Vec<_> = policy
        .domains
        .keys()
        .map(|domain| MemoryObservation {
            domain: domain.clone(),
            capacity_bytes: 1_i64 << 50,
            available_bytes: 1_i64 << 50,
            sampled_at_ms: 1000,
        })
        .collect();
    store
        .import_resource_policy(&session, &policy, &observations, 1000)
        .expect("import resource policy");

    let receipt = store
        .create_stopped_managed_configuration(
            &session,
            "owner",
            "toy",
            &json!({ "config": deployment }).to_string(),
            &host,
            1700,
        )
        .expect("create managed configuration");
    let fence = mllm_store::lifecycle::DeploymentFence {
        deployment_id: receipt.deployment_id,
        revision: receipt.revision,
        generation: receipt.generation,
    };
    store
        .accept_start(&session, &fence, 1800, 10_000)
        .expect("accept start");
    store
        .next_initialize(&session)
        .expect("next initialize")
        .expect("a freshly accepted start plans initialize work")
}

fn bindings() -> ProfileBindings {
    ProfileBindings::new(
        PathBuf::from("/tmp/mllm-test-logs"),
        PathBuf::from("/tmp/mllm-test-runtime"),
    )
}

/// Spec §3: a host that disables deep park launches without sleep mode and
/// with vLLM's own development mode off, whatever the profile asked for.
// T21
#[test]
fn a_disabled_profile_launches_without_sleep_flags() {
    let work = vllm_work(Some("disabled"));
    let spec = bindings().spec(&work).expect("vllm spec builds");
    let AdapterSpec::Vllm { policy, launch, .. } = spec else {
        panic!("fixture profile declares vllm");
    };
    assert_eq!(policy, ParkPolicy::Disabled);
    let launch = launch.expect("an owned vllm binding carries a launch plan");
    assert!(
        launch.sleep_flags.is_empty(),
        "deep_park disabled must render no sleep flags: {:?}",
        launch.sleep_flags
    );
    let rendered = render_command(&launch).expect("plan renders");
    assert_eq!(
        rendered.env.get("VLLM_SERVER_DEV_MODE").map(String::as_str),
        Some("0")
    );
}

/// Spec §3: the default profile launches ready to park, in vLLM's development
/// mode, with the sleep/eager-load flags that mode needs.
// T21
#[test]
fn an_enabled_profile_launches_with_sleep_mode() {
    let work = vllm_work(Some("enabled"));
    let spec = bindings().spec(&work).expect("vllm spec builds");
    let AdapterSpec::Vllm { policy, launch, .. } = spec else {
        panic!("fixture profile declares vllm");
    };
    assert_eq!(policy, ParkPolicy::Enabled);
    let launch = launch.expect("an owned vllm binding carries a launch plan");
    assert!(
        !launch.sleep_flags.is_empty(),
        "deep_park enabled with sleep mode requested must render sleep flags"
    );
    let rendered = render_command(&launch).expect("plan renders");
    assert_eq!(
        rendered.env.get("VLLM_SERVER_DEV_MODE").map(String::as_str),
        Some("1")
    );
}

/// Spec §3 and the owner's 2026-09-17 decision: deep park is on by default and a
/// host opts out, so a host file that never mentions the switch launches ready to
/// park. The enabled case above writes the value; only this one proves the
/// schema's own default.
// T21
#[test]
fn a_profile_that_does_not_mention_deep_park_launches_with_it_enabled() {
    let work = vllm_work(None);
    let spec = bindings().spec(&work).expect("vllm spec builds");
    let AdapterSpec::Vllm { policy, launch, .. } = spec else {
        panic!("the fixture profile declares vllm");
    };
    assert_eq!(policy, ParkPolicy::Enabled);
    let launch = launch.expect("an owned vllm binding carries a launch plan");
    assert!(
        !launch.sleep_flags.is_empty(),
        "an unmentioned deep park is an enabled one"
    );
    let rendered = render_command(&launch).expect("plan renders");
    assert_eq!(
        rendered.env.get("VLLM_SERVER_DEV_MODE").map(String::as_str),
        Some("1")
    );
}

/// A family whose production prerequisites are missing must be refused by name.
/// Building it with placeholder credentials would produce a runtime that looks
/// configured and is not, and the failure would surface later as an unauthorised
/// control rather than here as a missing prerequisite.
#[test]
fn sglang_is_refused_by_name_rather_than_stubbed() {
    let CoordinatorError::Service(message) =
        ProfileBindings::missing("SGLang", "its controls need a resolved admin credential")
    else {
        panic!("a missing prerequisite is a service failure");
    };
    assert!(message.contains("SGLang"), "{message}");
    assert!(message.contains("credential"), "{message}");
    assert!(
        message.contains("Refusing"),
        "the refusal must be explicit: {message}"
    );
}
