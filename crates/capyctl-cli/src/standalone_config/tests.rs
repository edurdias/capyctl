use super::*;

use std::sync::Mutex;

use capyctl_config::effective::{DeepPark, ModelSource};
use capyctl_config::engine_policy::Engine;
use capyctl_controller::{EngineInstallation, EngineProvider as _};
use capyctl_domain::launch::{LaunchSettings, SettingSource};

const CAPACITY: i64 = 128 * 1024 * 1024 * 1024;

/// The environment is process-wide, so the tests that read it take turns. Running
/// them concurrently would let one test's exports decide another's result.
static ENVIRONMENT: Mutex<()> = Mutex::new(());

/// Names the one test a child copy of this binary runs (see [`isolated`]).
const ISOLATED_TEST: &str = "CAPYCTL_ISOLATED_TEST";

/// The environment is process-wide, and so is every other test in this binary:
/// one that reads `CAPYCTL_*` or `HOME` while another exports it, or forks while
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
        engine_config: capyctl_testkit::vllm_engine_config_json(),
        kv_cache_declared: false,
        deep_park: false,
        trust_remote_code: false,
        models_root: "/srv/models".into(),
        runtime_dir: "/opt/capyctl/runtime".into(),
        args: Vec::new(),
        installation_drift: Default::default(),
        cuda_home: None,
        engine_ports: (8100, 8199),
    }
}

/// The one environment installation, published as `local`.
fn named(
    installation: &EngineInstallation,
) -> Vec<capyctl_controller::engine_provider::NamedInstallation> {
    vec![capyctl_controller::engine_provider::NamedInstallation {
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
        &[capyctl_controller::engine_provider::NamedInstallation {
            profile: "local".into(),
            installation: installed(Engine::Vllm, "/bin/true"),
        }],
        "env-1",
        CAPACITY,
        None,
        &HostShape::NoGpu,
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

/// SPEC §13.3 amendment (owner decision 2026-09-25): an installation's CUDA
/// toolkit (`CAPYCTL_CUDA_HOME` for the environment one) is published as the
/// profile's `cuda_home`; without one the profile names none.
// T21
#[test]
fn an_installation_cuda_home_is_published_only_when_named() {
    let mut with = installed(Engine::Vllm, "/bin/true");
    with.cuda_home = Some("/usr/local/cuda".into());
    let host = host_policy(
        &[
            capyctl_controller::engine_provider::NamedInstallation {
                profile: "local-vllm".into(),
                installation: with,
            },
            capyctl_controller::engine_provider::NamedInstallation {
                profile: "local-sglang".into(),
                installation: installed(Engine::Sglang, "/bin/true"),
            },
        ],
        "env-1",
        CAPACITY,
        None,
        &HostShape::NoGpu,
    );
    assert_eq!(
        host["runtime_profiles"]["local-vllm"]["cuda_home"],
        "/usr/local/cuda"
    );
    assert!(host["runtime_profiles"]["local-sglang"]
        .get("cuda_home")
        .is_none());
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
        let host = host_policy(
            &named(&installation),
            "env-1",
            CAPACITY,
            None,
            &HostShape::NoGpu,
        );
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
            &HostShape::NoGpu,
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
    let host = host_policy(
        &named(&installation),
        "env-1",
        CAPACITY,
        None,
        &HostShape::NoGpu,
    );
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
    use capyctl_config::effective::InstallationDrift;
    for policy in [InstallationDrift::Warn, InstallationDrift::Refuse] {
        let mut installation = installed(Engine::Vllm, "/opt/vllm");
        installation.installation_drift = policy;
        let host = host_policy(
            &named(&installation),
            "env-1",
            CAPACITY,
            None,
            &HostShape::NoGpu,
        );
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
            &TemplateMemory::Unified {
                capacity_bytes: CAPACITY,
            },
            DEFAULT_REQUEST_DEADLINE,
            false,
            "local",
        )
        .expect("the unified template");
        let mut deployment = deployment;
        deployment["engine_config"] = installation.engine_config.clone();
        let effective = capyctl_config::effective::resolve_effective(&deployment, &host).unwrap();
        assert_eq!(effective.profile.security.installation_drift, policy);
    }
}

/// Spec §7: a relative model path resolves against the store the host names, so the
/// published table has to carry the installation's own store rather than a default.
#[test]
fn the_published_host_names_the_store_its_weights_live_under() {
    let mut installation = installed(Engine::Vllm, "/opt/vllm");
    installation.models_root = "/data/checkpoints".into();
    let host = host_policy(
        &named(&installation),
        "env-1",
        CAPACITY,
        None,
        &HostShape::NoGpu,
    );
    assert_eq!(host["model_store"]["path"], "/data/checkpoints");
}

/// Limits come from observed capacity. An invented ceiling is how a host gets
/// overcommitted, so a larger machine must yield larger limits.
#[test]
fn limits_scale_with_observed_capacity() {
    let installation = installed(Engine::Vllm, "/bin/true");
    let small = host_policy(
        &named(&installation),
        "env-1",
        16 << 30,
        None,
        &HostShape::NoGpu,
    );
    let large = host_policy(
        &named(&installation),
        "env-1",
        128 << 30,
        None,
        &HostShape::NoGpu,
    );
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
        &[capyctl_controller::engine_provider::NamedInstallation {
            profile: "local".into(),
            installation: installed(Engine::Vllm, "/bin/true"),
        }],
        "env-1",
        CAPACITY,
        None,
        &HostShape::NoGpu,
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
        &TemplateMemory::Unified {
            capacity_bytes: CAPACITY,
        },
        DEFAULT_REQUEST_DEADLINE,
        true,
        "local",
    )
    .expect("the unified template");
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
        &TemplateMemory::Unified {
            capacity_bytes: CAPACITY,
        },
        DEFAULT_REQUEST_DEADLINE,
        true,
        "local",
    )
    .expect("the unified template");
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
        &TemplateMemory::Unified {
            capacity_bytes: CAPACITY,
        },
        DEFAULT_REQUEST_DEADLINE,
        true,
        "local",
    )
    .expect("the unified template");
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
        &TemplateMemory::Unified {
            capacity_bytes: CAPACITY,
        },
        DEFAULT_REQUEST_DEADLINE,
        true,
        "local",
    )
    .expect("the unified template");
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
        &TemplateMemory::Unified {
            capacity_bytes: CAPACITY,
        },
        DEFAULT_REQUEST_DEADLINE,
        true,
        "local",
    )
    .expect("the unified template");
    assert_eq!(fetched["model"]["source"]["type"], "huggingface");
    assert_eq!(fetched["model"]["source"]["repo"], "org/model");
}

