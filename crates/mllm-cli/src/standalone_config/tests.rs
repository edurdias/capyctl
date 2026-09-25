use super::*;

use std::sync::Mutex;

use mllm_config::effective::{DeepPark, ModelSource};
use mllm_config::engine_policy::Engine;
use mllm_controller::{EngineInstallation, EngineProvider as _};
use mllm_domain::launch::{LaunchSettings, SettingSource};

const CAPACITY: i64 = 128 * 1024 * 1024 * 1024;

/// The environment is process-wide, so the tests that read it take turns. Running
/// them concurrently would let one test's exports decide another's result.
static ENVIRONMENT: Mutex<()> = Mutex::new(());

/// Names the one test a child copy of this binary runs (see [`isolated`]).
const ISOLATED_TEST: &str = "MLLM_ISOLATED_TEST";

/// The environment is process-wide, and so is every other test in this binary:
/// one that reads `MLLM_*` or `HOME` while another exports it, or forks while
/// another has an engine script open for writing, sees the other's state. A
/// test that changes the environment therefore runs alone, in a child copy of
/// this test binary, and never changes the parent's environment. In the parent
/// this runs `test` there and returns `None` (the caller returns); in the child
/// it returns the lock, and the caller's body runs.
fn isolated(test: &str) -> Option<std::sync::MutexGuard<'static, ()>> {
    if std::env::var_os(ISOLATED_TEST).is_some_and(|named| named == test) {
        return Some(
            ENVIRONMENT
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
    }
    let output = std::process::Command::new(std::env::current_exe().expect("the test binary"))
        .args([
            &format!("standalone_config::tests::{test}"),
            "--exact",
            "--test-threads=1",
        ])
        .env(ISOLATED_TEST, test)
        .output()
        .expect("the isolated test runs");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "{test} in its own process:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    None
}

/// An installation with nothing interesting in it, for the tests that are about the
/// shape of the published table rather than about any particular engine.
fn installed(engine: Engine, executable: &str) -> EngineInstallation {
    EngineInstallation {
        engine,
        executable: executable.into(),
        build_fingerprint: "fp-1".into(),
        engine_config: mllm_testkit::vllm_engine_config_json(),
        deep_park: false,
        trust_remote_code: false,
        models_root: "/srv/models".into(),
        runtime_dir: "/opt/mllm/runtime".into(),
        args: Vec::new(),
        installation_drift: Default::default(),
        engine_ports: (8100, 8199),
    }
}

/// The one environment installation, published as `local`.
fn named(
    installation: &EngineInstallation,
) -> Vec<mllm_controller::engine_provider::NamedInstallation> {
    vec![mllm_controller::engine_provider::NamedInstallation {
        profile: STANDALONE_PROFILE.into(),
        installation: installation.clone(),
    }]
}

fn local(path: &str) -> ModelSource {
    ModelSource::Local { path: path.into() }
}

/// Standalone must declare an engine installation, because a deployment can only be
/// qualified against a profile the host has published. Declaring none is what made
/// every start refuse.
#[test]
fn the_host_declares_exactly_one_engine_installation() {
    let host = host_policy(
        &[mllm_controller::engine_provider::NamedInstallation {
            profile: "local".into(),
            installation: installed(Engine::Vllm, "/bin/true"),
        }],
        "env-1",
        CAPACITY,
        None,
    );
    let profiles = host["runtime_profiles"]
        .as_object()
        .expect("profiles object");
    assert_eq!(profiles.len(), 1, "one installation, named not anonymous");
    let profile = &profiles[STANDALONE_PROFILE];
    assert_eq!(profile["engine"], "vllm");
    assert_eq!(profile["executable"], "/bin/true");
    assert_eq!(profile["build_fingerprint"], "fp-1");
}

/// Deep-park paths are the host's decision, not the adapter's (SPEC §9.1, T21), so
/// the profile must carry the host's switch, including an opt-out, rather than
/// leaving it to be re-derived.
// T21
#[test]
fn the_deep_park_switch_is_carried_by_the_profile() {
    for allowed in [false, true] {
        let mut installation = installed(Engine::Vllm, "/opt/vllm");
        installation.deep_park = allowed;
        let host = host_policy(&named(&installation), "env-1", CAPACITY, None);
        assert_eq!(
            host["runtime_profiles"][STANDALONE_PROFILE]["security"]["deep_park"],
            if allowed { "enabled" } else { "disabled" }
        );
    }
}

