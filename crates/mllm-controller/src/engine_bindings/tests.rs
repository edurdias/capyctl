use super::*;
use mllm_adapters::vllm::args::render_command;
use mllm_adapters::ParkPolicy;
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
    // SPEC §3: a parking residency on a profile that opts out of deep park is
    // refused at admission (`core.rs`'s `UnsupportedCombination`), so the
    // opted-out fixture asks for the residency deep park does not gate.
    let residency = if deep_park == Some("disabled") {
        "restart_only"
    } else {
        "deep"
    };
    vllm_work_with(deep_park, residency)
}

/// [`vllm_work`] with the deployment's declared residency stated explicitly.
fn vllm_work_with(deep_park: Option<&str>, residency: &str) -> InitializeWork {
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
    deployment["residency"] = json!(residency);

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
    sglang_work_edit(|_| {})
}

/// Same fixture builder with a deployment mutation hook, so a test can shape
/// the frozen effective configuration before admission.
fn sglang_work_edit(edit: impl FnOnce(&mut Value)) -> InitializeWork {
    let store = Store::open_in_memory().expect("open in-memory store");
    let session = store
        .begin_coordinator_session()
        .expect("begin coordinator session");

    let source: Value = serde_json::from_str(include_str!(
        "../../../mllm-config/tests/fixtures/effective-sglang-golden.json"
    ))
    .expect("fixture JSON parses");
    let host = source["input"]["host"].clone();
    let mut deployment = source["input"]["deployment"].clone();
    edit(&mut deployment);

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

/// SPEC §9.1: an explicitly enabled profile launches ready to park, in vLLM's development
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

/// SPEC §9.1 / ADR 0012: omitted host policy enables deep parking, and the
/// mandatory controls come with it: the guard middleware is loaded and the
/// engine key never reaches argv.
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
    assert!(!launch.sleep_flags.is_empty());
    let rendered = render_command(&launch).expect("plan renders");
    assert_eq!(
        rendered.env.get("VLLM_SERVER_DEV_MODE").map(String::as_str),
        Some("1")
    );
    assert!(rendered
        .argv
        .windows(2)
        .any(|pair| pair == ["--middleware", "mllm_vllm_guard.RequireEngineKey"]));
    assert!(!rendered.argv.iter().any(|argument| argument == "--api-key"));
}

/// SPEC §6.2: `restart_only` prohibits sleep calls. Default-on deep parking
/// (ADR 0012) does not give a restart-only deployment a park-capable adapter.
// T21
#[test]
fn a_restart_only_deployment_never_gets_a_park_policy() {
    for deep_park in [None, Some("enabled"), Some("disabled")] {
        let work = vllm_work_with(deep_park, "restart_only");
        let spec = bindings().spec(&work).expect("vllm spec builds");
        let AdapterSpec::Vllm { policy, .. } = spec else {
            panic!("the fixture profile declares vllm");
        };
        assert_eq!(policy, ParkPolicy::Disabled, "{deep_park:?}");
    }
}