/// The hardware standalone runs on has one physical pool, which is what the domain
/// name has always claimed and nothing has ever stated. Declaring it is what makes
/// a host-backed park refusable rather than silently useless.
#[test]
fn the_published_host_declares_one_memory_pool() {
    let host = host_policy(
        &[capyctl_controller::engine_provider::NamedInstallation {
            profile: "local".into(),
            installation: installed(Engine::Vllm, "/bin/true"),
        }],
        "env-1",
        1 << 40,
        None,
        &HostShape::NoGpu,
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
    let host = host_policy(
        &named(&installation),
        "env-1",
        CAPACITY,
        None,
        &HostShape::NoGpu,
    );
    let deployment = deployment_document(
        "m",
        "m",
        &local("/models/m"),
        Engine::Vllm,
        &TemplateMemory::Unified {
            capacity_bytes: CAPACITY,
        },
        DEFAULT_REQUEST_DEADLINE,
        true,
        "local",
    )
    .expect("the unified template");
    assert_eq!(deployment["residency"], "deep");
    let resolved = capyctl_config::effective::resolve_effective(&deployment, &host)
        .expect("a deep vLLM deployment resolves on a deep-parking host");
    assert_eq!(
        resolved.residency,
        capyctl_config::effective::Residency::Deep
    );
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
    let host = host_policy(
        &named(&installation),
        "env-1",
        CAPACITY,
        None,
        &HostShape::NoGpu,
    );
    let deployment = deployment_document(
        "m",
        "m",
        &local("/models/m"),
        Engine::Vllm,
        &TemplateMemory::Unified {
            capacity_bytes: CAPACITY,
        },
        DEFAULT_REQUEST_DEADLINE,
        false,
        "local",
    )
    .expect("the unified template");
    assert_eq!(deployment["residency"], "restart_only");
    let resolved = capyctl_config::effective::resolve_effective(&deployment, &host)
        .expect("an opted-out vLLM host's deployment resolves");
    let LaunchSettings::Vllm(settings) = &resolved.engine_config else {
        panic!("a vLLM installation resolves vLLM settings");
    };
    assert!(
        !settings.enable_sleep_mode,
        "no sleep mode without deep parking"
    );
}

/// A copy of capyctl's runtime modules the way a prepared installation carries
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

/// An executable `tensorfold` in a venv `bin` holding `tools`.
fn fake_tensorfold_bin(dir: &std::path::Path, tools: &[&str]) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin_dir = dir.join("tf").join("bin");
    std::fs::create_dir_all(&bin_dir).expect("a bin directory");
    for (name, body) in std::iter::once(("tensorfold", "echo 'tensorfold 0.6.0'"))
        .chain(tools.iter().map(|tool| (*tool, "exit 0")))
    {
        let path = bin_dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("a script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).expect("mode");
    }
    bin_dir.join("tensorfold")
}

/// ADR 0023 §2, §6: a role's own TensorFold is declared by
/// CAPYCTL_TENSORFOLD_BIN, publishes its probed version with deep park off
/// whatever the default says, and is refused without its build toolchain.
// T41
#[test]
fn an_environment_tensorfold_is_checked_and_never_parks() {
    use crate::roles::EngineProvider as _;
    let Some(_guard) = isolated("an_environment_tensorfold_is_checked_and_never_parks") else {
        return;
    };
    let dir = tempfile::TempDir::new().expect("a temporary installation");
    std::env::set_var("CAPYCTL_MODELS_ROOT", dir.path());
    std::env::set_var("CAPYCTL_RUNTIME_DIR", private_runtime(dir.path()));
    std::env::remove_var("CAPYCTL_DEEP_PARK");

    let bin = fake_tensorfold_bin(dir.path(), &["ninja", "nvcc", "c++"]);
    std::env::set_var("CAPYCTL_TENSORFOLD_BIN", &bin);
    let named = crate::roles::EnvEngineProvider::new()
        .installations(&Default::default())
        .expect("the environment declares an installation");
    assert_eq!(named.len(), 1);
    assert_eq!(named[0].profile, "local");
    let installation = &named[0].installation;
    assert_eq!(installation.engine, Engine::Tensorfold);
    assert_eq!(installation.build_fingerprint, "0.6.0");
    assert!(!installation.deep_park);

    std::fs::remove_file(bin.with_file_name("ninja")).expect("remove ninja");
    // No system directory is searched, so the refusal does not depend on what
    // this machine has installed.
    let message = crate::roles::EnvEngineProvider::new()
        .with_toolchain_search(capyctl_config::toolchain::ToolchainSearch {
            system: String::new(),
            default_cuda_home: dir.path().join("no-cuda"),
        })
        .installation()
        .expect_err("a missing build tool refuses the start")
        .to_string();
    assert!(message.contains("toolchain_missing"), "{message}");
    assert!(message.contains("ninja"), "{message}");
    assert!(message.contains("--cuda-home"), "{message}");
    for name in [
        "CAPYCTL_TENSORFOLD_BIN",
        "CAPYCTL_MODELS_ROOT",
        "CAPYCTL_RUNTIME_DIR",
    ] {
        std::env::remove_var(name);
    }
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
    std::env::set_var("CAPYCTL_VLLM_BIN", &bin);
    std::env::set_var("CAPYCTL_MODELS_ROOT", &models);
    std::env::set_var("CAPYCTL_RUNTIME_DIR", private_runtime(dir.path()));
    for name in [
        "CAPYCTL_ENGINE_FINGERPRINT",
        "CAPYCTL_KV_CACHE_BYTES",
        "CAPYCTL_ENGINE_ARGS",
        "CAPYCTL_DEEP_PARK",
        "CAPYCTL_TRUST_REMOTE_CODE",
    ] {
        std::env::remove_var(name);
    }

    let installation = crate::roles::EnvEngineProvider::new()
        .installation()
        .expect("the environment declares an installation");
    // The fingerprint is the engine's own report, not a constant that would keep
    // claiming the same build after an upgrade.
    assert_eq!(installation.build_fingerprint, "vllm 0.29.0");

    let host = host_policy(
        &named(&installation),
        "env-1",
        CAPACITY,
        None,
        &HostShape::NoGpu,
    );
    // ADR 0014 §1: the published profile carries no engine tuning.
    assert!(host["runtime_profiles"][STANDALONE_PROFILE]
        .get("launch_settings")
        .is_none());
    let mut deployment = deployment_document(
        "m",
        "m",
        &local(models.join("m").to_str().expect("a utf-8 path")),
        Engine::Vllm,
        &TemplateMemory::Unified {
            capacity_bytes: CAPACITY,
        },
        DEFAULT_REQUEST_DEADLINE,
        installation.deep_park,
        "local",
    )
    .expect("the unified template");
    deployment["engine_config"] = installation.engine_config.clone();
    let resolved = capyctl_config::effective::resolve_effective(&deployment, &host)
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
    // SPEC §9.1 / ADR 0012: an unset `CAPYCTL_DEEP_PARK` leaves deep parking on,
    // so the generated vLLM deployment is deep and sleep mode is derived from
    // it (SPEC §6.2), as in server mode.
    assert_eq!(
        resolved.residency,
        capyctl_config::effective::Residency::Deep
    );
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

    std::env::remove_var("CAPYCTL_VLLM_BIN");
    std::env::remove_var("CAPYCTL_MODELS_ROOT");
    std::env::remove_var("CAPYCTL_RUNTIME_DIR");
}

/// SPEC §9.1 / ADR 0012: standalone deep parking is on unless the host opts out
/// with `CAPYCTL_DEEP_PARK=off`. SPEC §15.3: any other value is a configuration
/// error, never a silent guess in either direction.
// T21 T03
#[test]
fn deep_park_is_on_unless_the_host_opts_out() {
    let Some(_guard) = isolated("deep_park_is_on_unless_the_host_opts_out") else {
        return;
    };
    let dir = tempfile::TempDir::new().expect("a temporary installation");
    let bin = fake_engine_bin(dir.path());
    std::env::set_var("CAPYCTL_VLLM_BIN", &bin);
    std::env::set_var("CAPYCTL_MODELS_ROOT", dir.path());
    std::env::set_var("CAPYCTL_RUNTIME_DIR", private_runtime(dir.path()));
    std::env::set_var("CAPYCTL_DEEP_PARK", "off");
    std::env::set_var("CAPYCTL_TRUST_REMOTE_CODE", "1");

    for (value, enabled) in [(None, true), (Some("on"), true), (Some("off"), false)] {
        match value {
            Some(value) => std::env::set_var("CAPYCTL_DEEP_PARK", value),
            None => std::env::remove_var("CAPYCTL_DEEP_PARK"),
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
        std::env::set_var("CAPYCTL_DEEP_PARK", value);
        let refusal = crate::roles::EnvEngineProvider::new()
            .installation()
            .expect_err("an unrecognized value is refused");
        let message = refusal.to_string();
        assert!(
            message.contains("CAPYCTL_DEEP_PARK"),
            "{value:?}: {message}"
        );
        assert!(message.contains("off"), "{value:?}: {message}");
    }

    for name in [
        "CAPYCTL_VLLM_BIN",
        "CAPYCTL_MODELS_ROOT",
        "CAPYCTL_RUNTIME_DIR",
        "CAPYCTL_DEEP_PARK",
        "CAPYCTL_TRUST_REMOTE_CODE",
    ] {
        std::env::remove_var(name);
    }
}

/// ADR 0008 (owner decision 2026-09-23): `CAPYCTL_INSTALLATION_DRIFT` is the
/// standalone host's drift policy; unset is `warn`. SPEC §15.3: any other
/// value is refused, never guessed.
// T22 T03
#[test]
fn installation_drift_is_warn_unless_the_host_refuses() {
    use capyctl_config::effective::InstallationDrift;
    let Some(_guard) = isolated("installation_drift_is_warn_unless_the_host_refuses") else {
        return;
    };
    let dir = tempfile::TempDir::new().expect("a temporary installation");
    let bin = fake_engine_bin(dir.path());
    std::env::set_var("CAPYCTL_VLLM_BIN", &bin);
    std::env::set_var("CAPYCTL_MODELS_ROOT", dir.path());
    std::env::set_var("CAPYCTL_RUNTIME_DIR", private_runtime(dir.path()));
    for (value, policy) in [
        (None, InstallationDrift::Warn),
        (Some("warn"), InstallationDrift::Warn),
        (Some("refuse"), InstallationDrift::Refuse),
    ] {
        match value {
            Some(value) => std::env::set_var("CAPYCTL_INSTALLATION_DRIFT", value),
            None => std::env::remove_var("CAPYCTL_INSTALLATION_DRIFT"),
        }
        let installation = crate::roles::EnvEngineProvider::new()
            .installation()
            .expect("the environment declares an installation");
        assert_eq!(installation.installation_drift, policy, "{value:?}");
    }
    for value in ["", "Refuse", "off", "deny"] {
        std::env::set_var("CAPYCTL_INSTALLATION_DRIFT", value);
        let message = crate::roles::EnvEngineProvider::new()
            .installation()
            .expect_err("an unrecognized value is refused")
            .to_string();
        assert!(
            message.contains("CAPYCTL_INSTALLATION_DRIFT"),
            "{value:?}: {message}"
        );
    }
    for name in [
        "CAPYCTL_VLLM_BIN",
        "CAPYCTL_MODELS_ROOT",
        "CAPYCTL_RUNTIME_DIR",
        "CAPYCTL_INSTALLATION_DRIFT",
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
    std::env::set_var("CAPYCTL_VLLM_BIN", &bin);
    std::env::set_var("CAPYCTL_MODELS_ROOT", dir.path());
    std::env::set_var("CAPYCTL_RUNTIME_DIR", private_runtime(dir.path()));
    for (value, ports) in [(None, (8100, 8199)), (Some("20100-20107"), (20100, 20107))] {
        match value {
            Some(value) => std::env::set_var(crate::roles::ENGINE_PORTS_ENV, value),
            None => std::env::remove_var(crate::roles::ENGINE_PORTS_ENV),
        }
        let installation = crate::roles::EnvEngineProvider::new()
            .installation()
            .expect("the environment declares an installation");
        assert_eq!(installation.engine_ports, ports, "{value:?}");
        let host = host_policy(
            &named(&installation),
            "env-1",
            CAPACITY,
            None,
            &HostShape::NoGpu,
        );
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
        "CAPYCTL_VLLM_BIN",
        "CAPYCTL_MODELS_ROOT",
        "CAPYCTL_RUNTIME_DIR",
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
    std::env::set_var("CAPYCTL_VLLM_BIN", &bin);
    std::env::set_var("CAPYCTL_MODELS_ROOT", dir.path());
    std::env::set_var("CAPYCTL_RUNTIME_DIR", &runtime);
    std::env::remove_var("CAPYCTL_DEEP_PARK");
    let message = crate::roles::EnvEngineProvider::new()
        .installation()
        .expect_err("deep parking on needs the probes")
        .to_string();
    assert!(message.contains("engine_capabilities.py"), "{message}");
    std::env::set_var("CAPYCTL_DEEP_PARK", "off");
    crate::roles::EnvEngineProvider::new()
        .installation()
        .expect("an opted-out vLLM host never imports the probes");
    for name in [
        "CAPYCTL_VLLM_BIN",
        "CAPYCTL_MODELS_ROOT",
        "CAPYCTL_RUNTIME_DIR",
        "CAPYCTL_DEEP_PARK",
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
        "CAPYCTL_VLLM_BIN",
        "CAPYCTL_KV_CACHE_BYTES",
        "CAPYCTL_ENGINE_ARGS",
        "CAPYCTL_TRUST_REMOTE_CODE",
    ] {
        std::env::remove_var(name);
    }
    std::env::set_var("CAPYCTL_SGLANG_BIN", &bin);
    std::env::set_var("CAPYCTL_ENGINE_FINGERPRINT", "sglang 0.5.0");
    std::env::set_var("CAPYCTL_MODELS_ROOT", &models);
    std::env::set_var("CAPYCTL_RUNTIME_DIR", private_runtime(dir.path()));
    std::env::set_var("CAPYCTL_DEEP_PARK", "off");

    let installation = crate::roles::EnvEngineProvider::new()
        .installation()
        .expect("the environment declares an installation");
    assert_eq!(installation.engine, Engine::Sglang);
    assert!(!installation.deep_park, "the host opted out");

    let host = host_policy(
        &named(&installation),
        "env-1",
        CAPACITY,
        None,
        &HostShape::NoGpu,
    );
    let mut deployment = deployment_document(
        "m",
        "m",
        &local(models.join("m").to_str().expect("a utf-8 path")),
        installation.engine,
        &TemplateMemory::Unified {
            capacity_bytes: CAPACITY,
        },
        DEFAULT_REQUEST_DEADLINE,
        installation.deep_park,
        "local",
    )
    .expect("the unified template");
    deployment["engine_config"] = installation.engine_config.clone();
    assert_eq!(deployment["residency"], "restart_only");
    let resolved = capyctl_config::effective::resolve_effective(&deployment, &host);

    for name in [
        "CAPYCTL_SGLANG_BIN",
        "CAPYCTL_ENGINE_FINGERPRINT",
        "CAPYCTL_MODELS_ROOT",
        "CAPYCTL_RUNTIME_DIR",
        "CAPYCTL_DEEP_PARK",
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
            &TemplateMemory::Unified {
                capacity_bytes: CAPACITY,
            },
            DEFAULT_REQUEST_DEADLINE,
            deep_park,
            "local",
        )
        .expect("the unified template")["residency"]
            .clone()
    };
    assert_eq!(residency(Engine::Sglang, true), "deep");
    assert_eq!(residency(Engine::Sglang, false), "restart_only");
    assert_eq!(residency(Engine::Vllm, true), "deep");
    assert_eq!(residency(Engine::Vllm, false), "restart_only");
}

/// SPEC §3.3 / ADR 0001 (owner decision 2026-09-24): without
/// `CAPYCTL_RUNTIME_DIR`, standalone runs from the runtime embedded in the binary,
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
    std::env::set_var("CAPYCTL_VLLM_BIN", &bin);
    std::env::set_var("CAPYCTL_MODELS_ROOT", dir.path());
    std::env::remove_var("CAPYCTL_RUNTIME_DIR");
    std::env::remove_var("CAPYCTL_DEEP_PARK");

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
    for file in capyctl_agent::embedded_runtime::files() {
        assert_eq!(
            std::fs::read(managed.join(file.name)).expect("a module"),
            file.contents
        );
    }

    let named = private_runtime(dir.path());
    let other = dir.path().join("other-state").join("runtime");
    std::env::set_var("CAPYCTL_RUNTIME_DIR", &named);
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
    assert!(!named.join(capyctl_agent::embedded_runtime::MARKER).exists());

    std::env::remove_var("CAPYCTL_RUNTIME_DIR");
    let message = crate::roles::EnvEngineProvider::new()
        .installation()
        .expect_err("no managed directory and none named")
        .to_string();
    assert!(message.contains("CAPYCTL_RUNTIME_DIR"), "{message}");
    for name in ["CAPYCTL_VLLM_BIN", "CAPYCTL_MODELS_ROOT"] {
        std::env::remove_var(name);
    }
}

fn engine_env(dir: &std::path::Path, vllm: bool, sglang: bool) {
    let models = dir.join("models");
    std::fs::create_dir_all(&models).unwrap();
    std::env::set_var("CAPYCTL_MODELS_ROOT", &models);
    std::env::set_var("CAPYCTL_RUNTIME_DIR", private_runtime(dir));
    std::env::set_var("CAPYCTL_ENGINE_FINGERPRINT", "0.29.0");
    let bin = fake_engine_bin(dir);
    if vllm {
        std::env::set_var("CAPYCTL_VLLM_BIN", &bin)
    } else {
        std::env::remove_var("CAPYCTL_VLLM_BIN")
    }
    if sglang {
        std::env::set_var("CAPYCTL_SGLANG_BIN", bin.with_file_name("python3"))
    } else {
        std::env::remove_var("CAPYCTL_SGLANG_BIN")
    }
    for name in [
        "CAPYCTL_KV_CACHE_BYTES",
        "CAPYCTL_ENGINE_ARGS",
        "CAPYCTL_DEEP_PARK",
        "CAPYCTL_TRUST_REMOTE_CODE",
    ] {
        std::env::remove_var(name);
    }
}

fn names(found: &[capyctl_controller::engine_provider::NamedInstallation]) -> Vec<&str> {
    found.iter().map(|n| n.profile.as_str()).collect()
}

fn registered(executable: &std::path::Path) -> serde_json::Map<String, serde_json::Value> {
    let profile = capyctl_config::registration::profile_document(
        &capyctl_config::registration::ProfileSpec {
            engine: Engine::Vllm,
            executable: executable.into(),
            build_fingerprint: "0.29.0".into(),
            deep_park: true,
            installation_drift: capyctl_config::effective::InstallationDrift::Warn,
            args: vec![],
            cuda_home: None,
        },
    );
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
        Err(capyctl_controller::engine_provider::ProviderError::ProfileExists(name)) if name == "local"
    ));
    engine_env(dir.path(), false, false);
    let alone = crate::roles::EnvEngineProvider::new()
        .installations(&registered(&bin))
        .unwrap();
    assert_eq!(names(&alone), vec!["vllm-patched"]);
    // T02, SPEC §8 (amended 2026-10-01): nothing declared publishes nothing.
    assert!(crate::roles::EnvEngineProvider::new()
        .installations(&Default::default())
        .unwrap()
        .is_empty());
}