/// SPEC §9.1 / §13.3, ADR 0012: an embedded vLLM launch seals an admin key
/// apart from its inference key, as SGLang does, so the published profile names
/// both references for every engine family, and the two differ.
// T21 T37
#[test]
fn every_engine_profile_names_distinct_inference_and_admin_references() {
    for engine in [Engine::Vllm, Engine::Sglang] {
        let host = host_policy(
            &named(&installed(engine, "/opt/engine")),
            "env-1",
            CAPACITY,
            None,
        );
        let security = &host["runtime_profiles"][STANDALONE_PROFILE]["security"];
        assert_eq!(
            security["credential_ref"], "secret://engine-key",
            "{engine:?}"
        );
        assert_eq!(
            security["admin_credential_ref"], "secret://admin-key",
            "{engine:?}"
        );
    }
}

/// Spec §3: running Python that arrived inside a checkpoint is the host's decision
/// too, and it is a separate one from parking.
#[test]
fn trusting_checkpoint_code_is_published_separately_from_deep_park() {
    let mut installation = installed(Engine::Vllm, "/opt/vllm");
    installation.trust_remote_code = true;
    let host = host_policy(&named(&installation), "env-1", CAPACITY, None);
    let security = &host["runtime_profiles"][STANDALONE_PROFILE]["security"];
    assert_eq!(security["trust_remote_code"], true);
    assert_eq!(security["deep_park"], "disabled");
}

/// ADR 0008 (owner decision 2026-09-23): the standalone host's
/// `installation_drift` policy reaches the resolved profile a launch is gated
/// on; the default `warn` leaves the published document unchanged.
// T21 T22
#[test]
fn the_installation_drift_policy_is_carried_by_the_profile() {
    use mllm_config::effective::InstallationDrift;
    for policy in [InstallationDrift::Warn, InstallationDrift::Refuse] {
        let mut installation = installed(Engine::Vllm, "/opt/vllm");
        installation.installation_drift = policy;
        let host = host_policy(&named(&installation), "env-1", CAPACITY, None);
        let security = &host["runtime_profiles"][STANDALONE_PROFILE]["security"];
        match policy {
            InstallationDrift::Warn => assert!(security.get("installation_drift").is_none()),
            InstallationDrift::Refuse => assert_eq!(security["installation_drift"], "refuse"),
        }
        let deployment = deployment_document(
            "d",
            "d",
            &local("/srv/models/d"),
            Engine::Vllm,
            CAPACITY,
            DEFAULT_REQUEST_DEADLINE,
            false,
            "local",
        );
        let mut deployment = deployment;
        deployment["engine_config"] = installation.engine_config.clone();
        let effective = mllm_config::effective::resolve_effective(&deployment, &host).unwrap();
        assert_eq!(effective.profile.security.installation_drift, policy);
    }
}

/// Spec §7: a relative model path resolves against the store the host names, so the
/// published table has to carry the installation's own store rather than a default.
#[test]
fn the_published_host_names_the_store_its_weights_live_under() {
    let mut installation = installed(Engine::Vllm, "/opt/vllm");
    installation.models_root = "/data/checkpoints".into();
    let host = host_policy(&named(&installation), "env-1", CAPACITY, None);
    assert_eq!(host["model_store"]["path"], "/data/checkpoints");
}

/// Limits come from observed capacity. An invented ceiling is how a host gets
/// overcommitted, so a larger machine must yield larger limits.
#[test]
fn limits_scale_with_observed_capacity() {
    let installation = installed(Engine::Vllm, "/bin/true");
    let small = host_policy(&named(&installation), "env-1", 16 << 30, None);
    let large = host_policy(&named(&installation), "env-1", 128 << 30, None);
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
    let host = host_policy(
        &[mllm_controller::engine_provider::NamedInstallation {
            profile: "local".into(),
            installation: installed(Engine::Vllm, "/bin/true"),
        }],
        "env-1",
        CAPACITY,
        None,
    );
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
    let d = deployment_document(
        "m",
        "m",
        &local("/models/m"),
        Engine::Vllm,
        CAPACITY,
        DEFAULT_REQUEST_DEADLINE,
        true,
        "local",
    );
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
    assert!(
        bytes("cold") > bytes("ready"),
        "loading costs more than serving"
    );
    assert!(bytes("wake") > bytes("ready"));
    assert!(
        bytes("parked") < bytes("ready"),
        "parked retains only residue"
    );
}