/// SPEC §9.1 / T21, ADR 0012: an embedded vLLM launch keys its control routes
/// apart from inference, exactly as a remote host's does. The spec carries a
/// fresh admin credential beside the inference key, the two differ, and each
/// build issues new ones (SPEC §13.3: one key per launch).
// T21 T37
#[test]
fn a_vllm_spec_carries_a_fresh_admin_key_apart_from_the_inference_key() {
    let work = vllm_work(None);
    let keys = || {
        let AdapterSpec::Vllm {
            engine_key,
            admin_key,
            ..
        } = bindings().spec(&work).expect("vllm spec builds")
        else {
            panic!("the fixture profile declares vllm");
        };
        (
            engine_key.expect("an owned launch carries an inference key"),
            admin_key.expect("an owned launch carries an admin key"),
        )
    };
    let (inference, admin) = keys();
    assert_ne!(inference, admin, "the two roles must carry distinct keys");
    for key in [&inference, &admin] {
        assert_eq!(hex::decode(key).expect("hex").len(), 32);
    }
    let (next_inference, next_admin) = keys();
    assert_ne!(inference, next_inference);
    assert_ne!(admin, next_admin, "an admin key is never reused across launches");
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
        ..
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
    // The served name is the deployment's own route name, the same rule the
    // vLLM branch follows — here the golden fixture's first route.
    assert_eq!(frozen.metadata().served_name, "toy");
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

/// SPEC §§6.2, 9.2: restart-only is a first-class tier for every backend. An
/// SGLang `restart_only` deployment builds its native launch, and the rendered
/// public settings carry no memory saver and no weight backup, so the engine it
/// starts has no park surface.
// T21 T22
#[test]
fn a_restart_only_sglang_deployment_launches_without_the_memory_saver() {
    let work = sglang_work_edit(|deployment| {
        deployment["residency"] = json!("restart_only");
    });
    assert_eq!(
        work.effective().residency,
        mllm_config::effective::Residency::RestartOnly
    );
    let spec = bindings()
        .spec(&work)
        .expect("a restart_only SGLang spec builds");
    let AdapterSpec::Sglang { frozen, .. } = spec else {
        panic!("the fixture profile declares sglang");
    };
    mllm_adapters::sglang::SglangAdapter::from_frozen(&frozen, None)
        .expect("the frozen restart_only shape is accepted");
    let rendered = mllm_adapters::sglang::SglangLaunch::from_frozen(&frozen)
        .expect("the restart_only launch renders");
    let settings = &rendered.public_metadata()["settings"];
    assert_eq!(settings["memory_saver"], json!(false));
    assert_eq!(settings["cpu_weight_backup"], json!(false));
    assert_eq!(settings["weight_restore"], json!("disk_reload"));
}

/// The pinned native builder is SGLang's own: a work that names another family
/// is refused rather than adapted, and the family match in `resolve` is what
/// keeps a spec from being driven through the wrong adapter.
#[test]
fn the_native_builder_refuses_another_family() {
    let work = vllm_work(None);
    assert!(
        crate::native_launch::frozen_from_work(&work, "toy".into(), "i-ref".into(), "a-ref".into())
            .is_err(),
        "a vLLM profile must not build an SGLang launch"
    );
}

/// Spec §3: the served name is the deployment's first frozen route, not a
/// derived binding artifact and not the deployment name. The ordinary writer
/// freezes routes in its own stored order; `routes.first()` on the frozen
/// effective is the rule, exactly as the vLLM branch reads it.
// T16
#[test]
fn an_sglang_served_name_is_the_frozen_effective_first_route() {
    let work = sglang_work_edit(|deployment| {
        deployment["routes"] = json!(["gamma", "alpha"]);
    });
    let first = work.effective().routes.first().cloned().unwrap();
    let spec = bindings().spec(&work).expect("the sglang spec builds");
    let AdapterSpec::Sglang { frozen, .. } = spec else {
        panic!("the fixture profile declares sglang");
    };
    assert_ne!(first, "toy", "the route must not fall back to the name");
    assert_eq!(frozen.metadata().served_name, first);
}

/// SPEC §9.2 (W5): with the host's saver observation directory, a memory-saver
/// SGLang launch is built with the launch-wide observer that parks and restores
/// it on saver evidence and hands the launch that directory to enroll in; a
/// `restart_only` launch gets none. A restarted standalone builds the same
/// observer over the same directory, so a parked launch it adopts is observed
/// through the enrollment the engine made before the restart.
// T22 T33
#[test]
fn a_memory_saver_sglang_spec_carries_the_saver_observer() {
    let dir = PathBuf::from("/tmp/mllm-test-state/observation");
    let observed = || bindings().with_saver_observation(dir.clone());
    for _restart in 0..2 {
        let spec = observed().spec(&sglang_work()).expect("the sglang spec builds");
        let AdapterSpec::Sglang {
            frozen, observer, ..
        } = spec
        else {
            panic!("the fixture profile declares sglang");
        };
        assert!(frozen.settings().memory_saver);
        let observer = observer.expect("a memory-saver launch is observed");
        assert_eq!(observer.observation_dir(), Some(dir.clone()));
    }
    let restart_only = sglang_work_edit(|deployment| {
        deployment["residency"] = json!("restart_only");
    });
    let AdapterSpec::Sglang { observer, .. } = observed().spec(&restart_only).unwrap() else {
        panic!("the fixture profile declares sglang");
    };
    assert!(observer.is_none(), "a restart_only launch never parks");
}