// The isolation itself: what an environment test exports stays in its own
// process, so no other test in this binary can see it.
#[test]
fn an_isolated_test_leaves_this_process_environment_alone() {
    if let Some(_guard) = isolated("an_isolated_test_leaves_this_process_environment_alone") {
        std::env::set_var("CAPYCTL_ISOLATION_PROBE", "child");
        return;
    }
    assert!(std::env::var_os("CAPYCTL_ISOLATION_PROBE").is_none());
}

use capyctl_agent::gpu_memory::{GpuDevice, GpuMemory, HostShape};
const GIB: i64 = 1 << 30;
const MIB: i64 = 1 << 20;

/// The installation the discrete-host tests publish; the unified fixture was
/// captured from `main` with exactly this one.
fn installations() -> Vec<capyctl_controller::engine_provider::NamedInstallation> {
    named(&installed(Engine::Vllm, "/opt/venv/bin/vllm"))
}

fn rtx(index: u32, total_mib: i64, used_mib: i64) -> GpuDevice {
    GpuDevice {
        index,
        uuid: format!("GPU-{index:08}-2222-3333-4444-555555555555"),
        pci_bus_id: format!("00000000:0{index}:00.0"),
        name: "RTX".into(),
        memory: Some(GpuMemory {
            total_bytes: total_mib * MIB,
            used_bytes: used_mib * MIB,
            free_bytes: (total_mib - used_mib) * MIB,
        }),
    }
}