/// A parked deployment holds no device; that is what makes parking reclaim anything.
#[test]
fn a_parked_deployment_holds_no_device() {
    let d = deployment_document(
        "m",
        "m",
        &local("/models/m"),
        Engine::Vllm,
        CAPACITY,
        DEFAULT_REQUEST_DEADLINE,
        true,
        "local",
    );
    assert_eq!(
        d["resources"]["parked"]["devices"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        d["resources"]["ready"]["devices"].as_array().unwrap().len(),
        1
    );
}

/// The deployment must name the profile it runs on, or there is nothing to qualify
/// it against.
#[test]
fn the_deployment_names_its_installation() {
    let d = deployment_document(
        "m",
        "route-m",
        &local("/models/m"),
        Engine::Vllm,
        CAPACITY,
        DEFAULT_REQUEST_DEADLINE,
        true,
        "local",
    );
    assert_eq!(d["runtime_profile"], STANDALONE_PROFILE);
    assert_eq!(d["routes"][0], "route-m");
}

/// Spec §7: the deployment states where its weights come from as a source, so a
/// checkpoint that has to be fetched is expressible in the same document.
#[test]
fn the_deployment_states_its_model_source() {
    let d = deployment_document(
        "m",
        "m",
        &local("/models/m"),
        Engine::Vllm,
        CAPACITY,
        DEFAULT_REQUEST_DEADLINE,
        true,
        "local",
    );
    assert_eq!(d["model"]["source"]["type"], "local");
    assert_eq!(d["model"]["source"]["path"], "/models/m");

    let fetched = deployment_document(
        "m",
        "m",
        &ModelSource::HuggingFace {
            repo: "org/model".into(),
            revision: "0123456789abcdef0123456789abcdef01234567".into(),
            files: vec![],
            token_ref: None,
        },
        Engine::Vllm,
        CAPACITY,
        DEFAULT_REQUEST_DEADLINE,
        true,
        "local",
    );
    assert_eq!(fetched["model"]["source"]["type"], "huggingface");
    assert_eq!(fetched["model"]["source"]["repo"], "org/model");
}

/// The hardware standalone runs on has one physical pool, which is what the domain
/// name has always claimed and nothing has ever stated. Declaring it is what makes
/// a host-backed park refusable rather than silently useless.
#[test]
fn the_published_host_declares_one_memory_pool() {
    let host = host_policy(
        &[mllm_controller::engine_provider::NamedInstallation {
            profile: "local".into(),
            installation: installed(Engine::Vllm, "/bin/true"),
        }],
        "env-1",
        1 << 40,
        None,
    );
    assert_eq!(
        host["resource_policy"]["domains"]["unified"]["memory"],
        "unified"
    );
}

/// ADR 0012, SPEC §6.2: a standalone vLLM deployment on a deep-parking host is
/// `deep`, as a server-mode one is, so it launches in sleep mode and parks
/// instead of being stopped cold by idle eviction or a switch. Resolving it
/// against the published host proves the profile accepts that tier and that
/// sleep mode is derived from it rather than declared.
// T21
#[test]
fn a_standalone_vllm_deployment_deep_parks_when_the_host_does() {
    let mut installation = installed(Engine::Vllm, "/opt/vllm/bin/vllm");
    installation.deep_park = true;
    let host = host_policy(&named(&installation), "env-1", CAPACITY, None);
    let deployment = deployment_document(
        "m",
        "m",
        &local("/models/m"),
        Engine::Vllm,
        CAPACITY,
        DEFAULT_REQUEST_DEADLINE,
        true,
        "local",
    );
    assert_eq!(deployment["residency"], "deep");
    let resolved = mllm_config::effective::resolve_effective(&deployment, &host)
        .expect("a deep vLLM deployment resolves on a deep-parking host");
    assert_eq!(resolved.residency, mllm_config::effective::Residency::Deep);
    let LaunchSettings::Vllm(settings) = &resolved.engine_config else {
        panic!("a vLLM installation resolves vLLM settings");
    };
    assert!(
        settings.enable_sleep_mode,
        "sleep mode follows the deep tier"
    );

    // SPEC §6.2: the opted-out host declares restart_only and launches without
    // sleep mode.
    installation.deep_park = false;
    let host = host_policy(&named(&installation), "env-1", CAPACITY, None);
    let deployment = deployment_document(
        "m",
        "m",
        &local("/models/m"),
        Engine::Vllm,
        CAPACITY,
        DEFAULT_REQUEST_DEADLINE,
        false,
        "local",
    );
    assert_eq!(deployment["residency"], "restart_only");
    let resolved = mllm_config::effective::resolve_effective(&deployment, &host)
        .expect("an opted-out vLLM host's deployment resolves");
    let LaunchSettings::Vllm(settings) = &resolved.engine_config else {
        panic!("a vLLM installation resolves vLLM settings");
    };
    assert!(
        !settings.enable_sleep_mode,
        "no sleep mode without deep parking"
    );
}

/// A copy of mllm's runtime modules the way a prepared installation carries
/// them (SPEC §9.1 / T21): this user's, not group- or other-writable, whatever
/// the umask of the checkout they come from.
fn private_runtime(dir: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../runtime");
    let runtime = dir.join("runtime");
    std::fs::create_dir_all(&runtime).expect("a runtime directory");
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).expect("mode");
    for entry in std::fs::read_dir(&source).expect("the checkout runtime") {
        let path = entry.expect("an entry").path();
        if path.extension().is_some_and(|extension| extension == "py") {
            let copy = runtime.join(path.file_name().expect("a name"));
            std::fs::copy(&path, &copy).expect("a module copy");
            std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o644)).expect("mode");
        }
    }
    runtime
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

