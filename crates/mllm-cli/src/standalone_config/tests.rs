use super::*;

use std::sync::Mutex;

use mllm_config::effective::{DeepPark, ModelSource};
use mllm_config::engine_policy::Engine;
use mllm_controller::{EngineInstallation, EngineProvider as _};
use mllm_domain::launch::ProfileLaunchSettings;

const CAPACITY: i64 = 128 * 1024 * 1024 * 1024;

/// The environment is process-wide, so the tests that read it take turns. Running
/// them concurrently would let one test's exports decide another's result.
static ENVIRONMENT: Mutex<()> = Mutex::new(());

/// An installation with nothing interesting in it, for the tests that are about the
/// shape of the published table rather than about any particular engine.
fn installed(engine: Engine, executable: &str) -> EngineInstallation {
    EngineInstallation {
        engine,
        executable: executable.into(),
        build_fingerprint: "fp-1".into(),
        launch_settings: mllm_testkit::vllm_launch_settings_json(),
        deep_park: false,
        trust_remote_code: false,
        models_root: "/srv/models".into(),
        runtime_dir: "/opt/mllm/runtime".into(),
        args: Vec::new(),
    }
}

fn local(path: &str) -> ModelSource {
    ModelSource::Local { path: path.into() }
}

/// Standalone must declare an engine installation, because a deployment can only be
/// qualified against a profile the host has published. Declaring none is what made
/// every start refuse.
#[test]
fn the_host_declares_exactly_one_engine_installation() {
    let host = host_policy(&installed(Engine::Vllm, "/bin/true"), "env-1", CAPACITY);
    let profiles = host["runtime_profiles"].as_object().expect("profiles object");
    assert_eq!(profiles.len(), 1, "one installation, named not anonymous");
    let profile = &profiles[STANDALONE_PROFILE];
    assert_eq!(profile["engine"], "vllm");
    assert_eq!(profile["executable"], "/bin/true");
    assert_eq!(profile["build_fingerprint"], "fp-1");
}

/// Deep-park paths are the host's decision, not the adapter's (SPEC §9.1, T21), so
/// the profile must carry the opt-in rather than leaving it to be re-derived.
#[test]
fn the_deep_park_switch_is_carried_by_the_profile() {
    for allowed in [false, true] {
        let mut installation = installed(Engine::Vllm, "/opt/vllm");
        installation.deep_park = allowed;
        let host = host_policy(&installation, "env-1", CAPACITY);
        assert_eq!(
            host["runtime_profiles"][STANDALONE_PROFILE]["security"]["deep_park"],
            if allowed { "enabled" } else { "disabled" }
        );
    }
}

/// Spec §3: running Python that arrived inside a checkpoint is the host's decision
/// too, and it is a separate one from parking.
#[test]
fn trusting_checkpoint_code_is_published_separately_from_deep_park() {
    let mut installation = installed(Engine::Vllm, "/opt/vllm");
    installation.trust_remote_code = true;
    let host = host_policy(&installation, "env-1", CAPACITY);
    let security = &host["runtime_profiles"][STANDALONE_PROFILE]["security"];
    assert_eq!(security["trust_remote_code"], true);
    assert_eq!(security["deep_park"], "disabled");
}

/// Spec §7: a relative model path resolves against the store the host names, so the
/// published table has to carry the installation's own store rather than a default.
#[test]
fn the_published_host_names_the_store_its_weights_live_under() {
    let mut installation = installed(Engine::Vllm, "/opt/vllm");
    installation.models_root = "/data/checkpoints".into();
    let host = host_policy(&installation, "env-1", CAPACITY);
    assert_eq!(host["model_store"]["path"], "/data/checkpoints");
}

/// Limits come from observed capacity. An invented ceiling is how a host gets
/// overcommitted, so a larger machine must yield larger limits.
#[test]
fn limits_scale_with_observed_capacity() {
    let installation = installed(Engine::Vllm, "/bin/true");
    let small = host_policy(&installation, "env-1", 16 << 30);
    let large = host_policy(&installation, "env-1", 128 << 30);
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
    let host = host_policy(&installed(Engine::Vllm, "/bin/true"), "env-1", CAPACITY);
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
    let d = deployment_document("m", "m", &local("/models/m"), CAPACITY);
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
    let d = deployment_document("m", "m", &local("/models/m"), CAPACITY);
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
    let d = deployment_document("m", "route-m", &local("/models/m"), CAPACITY);
    assert_eq!(d["runtime_profile"], STANDALONE_PROFILE);
    assert_eq!(d["routes"][0], "route-m");
}

/// Spec §7: the deployment states where its weights come from as a source, so a
/// checkpoint that has to be fetched is expressible in the same document.
#[test]
fn the_deployment_states_its_model_source() {
    let d = deployment_document("m", "m", &local("/models/m"), CAPACITY);
    assert_eq!(d["model"]["source"]["type"], "local");
    assert_eq!(d["model"]["source"]["path"], "/models/m");

    let fetched = deployment_document(
        "m",
        "m",
        &ModelSource::HuggingFace {
            repo: "org/model".into(),
            revision: None,
            locked_commit: None,
        },
        CAPACITY,
    );
    assert_eq!(fetched["model"]["source"]["type"], "huggingface");
    assert_eq!(fetched["model"]["source"]["repo"], "org/model");
}

/// The hardware standalone runs on has one physical pool, which is what the domain
/// name has always claimed and nothing has ever stated. Declaring it is what makes
/// a host-backed park refusable rather than silently useless.
#[test]
fn the_published_host_declares_one_memory_pool() {
    let host = host_policy(&installed(Engine::Vllm, "/bin/true"), "env-1", 1 << 40);
    assert_eq!(
        host["resource_policy"]["domains"]["unified"]["memory"],
        "unified"
    );
}