// T26: a 16 GB card with 1.5 GiB of desktop use; 61 GiB of RAM.
#[test]
fn a_discrete_standalone_host_has_system_and_device_domains() {
    let shape = HostShape::Discrete(vec![rtx(0, 16376, 1536)]);
    let doc = host_policy(&installations(), "env", 61 * GIB, None, &shape);
    let domains = &doc["resource_policy"]["domains"];
    assert!(domains.get("unified").is_none());
    assert_eq!(domains["system"]["memory"], "distinct");
    assert_eq!(domains["gpu0"]["memory"], "device");
    assert_eq!(domains["gpu0"]["device"], "gpu0");
    let reserve = (16376 * MIB / 100 * 8).max(GIB);
    assert_eq!(domains["gpu0"]["free_reserve"], format!("{reserve}B"));
    assert_eq!(
        domains["gpu0"]["managed_limit"],
        format!("{}B", 16376 * MIB - reserve)
    );
    assert!(domains["gpu0"].get("host_kv_limit").is_none());
    assert_eq!(doc["resource_policy"]["devices"]["gpu0"]["domain"], "gpu0");
    // The published document is one the host policy accepts (ADR 0019).
    capyctl_config::effective::normalize_host_policy(&doc).expect("a valid discrete host policy");
}

// T26: the unified document is byte-identical to before.
#[test]
fn a_unified_standalone_host_is_unchanged() {
    // Captured from `main` before discrete hosts were published.
    let before = include_str!("fixtures/unified_host_policy.json");
    let doc = host_policy(
        &installations(),
        "env",
        128 * GIB,
        None,
        &HostShape::Unified,
    );
    assert_eq!(
        serde_json::to_string_pretty(&doc).unwrap(),
        before.trim_end()
    );
    let no_gpu = host_policy(&installations(), "env", 128 * GIB, None, &HostShape::NoGpu);
    assert_eq!(no_gpu, doc);
}