/// Spec §7: the host policy built from the environment carries the model store,
/// the deep-park switch and the flags the profile passes, and the deployment
/// standalone generates carries the engine configuration (ADR 0014 §1) — and
/// together they resolve, which is what a deployment is qualified
/// against. A table that merely looked complete but did not resolve would refuse
/// every start at the point where the refusal is hardest to read.
#[test]
fn host_policy_from_env_is_complete() {
    let Some(_guard) = isolated("host_policy_from_env_is_complete") else {
        return;
    };
    let dir = tempfile::TempDir::new().expect("a temporary installation");
    let bin = fake_engine_bin(dir.path());
    let models = dir.path().join("models");
    std::fs::create_dir_all(&models).expect("a model store");
    std::env::set_var("MLLM_VLLM_BIN", &bin);
    std::env::set_var("MLLM_MODELS_ROOT", &models);
    std::env::set_var("MLLM_RUNTIME_DIR", private_runtime(dir.path()));
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

    let host = host_policy(&named(&installation), "env-1", CAPACITY, None);
    // ADR 0014 §1: the published profile carries no engine tuning.
    assert!(host["runtime_profiles"][STANDALONE_PROFILE]
        .get("launch_settings")
        .is_none());
    let mut deployment = deployment_document(
        "m",
        "m",
        &local(models.join("m").to_str().expect("a utf-8 path")),
        Engine::Vllm,
        CAPACITY,
        DEFAULT_REQUEST_DEADLINE,
        installation.deep_park,
        "local",
    );
    deployment["engine_config"] = installation.engine_config.clone();
    let resolved = mllm_config::effective::resolve_effective(&deployment, &host)
        .expect("the published table resolves");

    let LaunchSettings::Vllm(settings) = &resolved.engine_config else {
        panic!("the installation is vLLM, so its launch settings are vLLM's");
    };
    // The environment's default KV cache; the request is the Ready allocation.
    assert_eq!(settings.memory.kv_cache_bytes, 16 << 30);
    assert_eq!(
        settings.memory.request_bytes,
        resolved.resources.ready.allocations[0].bytes
    );
    assert_eq!(settings.common.kv_cache_dtype, None);
    // SPEC §9.1 / ADR 0012: an unset `MLLM_DEEP_PARK` leaves deep parking on,
    // so the generated vLLM deployment is deep and sleep mode is derived from
    // it (SPEC §6.2), as in server mode.
    assert_eq!(resolved.residency, mllm_config::effective::Residency::Deep);
    assert!(settings.enable_sleep_mode);
    assert_eq!(
        settings.provenance["enable_sleep_mode"],
        SettingSource::Derived
    );
    assert_eq!(resolved.profile.security.deep_park, DeepPark::Enabled);
    assert!(!resolved.profile.security.trust_remote_code);
    // ADR 0014 §5 (owner decision 2026-09-25): no host-fixed context default;
    // the launch fits the context to the KV grant.
    assert!(resolved.profile.args.is_empty());
    assert_eq!(resolved.profile.executable, bin.to_string_lossy());
    assert_eq!(resolved.host.model_store, models);
    assert_eq!(
        resolved
            .model
            .require_resolved_path()
            .expect("a local model"),
        models.join("m").to_string_lossy()
    );

    std::env::remove_var("MLLM_VLLM_BIN");
    std::env::remove_var("MLLM_MODELS_ROOT");
    std::env::remove_var("MLLM_RUNTIME_DIR");
}

