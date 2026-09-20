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

/// Builds a real `InitializeWork` for an SGLang deployment the same way: the
/// golden effective fixture drives the actual store lifecycle, so `spec` reads
/// exactly what production reads.
fn sglang_work() -> InitializeWork {
    let store = Store::open_in_memory().expect("open in-memory store");
    let session = store
        .begin_coordinator_session()
        .expect("begin coordinator session");

    let source: Value = serde_json::from_str(include_str!(
        "../../../mllm-config/tests/fixtures/effective-sglang-golden.json"
    ))
    .expect("fixture JSON parses");
    let host = source["input"]["host"].clone();
    let deployment = source["input"]["deployment"].clone();

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

/// The SGLang branch builds the real frozen native launch and names both
/// credential references it will seal: the launch, its two keys, its wrapper
/// and its log are all resolved here, so a resolved adapter that cannot launch
/// is a construction failure, not a discovery at spawn time.
// T16
#[test]
fn an_sglang_spec_builds_the_native_launch_with_both_credential_references() {
    let work = sglang_work();
    let binding = work.binding_id().to_owned();
    let incarnation = work.incarnation().to_owned();
    let deployment = work.fence().deployment_id.clone();
    let endpoint = work.endpoint().to_owned();
    let spec = bindings().spec(&work).expect("the sglang spec builds");
    let AdapterSpec::Sglang {
        frozen,
        inference,
        admin,
        observer,
        wrapper,
        log,
        session,
    } = spec
    else {
        panic!("the fixture profile declares sglang");
    };
    assert!(observer.is_none(), "a launch adapter has no observer");
    assert!(
        session.is_none(),
        "the coordinator, not the bindings, threads the session ULID"
    );
    assert_eq!(
        frozen.inference_credential_ref(),
        format!("sglang-inference-{binding}")
    );
    assert_eq!(
        frozen.admin_credential_ref(),
        format!("sglang-admin-{binding}")
    );
    assert_eq!(frozen.metadata().binding_id, binding);
    assert_eq!(frozen.metadata().incarnation, incarnation);
    assert_eq!(frozen.metadata().endpoint, format!("http://{endpoint}"));
    // The two keys are fresh, distinct, and hex so the factory can decode and
    // seal both roles.
    for key in [&inference, &admin] {
        assert_eq!(key.len(), 64, "{key}");
        assert!(key.bytes().all(|byte| byte.is_ascii_hexdigit()), "{key}");
    }
    assert_ne!(inference, admin, "the two roles carry distinct keys");
    assert_eq!(
        wrapper,
        Some(PathBuf::from("/tmp/mllm-test-runtime").join("sglang_entry.py"))
    );
    assert_eq!(
        log.as_deref(),
        Some(format!("/tmp/mllm-test-logs/{deployment}/{incarnation}.log").as_str())
    );
}

/// The pinned native builder is SGLang's own: a work that names another family
/// is refused rather than adapted, and the family match in `resolve` is what
/// keeps a spec from being driven through the wrong adapter.
#[test]
fn the_native_builder_refuses_another_family() {
    let work = vllm_work(None);
    assert!(
        crate::native_launch::frozen_from_work(&work, "i-ref".into(), "a-ref".into()).is_err(),
        "a vLLM profile must not build an SGLang launch"
    );
}