// T26 (owner decision 3): two GPUs publish two devices and two device domains.
#[test]
fn two_gpus_publish_two_device_domains() {
    let shape = HostShape::Discrete(vec![rtx(0, 24576, 0), rtx(1, 32768, 0)]);
    let doc = host_policy(&installations(), "env", 64 * GIB, None, &shape);
    assert_eq!(doc["resource_policy"]["devices"]["gpu1"]["domain"], "gpu1");
    assert_eq!(doc["resource_policy"]["domains"]["gpu1"]["device"], "gpu1");
    capyctl_config::effective::normalize_host_policy(&doc).expect("a valid two-GPU host policy");
}

// T26
#[test]
fn device_limits_follow_the_spec_table() {
    let limits = device_limits(&rtx(0, 16376, 0).memory.unwrap(), 4);
    assert_eq!(limits.free_reserve, GIB.max(16376 * MIB / 100 * 8));
    assert_eq!(
        limits.parked_limit,
        (2 * GIB * 4).min(16376 * MIB / 100 * 25)
    );
}

fn source() -> ModelSource {
    ModelSource::Local {
        path: "/models/a".into(),
    }
}

// T26/T23: a 3B bf16 model (~6 GiB) on a 16 GB card: request, no fixed
// shares; its pinned copy (1.5 x 6 GiB) plus the engine's 4 GiB fits the
// 15 GiB the system domain holds parked, so it parks host_backed.
#[test]
fn a_discrete_template_states_a_request_and_derives_phases() {
    let memory = TemplateMemory::Device {
        managed_limit: 15 << 30,
        device_total: 16376 << 20,
        weights_bytes: Some(6 << 30),
        system_parked_limit: 15 << 30,
        kv_cache_bytes: None,
    };
    let doc = deployment_document(
        "a",
        "a",
        &source(),
        Engine::Sglang,
        &memory,
        DEFAULT_REQUEST_DEADLINE,
        true,
        "local",
    )
    .unwrap();
    assert!(doc.get("resources").is_none());
    // The picker chooses the GPU (discrete GPU design §7); the deployment
    // parser requires the key, so the template pins nothing with an empty list.
    assert_eq!(doc["devices"], serde_json::json!([]));
    let (request, kv) = device_request(Engine::Sglang, 6 << 30, 15 << 30, 16376 << 20);
    assert_eq!(kv, (15i64 << 30) / 4); // min(4 GiB, 3.75 GiB)
    assert_eq!(
        doc["engine_config"]["memory"]["request"],
        format!("{request}B")
    );
    assert_eq!(doc["engine_config"]["memory"]["kv_cache"], format!("{kv}B"));
    assert_eq!(doc["residency"], "host_backed");
}

// T26: vLLM's request never falls below 0.75 of the card.
#[test]
fn the_vllm_request_has_a_floor() {
    let (small, _) = device_request(Engine::Vllm, 2 << 30, 15 << 30, 16376 << 20);
    assert!(small >= (16376i64 << 20) / 100 * 75);
    // SGLang has no floor: weights x 1.10 plus the KV cache.
    let (sglang, kv) = device_request(Engine::Sglang, 2 << 30, 15 << 30, 16376 << 20);
    assert_eq!(sglang, (2i64 << 30) / 100 * 110 + kv);
}

// Owner decision 2: host_backed is the discrete default when the copy fits.
#[test]
fn the_default_tier_follows_the_host() {
    // The pinned copy is charged at 1.5 times the weights: 12 + 4 <= 16.
    assert_eq!(
        default_residency(true, Some((8 << 30, 16 << 30))),
        "host_backed"
    );
    assert_eq!(default_residency(true, Some((8 << 30, 15 << 30))), "deep");
    assert_eq!(default_residency(true, Some((20 << 30, 15 << 30))), "deep");
    assert_eq!(default_residency(true, None), "deep");
    assert_eq!(
        default_residency(false, Some((8 << 30, 15 << 30))),
        "restart_only"
    );
    // Task 6's rule: the parked system allocation is the engine's host overhead
    // plus the copy, so a copy that fits only without the overhead parks deep.
    assert_eq!(default_residency(true, Some((12 << 30, 15 << 30))), "deep");
}