/// Standalone's deployments stay restart-only until ordinary park exists. The point
/// of asserting it is that the value is now one of three rather than one of two, so
/// a later change to a parking tier is a deliberate edit with a test behind it.
#[test]
fn a_standalone_deployment_is_restart_only() {
    let deployment = deployment_document("m", "m", &local("/models/m"), 1 << 40);
    assert_eq!(deployment["residency"], "restart_only");
}

/// An executable that prints a version, as an engine installation's would.
fn fake_engine_bin(dir: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin_dir = dir.join("venv").join("bin");
    std::fs::create_dir_all(&bin_dir).expect("a bin directory");
    let bin = bin_dir.join("vllm");
    std::fs::write(&bin, "#!/bin/sh\necho 'vllm 0.29.0'\n").expect("the engine script");
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).expect("executable");
    bin
}

/// Spec §7: the host policy built from the environment carries the full vLLM launch
/// settings, the model store, deep park enabled by default and the flags the
/// profile passes — and it resolves, which is what a deployment is qualified
/// against. A table that merely looked complete but did not resolve would refuse
/// every start at the point where the refusal is hardest to read.
#[test]
fn host_policy_from_env_is_complete() {
    let _guard = ENVIRONMENT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = tempfile::TempDir::new().expect("a temporary installation");
    let bin = fake_engine_bin(dir.path());
    let models = dir.path().join("models");
    std::fs::create_dir_all(&models).expect("a model store");
    std::env::set_var("MLLM_VLLM_BIN", &bin);
    std::env::set_var("MLLM_MODELS_ROOT", &models);
    std::env::set_var(
        "MLLM_RUNTIME_DIR",
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime"),
    );
    for name in [
        "MLLM_ENGINE_FINGERPRINT",
        "MLLM_KV_CACHE_BYTES",
        "MLLM_ENGINE_ARGS",
        "MLLM_DEEP_PARK",
        "MLLM_TRUST_REMOTE_CODE",
    ] {
        std::env::remove_var(name);
    }

    let installation = crate::roles::EnvEngineProvider::new()
        .installation()
        .expect("the environment declares an installation");
    // The fingerprint is the engine's own report, not a constant that would keep
    // claiming the same build after an upgrade.
    assert_eq!(installation.build_fingerprint, "vllm 0.29.0");

    let host = host_policy(&installation, "env-1", CAPACITY);
    let deployment = deployment_document(
        "m",
        "m",
        &local(models.join("m").to_str().expect("a utf-8 path")),
        CAPACITY,
    );
    let resolved = mllm_config::effective::resolve_effective(&deployment, &host)
        .expect("the published table resolves");

    let ProfileLaunchSettings::Vllm(settings) = &resolved.profile.launch_settings else {
        panic!("the installation is vLLM, so its launch settings are vLLM's");
    };
    assert_eq!(settings.tensor_parallel_size, 1);
    assert_eq!(settings.pipeline_parallel_size, 1);
    assert_eq!(settings.kv_cache_dtype, "auto");
    assert_eq!(settings.block_size_tokens, 16);
    assert_eq!(settings.requested_budget.kv_cache_bytes, 16 << 30);
    assert_eq!(settings.requested_budget.swap_space_bytes, 0);
    assert_eq!(settings.requested_budget.gpu_utilization_pct, 10);
    assert!(
        settings.enable_sleep_mode,
        "deep park is on unless the host switches it off, and sleep mode is what \
         makes it possible"
    );
    assert_eq!(resolved.profile.security.deep_park, DeepPark::Enabled);
    assert!(!resolved.profile.security.trust_remote_code);
    assert_eq!(resolved.profile.args, ["--max-model-len", "4096"]);
    assert_eq!(resolved.profile.executable, bin.to_string_lossy());
    assert_eq!(resolved.host.model_store, models);
    assert_eq!(
        resolved.model.require_resolved_path().expect("a local model"),
        models.join("m").to_string_lossy()
    );

    std::env::remove_var("MLLM_VLLM_BIN");
    std::env::remove_var("MLLM_MODELS_ROOT");
    std::env::remove_var("MLLM_RUNTIME_DIR");
}

/// The switch is the host's, and it is off only when the host says so.
#[test]
fn deep_park_is_switched_off_only_by_the_host_saying_so() {
    let _guard = ENVIRONMENT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = tempfile::TempDir::new().expect("a temporary installation");
    let bin = fake_engine_bin(dir.path());
    std::env::set_var("MLLM_VLLM_BIN", &bin);
    std::env::set_var("MLLM_MODELS_ROOT", dir.path());
    std::env::set_var(
        "MLLM_RUNTIME_DIR",
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime"),
    );
    std::env::set_var("MLLM_DEEP_PARK", "off");
    std::env::set_var("MLLM_TRUST_REMOTE_CODE", "1");

    let installation = crate::roles::EnvEngineProvider::new()
        .installation()
        .expect("the environment declares an installation");
    assert!(!installation.deep_park);
    assert!(installation.trust_remote_code);
    assert_eq!(installation.launch_settings["enable_sleep_mode"], false);

    for name in [
        "MLLM_VLLM_BIN",
        "MLLM_MODELS_ROOT",
        "MLLM_RUNTIME_DIR",
        "MLLM_DEEP_PARK",
        "MLLM_TRUST_REMOTE_CODE",
    ] {
        std::env::remove_var(name);
    }
}