/// SPEC §9.1 / ADR 0012: standalone deep parking is on unless the host opts out
/// with `MLLM_DEEP_PARK=off`. SPEC §15.3: any other value is a configuration
/// error, never a silent guess in either direction.
// T21 T03
#[test]
fn deep_park_is_on_unless_the_host_opts_out() {
    let Some(_guard) = isolated("deep_park_is_on_unless_the_host_opts_out") else {
        return;
    };
    let dir = tempfile::TempDir::new().expect("a temporary installation");
    let bin = fake_engine_bin(dir.path());
    std::env::set_var("MLLM_VLLM_BIN", &bin);
    std::env::set_var("MLLM_MODELS_ROOT", dir.path());
    std::env::set_var("MLLM_RUNTIME_DIR", private_runtime(dir.path()));
    std::env::set_var("MLLM_DEEP_PARK", "off");
    std::env::set_var("MLLM_TRUST_REMOTE_CODE", "1");

    for (value, enabled) in [(None, true), (Some("on"), true), (Some("off"), false)] {
        match value {
            Some(value) => std::env::set_var("MLLM_DEEP_PARK", value),
            None => std::env::remove_var("MLLM_DEEP_PARK"),
        }
        let installation = crate::roles::EnvEngineProvider::new()
            .installation()
            .expect("the environment declares an installation");
        assert_eq!(installation.deep_park, enabled, "{value:?}");
        assert!(installation.trust_remote_code);
        assert_eq!(installation.engine_config["memory"]["kv_cache"], "16GiB");
    }
    // An empty export is the shape a mistyped opt-out leaves behind; it must not
    // be read as "unset" and silently leave deep parking on.
    for value in ["", "typo", "ON", "true", "1", "disabled"] {
        std::env::set_var("MLLM_DEEP_PARK", value);
        let refusal = crate::roles::EnvEngineProvider::new()
            .installation()
            .expect_err("an unrecognized value is refused");
        let message = refusal.to_string();
        assert!(message.contains("MLLM_DEEP_PARK"), "{value:?}: {message}");
        assert!(message.contains("off"), "{value:?}: {message}");
    }

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

/// ADR 0008 (owner decision 2026-09-23): `MLLM_INSTALLATION_DRIFT` is the
/// standalone host's drift policy; unset is `warn`. SPEC §15.3: any other
/// value is refused, never guessed.
// T22 T03
#[test]
fn installation_drift_is_warn_unless_the_host_refuses() {
    use mllm_config::effective::InstallationDrift;
    let Some(_guard) = isolated("installation_drift_is_warn_unless_the_host_refuses") else {
        return;
    };
    let dir = tempfile::TempDir::new().expect("a temporary installation");
    let bin = fake_engine_bin(dir.path());
    std::env::set_var("MLLM_VLLM_BIN", &bin);
    std::env::set_var("MLLM_MODELS_ROOT", dir.path());
    std::env::set_var("MLLM_RUNTIME_DIR", private_runtime(dir.path()));
    for (value, policy) in [
        (None, InstallationDrift::Warn),
        (Some("warn"), InstallationDrift::Warn),
        (Some("refuse"), InstallationDrift::Refuse),
    ] {
        match value {
            Some(value) => std::env::set_var("MLLM_INSTALLATION_DRIFT", value),
            None => std::env::remove_var("MLLM_INSTALLATION_DRIFT"),
        }
        let installation = crate::roles::EnvEngineProvider::new()
            .installation()
            .expect("the environment declares an installation");
        assert_eq!(installation.installation_drift, policy, "{value:?}");
    }
    for value in ["", "Refuse", "off", "deny"] {
        std::env::set_var("MLLM_INSTALLATION_DRIFT", value);
        let message = crate::roles::EnvEngineProvider::new()
            .installation()
            .expect_err("an unrecognized value is refused")
            .to_string();
        assert!(
            message.contains("MLLM_INSTALLATION_DRIFT"),
            "{value:?}: {message}"
        );
    }
    for name in [
        "MLLM_VLLM_BIN",
        "MLLM_MODELS_ROOT",
        "MLLM_RUNTIME_DIR",
        "MLLM_INSTALLATION_DRIFT",
    ] {
        std::env::remove_var(name);
    }
}

/// SPEC §3, §15.2: the engines' loopback port range is 8100-8199 unless this
/// run names another, which the published host's resource policy then carries;
/// SPEC §15.3: a malformed range is refused.
// T03
#[test]
fn the_engine_port_range_can_be_named_for_one_run() {
    let Some(_guard) = isolated("the_engine_port_range_can_be_named_for_one_run") else {
        return;
    };
    let dir = tempfile::TempDir::new().expect("a temporary installation");
    let bin = fake_engine_bin(dir.path());
    std::env::set_var("MLLM_VLLM_BIN", &bin);
    std::env::set_var("MLLM_MODELS_ROOT", dir.path());
    std::env::set_var("MLLM_RUNTIME_DIR", private_runtime(dir.path()));
    for (value, ports) in [(None, (8100, 8199)), (Some("20100-20107"), (20100, 20107))] {
        match value {
            Some(value) => std::env::set_var(crate::roles::ENGINE_PORTS_ENV, value),
            None => std::env::remove_var(crate::roles::ENGINE_PORTS_ENV),
        }
        let installation = crate::roles::EnvEngineProvider::new()
            .installation()
            .expect("the environment declares an installation");
        assert_eq!(installation.engine_ports, ports, "{value:?}");
        let host = host_policy(&named(&installation), "env-1", CAPACITY, None);
        let range = &host["resource_policy"]["endpoint_port_range"];
        assert_eq!(
            (range["start"].as_u64(), range["end"].as_u64()),
            (Some(ports.0.into()), Some(ports.1.into()))
        );
    }
    for value in ["", "8100", "8199-8100", "80-90", "8100-70000", "a-b"] {
        std::env::set_var(crate::roles::ENGINE_PORTS_ENV, value);
        let message = crate::roles::EnvEngineProvider::new()
            .installation()
            .expect_err("a malformed range is refused")
            .to_string();
        assert!(
            message.contains(crate::roles::ENGINE_PORTS_ENV),
            "{value:?}: {message}"
        );
    }
    for name in [
        "MLLM_VLLM_BIN",
        "MLLM_MODELS_ROOT",
        "MLLM_RUNTIME_DIR",
        crate::roles::ENGINE_PORTS_ENV,
    ] {
        std::env::remove_var(name);
    }
}

/// ADR 0008: with deep parking on, a parking vLLM deployment renders sleep
/// mode, whose entry imports the capability probes, so a runtime directory
/// without `engine_capabilities.py` is refused at boot; an opted-out vLLM host
/// never imports them.
// T21 T22 T37
#[test]
fn the_capability_probe_is_required_when_vllm_may_sleep() {
    let Some(_guard) = isolated("the_capability_probe_is_required_when_vllm_may_sleep") else {
        return;
    };
    let dir = tempfile::TempDir::new().expect("a temporary installation");
    let bin = fake_engine_bin(dir.path());
    let runtime = private_runtime(dir.path());
    std::fs::remove_file(runtime.join("engine_capabilities.py")).expect("the probe copy");
    std::env::set_var("MLLM_VLLM_BIN", &bin);
    std::env::set_var("MLLM_MODELS_ROOT", dir.path());
    std::env::set_var("MLLM_RUNTIME_DIR", &runtime);
    std::env::remove_var("MLLM_DEEP_PARK");
    let message = crate::roles::EnvEngineProvider::new()
        .installation()
        .expect_err("deep parking on needs the probes")
        .to_string();
    assert!(message.contains("engine_capabilities.py"), "{message}");
    std::env::set_var("MLLM_DEEP_PARK", "off");
    crate::roles::EnvEngineProvider::new()
        .installation()
        .expect("an opted-out vLLM host never imports the probes");
    for name in [
        "MLLM_VLLM_BIN",
        "MLLM_MODELS_ROOT",
        "MLLM_RUNTIME_DIR",
        "MLLM_DEEP_PARK",
    ] {
        std::env::remove_var(name);
    }
}

/// ADR 0012: deep parking is on by default and a host opts out. SPEC §6.2:
/// restart_only is a first-class residency, so an SGLang host that opts out must
/// get a restart_only deployment that still resolves, rather than a deep
/// deployment its own profile then refuses. The residency follows the host's
/// switch; it is not a constant of the engine.
// T21 T22
#[test]
fn an_sglang_host_that_opts_out_of_deep_park_deploys_restart_only() {
    let Some(_guard) = isolated("an_sglang_host_that_opts_out_of_deep_park_deploys_restart_only")
    else {
        return;
    };
    let dir = tempfile::TempDir::new().expect("a temporary installation");
    let bin = fake_engine_bin(dir.path());
    let models = dir.path().join("models");
    std::fs::create_dir_all(&models).expect("a model store");
    for name in [
        "MLLM_VLLM_BIN",
        "MLLM_KV_CACHE_BYTES",
        "MLLM_ENGINE_ARGS",
        "MLLM_TRUST_REMOTE_CODE",
    ] {
        std::env::remove_var(name);
    }
    std::env::set_var("MLLM_SGLANG_BIN", &bin);
    std::env::set_var("MLLM_ENGINE_FINGERPRINT", "sglang 0.5.0");
    std::env::set_var("MLLM_MODELS_ROOT", &models);
    std::env::set_var("MLLM_RUNTIME_DIR", private_runtime(dir.path()));
    std::env::set_var("MLLM_DEEP_PARK", "off");

    let installation = crate::roles::EnvEngineProvider::new()
        .installation()
        .expect("the environment declares an installation");
    assert_eq!(installation.engine, Engine::Sglang);
    assert!(!installation.deep_park, "the host opted out");

    let host = host_policy(&named(&installation), "env-1", CAPACITY, None);
    let mut deployment = deployment_document(
        "m",
        "m",
        &local(models.join("m").to_str().expect("a utf-8 path")),
        installation.engine,
        CAPACITY,
        DEFAULT_REQUEST_DEADLINE,
        installation.deep_park,
        "local",
    );
    deployment["engine_config"] = installation.engine_config.clone();
    assert_eq!(deployment["residency"], "restart_only");
    let resolved = mllm_config::effective::resolve_effective(&deployment, &host);

    for name in [
        "MLLM_SGLANG_BIN",
        "MLLM_ENGINE_FINGERPRINT",
        "MLLM_MODELS_ROOT",
        "MLLM_RUNTIME_DIR",
        "MLLM_DEEP_PARK",
    ] {
        std::env::remove_var(name);
    }

    let resolved = resolved.expect("an opted-out SGLang host's deployment resolves");
    assert_eq!(resolved.profile.security.deep_park, DeepPark::Disabled);
    assert!(matches!(resolved.engine_config, LaunchSettings::Sglang(_)));
}

/// ADR 0012: the residency follows the host's deep-park switch for every
/// engine, so standalone and server mode park alike. SGLang keeps the deep
/// residency its memory saver delivers (ADR 0014 §4) and vLLM the one its
/// sleep mode delivers; an opted-out host is restart_only (SPEC §6.2).
// T21
#[test]
fn the_residency_follows_the_deep_park_switch_for_every_engine() {
    let residency = |engine, deep_park| {
        deployment_document(
            "m",
            "m",
            &local("/models/m"),
            engine,
            CAPACITY,
            DEFAULT_REQUEST_DEADLINE,
            deep_park,
            "local",
        )["residency"]
            .clone()
    };
    assert_eq!(residency(Engine::Sglang, true), "deep");
    assert_eq!(residency(Engine::Sglang, false), "restart_only");
    assert_eq!(residency(Engine::Vllm, true), "deep");
    assert_eq!(residency(Engine::Vllm, false), "restart_only");
}

/// SPEC §3.3 / ADR 0001 (owner decision 2026-09-24): without
/// `MLLM_RUNTIME_DIR`, standalone runs from the runtime embedded in the binary,
/// written to its managed directory; with it, the named directory is used and
/// the managed one is never created. A run with neither refuses.
// T21 T37
#[test]
fn standalone_runs_from_the_embedded_runtime_unless_one_is_named() {
    use std::os::unix::fs::PermissionsExt;
    let Some(_guard) = isolated("standalone_runs_from_the_embedded_runtime_unless_one_is_named")
    else {
        return;
    };
    let dir = tempfile::TempDir::new().expect("a temporary installation");
    let bin = fake_engine_bin(dir.path());
    std::env::set_var("MLLM_VLLM_BIN", &bin);
    std::env::set_var("MLLM_MODELS_ROOT", dir.path());
    std::env::remove_var("MLLM_RUNTIME_DIR");
    std::env::remove_var("MLLM_DEEP_PARK");

    let managed = dir.path().join("state").join("runtime");
    let installation = crate::roles::EnvEngineProvider::with_managed_runtime(managed.clone())
        .installation()
        .expect("the embedded runtime is materialized");
    assert_eq!(
        installation.runtime_dir,
        managed.canonicalize().expect("created")
    );
    assert_eq!(
        std::fs::metadata(&managed)
            .expect("created")
            .permissions()
            .mode()
            & 0o7777,
        0o700
    );
    for file in mllm_agent::embedded_runtime::files() {
        assert_eq!(
            std::fs::read(managed.join(file.name)).expect("a module"),
            file.contents
        );
    }

    let named = private_runtime(dir.path());
    let other = dir.path().join("other-state").join("runtime");
    std::env::set_var("MLLM_RUNTIME_DIR", &named);
    let installation = crate::roles::EnvEngineProvider::with_managed_runtime(other.clone())
        .installation()
        .expect("the named runtime is used");
    assert_eq!(
        installation.runtime_dir,
        named.canonicalize().expect("exists")
    );
    assert!(
        !other.exists(),
        "a named runtime leaves the managed one alone"
    );
    assert!(!named.join(mllm_agent::embedded_runtime::MARKER).exists());

    std::env::remove_var("MLLM_RUNTIME_DIR");
    let message = crate::roles::EnvEngineProvider::new()
        .installation()
        .expect_err("no managed directory and none named")
        .to_string();
    assert!(message.contains("MLLM_RUNTIME_DIR"), "{message}");
    for name in ["MLLM_VLLM_BIN", "MLLM_MODELS_ROOT"] {
        std::env::remove_var(name);
    }
}

fn engine_env(dir: &std::path::Path, vllm: bool, sglang: bool) {
    let models = dir.join("models");
    std::fs::create_dir_all(&models).unwrap();
    std::env::set_var("MLLM_MODELS_ROOT", &models);
    std::env::set_var("MLLM_RUNTIME_DIR", private_runtime(dir));
    std::env::set_var("MLLM_ENGINE_FINGERPRINT", "0.29.0");
    let bin = fake_engine_bin(dir);
    if vllm {
        std::env::set_var("MLLM_VLLM_BIN", &bin)
    } else {
        std::env::remove_var("MLLM_VLLM_BIN")
    }
    if sglang {
        std::env::set_var("MLLM_SGLANG_BIN", bin.with_file_name("python3"))
    } else {
        std::env::remove_var("MLLM_SGLANG_BIN")
    }
    for name in [
        "MLLM_KV_CACHE_BYTES",
        "MLLM_ENGINE_ARGS",
        "MLLM_DEEP_PARK",
        "MLLM_TRUST_REMOTE_CODE",
    ] {
        std::env::remove_var(name);
    }
}

fn names(found: &[mllm_controller::engine_provider::NamedInstallation]) -> Vec<&str> {
    found.iter().map(|n| n.profile.as_str()).collect()
}

fn registered(executable: &std::path::Path) -> serde_json::Map<String, serde_json::Value> {
    let profile =
        mllm_config::registration::profile_document(&mllm_config::registration::ProfileSpec {
            engine: Engine::Vllm,
            executable: executable.into(),
            build_fingerprint: "0.29.0".into(),
            deep_park: true,
            installation_drift: mllm_config::effective::InstallationDrift::Warn,
            args: vec![],
        });
    [("vllm-patched".to_string(), profile)]
        .into_iter()
        .collect()
}

// ADR 0018 §5: one variable gives `local`, exactly as before.
// T01 T07
#[test]
fn one_variable_gives_local() {
    let Some(_guard) = isolated("one_variable_gives_local") else {
        return;
    };
    let dir = tempfile::TempDir::new().unwrap();
    engine_env(dir.path(), true, false);
    let found = crate::roles::EnvEngineProvider::new()
        .installations(&Default::default())
        .unwrap();
    assert_eq!(names(&found), vec!["local"]);
}

// ADR 0018 §5: both variables (refused before) give two profiles.
// T01 T07
#[test]
fn both_variables_give_local_vllm_and_local_sglang() {
    let Some(_guard) = isolated("both_variables_give_local_vllm_and_local_sglang") else {
        return;
    };
    let dir = tempfile::TempDir::new().unwrap();
    engine_env(dir.path(), true, true);
    let found = crate::roles::EnvEngineProvider::new()
        .installations(&Default::default())
        .unwrap();
    assert_eq!(names(&found), vec!["local-vllm", "local-sglang"]);
    assert_eq!(found[1].installation.engine, Engine::Sglang);
}

// ADR 0018 §5: registered profiles coexist; a collision is profile_exists;
// a registered profile alone is enough.
// T03 T07
#[test]
fn registered_profiles_coexist_and_collide_by_name() {
    let Some(_guard) = isolated("registered_profiles_coexist_and_collide_by_name") else {
        return;
    };
    let dir = tempfile::TempDir::new().unwrap();
    engine_env(dir.path(), true, false);
    let bin = dir.path().join("venv/bin/vllm");
    let found = crate::roles::EnvEngineProvider::new()
        .installations(&registered(&bin))
        .unwrap();
    assert_eq!(names(&found), vec!["local", "vllm-patched"]);
    let mut clash = registered(&bin);
    clash.insert("local".into(), clash["vllm-patched"].clone());
    assert!(matches!(
        crate::roles::EnvEngineProvider::new().installations(&clash),
        Err(mllm_controller::engine_provider::ProviderError::ProfileExists(name)) if name == "local"
    ));
    engine_env(dir.path(), false, false);
    let alone = crate::roles::EnvEngineProvider::new()
        .installations(&registered(&bin))
        .unwrap();
    assert_eq!(names(&alone), vec!["vllm-patched"]);
    assert!(crate::roles::EnvEngineProvider::new()
        .installations(&Default::default())
        .is_err());
}

// The isolation itself: what an environment test exports stays in its own
// process, so no other test in this binary can see it.
#[test]
fn an_isolated_test_leaves_this_process_environment_alone() {
    if let Some(_guard) = isolated("an_isolated_test_leaves_this_process_environment_alone") {
        std::env::set_var("MLLM_ISOLATION_PROBE", "child");
        return;
    }
    assert!(std::env::var_os("MLLM_ISOLATION_PROBE").is_none());
}