// Review focus 1: a model that cannot fit is refused at deploy with numbers.
#[test]
fn a_model_larger_than_the_device_is_refused() {
    let memory = TemplateMemory::Device {
        managed_limit: 15 << 30,
        device_total: 16376 << 20,
        weights_bytes: Some(16 << 30),
        system_parked_limit: 15 << 30,
        kv_cache_bytes: None,
    };
    let error = deployment_document(
        "a",
        "a",
        &source(),
        Engine::Vllm,
        &memory,
        DEFAULT_REQUEST_DEADLINE,
        true,
        "local",
    )
    .unwrap_err();
    assert!(matches!(
        error,
        TemplateError::InsufficientDeviceMemory { .. }
    ));
    assert!(error.to_string().starts_with("insufficient_device_memory"));
    let (request, _) = device_request(Engine::Vllm, 16 << 30, 15 << 30, 16376 << 20);
    let charged = request + capyctl_config::effective::ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES;
    assert!(error.to_string().contains(&charged.to_string()), "{error}");
    assert!(
        error.to_string().contains(&(15i64 << 30).to_string()),
        "{error}"
    );
}

// T26: the unified template is byte-identical to before.
#[test]
fn the_unified_template_is_unchanged() {
    // Captured from `main` before discrete hosts had a template.
    let before = include_str!("fixtures/unified_deployment.json");
    let doc = deployment_document(
        "a",
        "a",
        &source(),
        Engine::Vllm,
        &TemplateMemory::Unified {
            capacity_bytes: 128 << 30,
        },
        DEFAULT_REQUEST_DEADLINE,
        true,
        "local",
    )
    .unwrap();
    assert_eq!(
        serde_json::to_string_pretty(&doc).unwrap(),
        before.trim_end()
    );
}

// T26/T23: the generated discrete template resolves on the discrete host this
// module publishes, for every engine and tier, and every phase fits the card:
// a template whose startup peak exceeded the device could never be placed.
#[test]
fn the_discrete_template_resolves_and_fits_the_card() {
    let shape = HostShape::Discrete(vec![rtx(0, 16376, 1536)]);
    let host = host_policy(&installations(), "env", 61 * GIB, None, &shape);
    let gpu = rtx(0, 16376, 1536).memory.unwrap();
    let limits = device_limits(&gpu, MAX_PARKED);
    let system_parked = 61 * GIB / 100 * PARKED_FRACTION;
    for (engine, deep_park) in [
        (Engine::Vllm, true),
        (Engine::Sglang, true),
        (Engine::Vllm, false),
    ] {
        let mut host = host.clone();
        host["runtime_profiles"]["local"]["engine"] = engine.name().into();
        host["runtime_profiles"]["local"]["security"]["deep_park"] =
            if deep_park { "enabled" } else { "disabled" }.into();
        let doc = deployment_document(
            "a",
            "a",
            &source(),
            engine,
            &TemplateMemory::Device {
                managed_limit: limits.managed_limit,
                device_total: gpu.total_bytes,
                weights_bytes: Some(6 * GIB),
                system_parked_limit: system_parked,
                kv_cache_bytes: None,
            },
            DEFAULT_REQUEST_DEADLINE,
            deep_park,
            "local",
        )
        .unwrap();
        let expected = if deep_park {
            "host_backed"
        } else {
            "restart_only"
        };
        assert_eq!(doc["residency"], expected);
        let choices = capyctl_config::instances::device_choices(&doc, &host).unwrap();
        let (device, chosen) = choices.first().expect("the picker has a GPU to choose");
        assert_eq!(device, "gpu0");
        let effective = capyctl_config::effective::resolve_effective_with_checkpoint(
            chosen,
            &host,
            capyctl_config::effective::CheckpointFacts {
                weights_bytes: Some(6 * GIB),
                ..Default::default()
            },
        )
        .unwrap_or_else(|error| panic!("{engine:?}: {error}"));
        let value = serde_json::to_value(&effective).unwrap();
        for (phase, footprint) in value["resources"].as_object().unwrap() {
            for allocation in footprint["allocations"].as_array().unwrap() {
                if allocation["domain"] == "gpu0" {
                    assert!(
                        allocation["bytes"].as_i64().unwrap() <= limits.managed_limit,
                        "{engine:?} {phase}: {allocation}"
                    );
                }
            }
        }
    }
}

fn hugging_face() -> ModelSource {
    ModelSource::HuggingFace {
        repo: "org/model".into(),
        revision: "0123456789abcdef0123456789abcdef01234567".into(),
        files: vec![],
        token_ref: None,
    }
}

/// The discrete host this module publishes for one 16 GB card, opted in to
/// Hugging Face sources (ADR 0008).
fn discrete_host_allowing_sources(engine: Engine) -> serde_json::Value {
    let shape = HostShape::Discrete(vec![rtx(0, 16376, 1536)]);
    let mut host = host_policy(&installations(), "env", 61 * GIB, None, &shape);
    host["runtime_profiles"]["local"]["engine"] = engine.name().into();
    host["runtime_profiles"]["local"]["security"]["deep_park"] = "enabled".into();
    host["model_sources"] = serde_json::json!({"huggingface": "allowed", "max_bytes": "100GiB"});
    host
}

fn card_memory(weights_bytes: Option<i64>, kv_cache_bytes: Option<i64>) -> TemplateMemory {
    let gpu = rtx(0, 16376, 1536).memory.unwrap();
    TemplateMemory::Device {
        managed_limit: device_limits(&gpu, MAX_PARKED).managed_limit,
        device_total: gpu.total_bytes,
        weights_bytes,
        system_parked_limit: 61 * GIB / 100 * PARKED_FRACTION,
        kv_cache_bytes,
    }
}

// T26 (review decision): a Hugging Face or HTTP source works on a discrete
// host. Its weights are known only once downloaded, so the template states the
// KV cache alone and parks deep (a host-RAM copy cannot be sized yet): the
// revision is accepted provisional and sized from the checkpoint once the
// download is measured (ADR 0014 §7), never refused for being remote.
#[test]
fn a_remote_source_on_a_discrete_host_is_sized_once_downloaded() {
    for engine in [Engine::Vllm, Engine::Sglang] {
        let doc = deployment_document(
            "a",
            "a",
            &hugging_face(),
            engine,
            &card_memory(None, None),
            DEFAULT_REQUEST_DEADLINE,
            true,
            "local",
        )
        .expect("a remote source is not refused");
        let kv = device_limits(&rtx(0, 16376, 1536).memory.unwrap(), MAX_PARKED).managed_limit / 4;
        let kv = kv.min(4 * GIB);
        assert_eq!(
            doc["engine_config"]["memory"],
            serde_json::json!({"kv_cache": format!("{kv}B")})
        );
        assert_eq!(doc["residency"], "deep");
        let host = discrete_host_allowing_sources(engine);
        let (_, chosen) = capyctl_config::instances::device_choices(&doc, &host)
            .unwrap()
            .into_iter()
            .next()
            .expect("the picker has a GPU");
        // Not materializable before the download: acceptance freezes it
        // provisional (the placeholder zero weights resolve).
        let e = capyctl_config::effective::resolve_effective(&chosen, &host).unwrap_err();
        assert_eq!(
            e.code,
            capyctl_config::ConfigErrorCode::NotMaterializable,
            "{e}"
        );
        let facts = |weights| capyctl_config::effective::CheckpointFacts {
            weights_bytes: Some(weights),
            ..Default::default()
        };
        capyctl_config::effective::resolve_effective_with_checkpoint(&chosen, &host, facts(0))
            .expect("the provisional placeholder");
        // Measured: sized as a local checkpoint of the same weights is.
        let sized = capyctl_config::effective::resolve_effective_with_checkpoint(
            &chosen,
            &host,
            facts(8 * GIB),
        )
        .unwrap_or_else(|error| panic!("{engine:?}: {error}"));
        let (_, device) = sized.ready_device_allocation().expect("on the card");
        let gpu = rtx(0, 16376, 1536).memory.unwrap();
        let (local, _) = device_request(
            engine,
            8 * GIB,
            device_limits(&gpu, MAX_PARKED).managed_limit,
            gpu.total_bytes,
        );
        assert_eq!(
            device,
            local + capyctl_config::effective::ENGINE_DEVICE_OVERHEAD_PLACEHOLDER_BYTES,
            "{engine:?}"
        );
    }
    // Without deep parking the tier is restart_only, as for a local checkpoint.
    let doc = deployment_document(
        "a",
        "a",
        &hugging_face(),
        Engine::Vllm,
        &card_memory(None, None),
        DEFAULT_REQUEST_DEADLINE,
        false,
        "local",
    )
    .unwrap();
    assert_eq!(doc["residency"], "restart_only");
}

// T26 (review decision): an operator's CAPYCTL_KV_CACHE_BYTES is honoured on a
// discrete host within the card, and refused with the numbers and the variable
// when it cannot fit, never silently replaced by the template's own KV cache.
#[test]
fn a_declared_kv_cache_is_honoured_within_the_card_or_refused() {
    let doc = deployment_document(
        "a",
        "a",
        &source(),
        Engine::Sglang,
        &card_memory(Some(8 * GIB), Some(2 * GIB)),
        DEFAULT_REQUEST_DEADLINE,
        true,
        "local",
    )
    .unwrap();
    assert_eq!(
        doc["engine_config"]["memory"]["kv_cache"],
        format!("{}B", 2 * GIB)
    );
    assert_eq!(
        doc["engine_config"]["memory"]["request"],
        format!("{}B", (8 * GIB) / 100 * 110 + 2 * GIB)
    );
    // A remote source states the operator's KV cache too.
    let remote = deployment_document(
        "a",
        "a",
        &hugging_face(),
        Engine::Sglang,
        &card_memory(None, Some(2 * GIB)),
        DEFAULT_REQUEST_DEADLINE,
        true,
        "local",
    )
    .unwrap();
    assert_eq!(
        remote["engine_config"]["memory"],
        serde_json::json!({"kv_cache": format!("{}B", 2 * GIB)})
    );
    // 8 GiB of weights and a 10 GiB KV cache do not fit a 16 GB card; neither
    // does a KV cache alone larger than what the card's domain manages.
    for (weights, kv) in [(Some(8 * GIB), 10 * GIB), (None, 16 * GIB)] {
        let error = deployment_document(
            "a",
            "a",
            &source(),
            Engine::Sglang,
            &card_memory(weights, Some(kv)),
            DEFAULT_REQUEST_DEADLINE,
            true,
            "local",
        )
        .unwrap_err();
        assert_eq!(error.code(), "insufficient_device_memory");
        let text = error.to_string();
        assert!(text.contains("CAPYCTL_KV_CACHE_BYTES"), "{text}");
        assert!(text.contains(&kv.to_string()), "{text}");
    }
}

/// Owner decision 2026-09-25: `CAPYCTL_MODELS_ROOT` is optional. Unset, the
/// installation names no models directory (the role resolves `~/models`);
/// set, it must be a directory, and a relative value is made absolute.
// T03 T14
#[test]
fn the_models_root_variable_is_optional() {
    let Some(_guard) = isolated("the_models_root_variable_is_optional") else {
        return;
    };
    let dir = tempfile::TempDir::new().expect("a temporary installation");
    engine_env(dir.path(), true, false);
    std::env::remove_var("CAPYCTL_MODELS_ROOT");
    let installation = crate::roles::EnvEngineProvider::new()
        .installation()
        .expect("no models directory is needed to find the engine");
    assert!(installation.models_root.as_os_str().is_empty());
    std::env::set_var("CAPYCTL_MODELS_ROOT", dir.path().join("missing"));
    let message = crate::roles::EnvEngineProvider::new()
        .installation()
        .expect_err("a named directory must exist")
        .to_string();
    assert!(message.contains("CAPYCTL_MODELS_ROOT"), "{message}");
    std::env::set_var("CAPYCTL_MODELS_ROOT", dir.path());
    assert_eq!(
        crate::roles::EnvEngineProvider::new()
            .installation()
            .unwrap()
            .models_root,
        dir.path()
    );
    for name in [
        "CAPYCTL_VLLM_BIN",
        "CAPYCTL_MODELS_ROOT",
        "CAPYCTL_RUNTIME_DIR",
        "CAPYCTL_ENGINE_FINGERPRINT",
    ] {
        std::env::remove_var(name);
    }
}

/// Owner rule 2026-09-25 (every setting three ways, SPEC §15.2): the
/// standalone installation's settings follow CLI flag > environment > the
/// document's `host:` block > default, and the document alone can declare
/// the engine (`host.local_engine.vllm`) with no variable set.
// T03 T21
#[test]
fn the_standalone_installation_follows_flag_env_document_default() {
    use crate::roles::EngineOverrides;
    use capyctl_config::effective::InstallationDrift;
    let Some(_guard) = isolated("the_standalone_installation_follows_flag_env_document_default")
    else {
        return;
    };
    let dir = tempfile::TempDir::new().expect("a temporary installation");
    let bin = fake_engine_bin(dir.path());
    let runtime = private_runtime(dir.path());
    for name in [
        "CAPYCTL_VLLM_BIN",
        "CAPYCTL_SGLANG_BIN",
        "CAPYCTL_RUNTIME_DIR",
        "CAPYCTL_ENGINE_FINGERPRINT",
        "CAPYCTL_KV_CACHE_BYTES",
        "CAPYCTL_ENGINE_ARGS",
        "CAPYCTL_DEEP_PARK",
        "CAPYCTL_TRUST_REMOTE_CODE",
        "CAPYCTL_INSTALLATION_DRIFT",
        "CAPYCTL_ENGINE_PORTS",
        "CAPYCTL_STANDALONE_ENGINE_PORTS",
    ] {
        std::env::remove_var(name);
    }
    std::env::set_var("CAPYCTL_MODELS_ROOT", dir.path());
    let host = serde_json::json!({
        "local_engine": {
            "vllm": bin, "build_fingerprint": "yaml-fp", "args": ["--yaml"],
            "kv_cache": "1GiB", "deep_park": "off", "trust_remote_code": false,
            "installation_drift": "refuse",
        },
        "runtime_dir": runtime,
        "resource_policy": {"endpoint_port_range": {"start": 20200, "end": 20209}},
    });
    let provider = |flags: EngineOverrides| {
        let provider = crate::roles::EnvEngineProvider::new().with_flags(flags);
        provider.configure(&host).expect("the document is valid");
        provider
            .installation()
            .expect("the document declares an installation")
    };
    // The document alone.
    let yaml = provider(EngineOverrides::default());
    assert_eq!(yaml.executable, bin);
    assert_eq!(yaml.build_fingerprint, "yaml-fp");
    assert_eq!(yaml.args, ["--yaml"]);
    assert_eq!(yaml.engine_config["memory"]["kv_cache"], "1GiB");
    assert!(yaml.kv_cache_declared);
    assert!(!yaml.deep_park);
    assert!(!yaml.trust_remote_code);
    assert_eq!(yaml.installation_drift, InstallationDrift::Refuse);
    assert_eq!(yaml.engine_ports, (20200, 20209));
    // The environment wins over the document.
    std::env::set_var("CAPYCTL_ENGINE_FINGERPRINT", "env-fp");
    std::env::set_var("CAPYCTL_ENGINE_ARGS", "--env");
    std::env::set_var("CAPYCTL_KV_CACHE_BYTES", "2GiB");
    std::env::set_var("CAPYCTL_DEEP_PARK", "on");
    std::env::set_var("CAPYCTL_TRUST_REMOTE_CODE", "1");
    std::env::set_var("CAPYCTL_INSTALLATION_DRIFT", "warn");
    std::env::set_var("CAPYCTL_ENGINE_PORTS", "20300-20309");
    let env = provider(EngineOverrides::default());
    assert_eq!(env.build_fingerprint, "env-fp");
    assert_eq!(env.args, ["--env"]);
    assert_eq!(env.engine_config["memory"]["kv_cache"], "2GiB");
    assert!(env.deep_park);
    assert!(env.trust_remote_code);
    assert_eq!(env.installation_drift, InstallationDrift::Warn);
    assert_eq!(env.engine_ports, (20300, 20309));
    // The flags win over both.
    let flagged = provider(EngineOverrides {
        build_fingerprint: Some("flag-fp".into()),
        args: Some(vec!["--flag".into()]),
        kv_cache: Some("3GiB".into()),
        deep_park: Some(false),
        trust_remote_code: Some(false),
        installation_drift: Some(InstallationDrift::Refuse),
        engine_ports: Some((20400, 20409)),
        ..Default::default()
    });
    assert_eq!(flagged.build_fingerprint, "flag-fp");
    assert_eq!(flagged.args, ["--flag"]);
    assert_eq!(flagged.engine_config["memory"]["kv_cache"], "3GiB");
    assert!(!flagged.deep_park);
    assert!(!flagged.trust_remote_code);
    assert_eq!(flagged.installation_drift, InstallationDrift::Refuse);
    assert_eq!(flagged.engine_ports, (20400, 20409));
    // A malformed document value is refused with its path.
    let refused = crate::roles::EnvEngineProvider::new()
        .configure(&serde_json::json!({"local_engine": {"deep_park": "maybe"}}))
        .expect_err("a malformed switch is refused")
        .to_string();
    assert!(refused.contains("host.local_engine.deep_park"), "{refused}");
    for name in [
        "CAPYCTL_MODELS_ROOT",
        "CAPYCTL_ENGINE_FINGERPRINT",
        "CAPYCTL_ENGINE_ARGS",
        "CAPYCTL_KV_CACHE_BYTES",
        "CAPYCTL_DEEP_PARK",
        "CAPYCTL_TRUST_REMOTE_CODE",
        "CAPYCTL_INSTALLATION_DRIFT",
        "CAPYCTL_ENGINE_PORTS",
    ] {
        std::env::remove_var(name);
    }
}

// T26 (final review I3): found live on the discrete-GPU laptop host, vLLM
// 0.29 held 13.2 GiB of the 16 GB card against a 12.0 GiB reservation (the
// CUDA context and graphs sit outside the request). The device domain is now
// charged the request plus that overhead, so the planner's figure and what
// the launch check sees on the card agree; before, the ledger under-charged
// the card by about 1.2 GiB.
#[test]
fn a_discrete_charge_covers_what_vllm_holds_on_the_card() {
    const MEASURED_HELD: i64 = 13_516 << 20; // 13.2 GiB
    let gpu = rtx(0, 16376, 1536).memory.unwrap();
    let limits = device_limits(&gpu, MAX_PARKED);
    let weights = 8_040_000_000;
    let (request, _) = device_request(Engine::Vllm, weights, limits.managed_limit, gpu.total_bytes);
    assert!(request < MEASURED_HELD, "the request alone under-charges");
    let doc = deployment_document(
        "a",
        "a",
        &source(),
        Engine::Vllm,
        &card_memory(Some(weights), None),
        DEFAULT_REQUEST_DEADLINE,
        true,
        "local",
    )
    .unwrap();
    let host = discrete_host_allowing_sources(Engine::Vllm);
    let (_, chosen) = capyctl_config::instances::device_choices(&doc, &host)
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let facts = capyctl_config::effective::CheckpointFacts {
        weights_bytes: Some(weights),
        ..Default::default()
    };
    let effective =
        capyctl_config::effective::resolve_effective_with_checkpoint(&chosen, &host, facts)
            .unwrap();
    let (_, charged) = effective.ready_device_allocation().unwrap();
    assert!(charged >= MEASURED_HELD, "{charged} < {MEASURED_HELD}");
    assert!(charged <= limits.managed_limit);
    // vLLM is still told the request, not the charge: the overhead is what it
    // holds beyond the fraction capyctl renders.
    assert_eq!(effective.engine_config.memory().request_bytes, request);
}
