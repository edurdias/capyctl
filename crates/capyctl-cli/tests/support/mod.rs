//! Shared wiring for the standalone integration tests.
//!
//! Every test binary that includes this module uses some of it, so unused items
//! here are expected rather than a sign of dead code.
#![allow(dead_code)]

pub mod process;

/// Final review I14: an isolated home for every `capyctl` process a test spawns,
/// one per test binary, owner-only, so a spawned role or client never reads
/// the developer's `~/.config/capyctl/engines.yaml`, host document or state.
/// It lives in cargo's scratch directory for integration tests
/// (`CARGO_TARGET_TMPDIR`), never in the developer's home; a test that
/// starts a role states its own `CAPYCTL_STATE_DIR` under an owner-only root.
pub fn isolated_home() -> &'static std::path::Path {
    use std::os::unix::fs::PermissionsExt;
    static HOME: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    /// Removes the isolated home when the test binary exits (the harness
    /// exits through `exit`, which runs `atexit` handlers).
    extern "C" fn remove_home() {
        if let Some(home) = HOME.get() {
            let _ = std::fs::remove_dir_all(home);
        }
    }
    HOME.get_or_init(|| {
        let home = tempfile::Builder::new()
            .prefix("capyctl-test-home-")
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .expect("an isolated home")
            .keep();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        for dir in [".config", ".local/state", ".local/share", ".cache"] {
            std::fs::create_dir_all(home.join(dir)).unwrap();
        }
        // SAFETY: `remove_home` is a plain function that only reads an
        // initialised static; registering it has no other precondition.
        unsafe {
            libc::atexit(remove_home);
        }
        home
    })
}

/// Final review I14: `isolate(command)` points `HOME` and every XDG base
/// directory of a spawned `capyctl` at [`isolated_home`] and drops every
/// `CAPYCTL_*` and `HF_*` variable of the developer's environment (a role
/// document named by `CAPYCTL_CONFIG` included). A test that states its own `HOME` or
/// `XDG_CONFIG_HOME` afterwards still wins, since later `env` calls replace
/// these.
pub fn isolate(command: &mut std::process::Command) -> &mut std::process::Command {
    let home = isolated_home();
    // Re-review: the developer's own `CAPYCTL_*` and `HF_*` settings (a state
    // root, an engine, a management address, a Hugging Face token or
    // endpoint) never reach a spawned process; a test states what it needs
    // after this.
    for (name, _) in std::env::vars_os() {
        let text = name.to_string_lossy();
        if text.starts_with("CAPYCTL_") || text.starts_with("HF_") {
            command.env_remove(&name);
        }
    }
    command
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env_remove("CAPYCTL_CONFIG")
}

/// The `capyctl` binary under test, isolated from the developer's home
/// ([`isolate`]) and observing the stated host memory ([`pin_host_memory`]).
/// Every test spawns the binary through this.
pub fn capyctl() -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_capyctl"));
    isolate(&mut command);
    pin_host_memory(&mut command);
    command
}

/// Every role a spawned `capyctl` runs (standalone or host) observes
/// [`TEST_CAPACITY_BYTES`], all of it free, instead of this machine's
/// `/proc/meminfo`, through the test-only
/// [`capyctl_agent::memory::TEST_PINNED_HOST_MEMORY_ENV`] (debug builds only).
/// SPEC §7: standalone derives its limits from observed capacity and admits
/// against observed free memory, so on a busy machine (found 2026-10-08:
/// 14 GiB available of 61) every deploy the binary tests made failed
/// `insufficient resources`. A test that clears the environment calls this
/// again.
pub fn pin_host_memory(command: &mut std::process::Command) -> &mut std::process::Command {
    command.env(
        capyctl_agent::memory::TEST_PINNED_HOST_MEMORY_ENV,
        format!("{TEST_CAPACITY_BYTES}:{TEST_CAPACITY_BYTES}"),
    )
}

use std::sync::Arc;

use axum::response::IntoResponse as _;

/// A state directory the controller lock will accept.
///
/// The lock walks every ancestor of the state path and refuses any that is group- or
/// other-writable, because such an ancestor lets another account replace the
/// directory the lock guards. `/tmp` is 1777 and a checkout is commonly 0775, so
/// neither can hold controller state. The home directory is the usual root that
/// satisfies the rule.
pub fn safe_state_dir() -> tempfile::TempDir {
    let home = std::env::var("HOME").expect("HOME is set");
    tempfile::TempDir::new_in(home).expect("a state directory under an owner-only root")
}

/// Boot standalone on the testkit's Fake installation.
///
/// Passing the installation in is what keeps these tests from depending on whatever
/// engine the developer's environment happens to name. Passing this one is not
/// qualification of a native recipe and must never be reported as one (SPEC §18).
pub async fn boot(state_dir: &std::path::Path) -> capyctl_cli::roles::App {
    try_boot_on(state_dir, engine_ports())
        .await
        .expect("standalone boots")
}

/// As [`boot`], on the engine port range `ports`, returning the refusal
/// instead of panicking on it. The range is part of the published host
/// document, so a test that restarts the role passes the same range again.
pub async fn try_boot_on(
    state_dir: &std::path::Path,
    ports: (u16, u16),
) -> Result<capyctl_cli::roles::App, capyctl_cli::roles::StartError> {
    capyctl_cli::roles::start_standalone_with_memory(
        state_dir,
        Arc::new(PortedProvider {
            ports,
            deep_park: false,
            members: None,
            models_root: None,
            kv_cache: None,
            source_origin: None,
        }),
        test_memory(),
    )
    .await
}

/// As [`boot`], observing `memory` instead of [`test_memory`] (a host whose
/// available memory is below its capacity).
pub async fn boot_with_memory(
    state_dir: &std::path::Path,
    memory: capyctl_cli::host_observation::MemoryReader,
) -> capyctl_cli::roles::App {
    capyctl_cli::roles::start_standalone_with_memory(
        state_dir,
        Arc::new(PortedProvider {
            ports: engine_ports(),
            deep_park: false,
            members: None,
            models_root: None,
            kv_cache: None,
            source_origin: None,
        }),
        memory,
    )
    .await
    .expect("standalone boots")
}

/// As [`try_boot_on`], sampling the host's GPUs through `gpu` (design §1)
/// instead of the machine's own `nvidia-smi`.
pub async fn try_boot_with_gpu(
    state_dir: &std::path::Path,
    gpu: impl Fn() -> Option<capyctl_agent::gpu_memory::GpuSample> + Send + Sync + 'static,
) -> Result<capyctl_cli::roles::App, capyctl_cli::roles::StartError> {
    capyctl_cli::roles::start_standalone_with_gpu(
        state_dir,
        Arc::new(PortedProvider {
            ports: engine_ports(),
            deep_park: false,
            members: None,
            models_root: None,
            kv_cache: None,
            source_origin: None,
        }),
        test_memory(),
        Arc::new(gpu),
    )
    .await
}

/// As [`try_boot_with_gpu`], with deep parking set by `deep_park` and the
/// model store at `models_root`, so a test can size a checkpoint it wrote.
pub async fn try_boot_discrete(
    state_dir: &std::path::Path,
    gpu: impl Fn() -> Option<capyctl_agent::gpu_memory::GpuSample> + Send + Sync + 'static,
    models_root: &std::path::Path,
    deep_park: bool,
) -> Result<capyctl_cli::roles::App, capyctl_cli::roles::StartError> {
    try_boot_discrete_with_kv(state_dir, gpu, models_root, deep_park, None).await
}

/// As [`try_boot_discrete`], on the engine port range `ports`: a test that
/// restarts the role passes the same range again (it is part of the
/// generated policy).
pub async fn try_boot_discrete_on(
    state_dir: &std::path::Path,
    gpu: impl Fn() -> Option<capyctl_agent::gpu_memory::GpuSample> + Send + Sync + 'static,
    models_root: &std::path::Path,
    ports: (u16, u16),
) -> Result<capyctl_cli::roles::App, capyctl_cli::roles::StartError> {
    capyctl_cli::roles::start_standalone_with_gpu(
        state_dir,
        Arc::new(PortedProvider {
            ports,
            deep_park: true,
            members: None,
            models_root: Some(models_root.to_path_buf()),
            kv_cache: None,
            source_origin: None,
        }),
        test_memory(),
        Arc::new(gpu),
    )
    .await
}

/// As [`try_boot_discrete`], with the KV cache the operator stated
/// (`CAPYCTL_KV_CACHE_BYTES`) when `kv_cache` is `Some`.
pub async fn try_boot_discrete_with_kv(
    state_dir: &std::path::Path,
    gpu: impl Fn() -> Option<capyctl_agent::gpu_memory::GpuSample> + Send + Sync + 'static,
    models_root: &std::path::Path,
    deep_park: bool,
    kv_cache: Option<&'static str>,
) -> Result<capyctl_cli::roles::App, capyctl_cli::roles::StartError> {
    capyctl_cli::roles::start_standalone_with_gpu(
        state_dir,
        Arc::new(PortedProvider {
            ports: engine_ports(),
            deep_park,
            members: None,
            models_root: Some(models_root.to_path_buf()),
            kv_cache,
            source_origin: None,
        }),
        test_memory(),
        Arc::new(gpu),
    )
    .await
}

/// As [`boot`], on a Fake installation whose host leaves deep parking on
/// (ADR 0012: the product default, `CAPYCTL_DEEP_PARK` unset), with every Fake
/// reporting `members` as its launched group. The coordinator checks after a
/// park or restore that the recorded processes are the ones alive (SPEC
/// §13.2), so a park test hands it real processes it owns. Only the adapter is
/// the Fake: the generated deployment, its resolution and the coordinator's
/// park and wake are the product's own. Not qualification of a native recipe
/// (SPEC §18).
pub async fn boot_deep_parking(
    state_dir: &std::path::Path,
    members: Vec<capyctl_domain::completion::ProcessIdentity>,
) -> capyctl_cli::roles::App {
    capyctl_cli::roles::start_standalone_with_memory(
        state_dir,
        Arc::new(PortedProvider {
            ports: engine_ports(),
            deep_park: true,
            members: Some(members),
            models_root: None,
            kv_cache: None,
            source_origin: None,
        }),
        test_memory(),
    )
    .await
    .expect("standalone boots")
}

/// As [`boot_deep_parking`], with this run's generic overrides (`--set`,
/// `CAPYCTL_SET__…`).
pub async fn boot_deep_parking_with_overrides(
    state_dir: &std::path::Path,
    members: Vec<capyctl_domain::completion::ProcessIdentity>,
    overrides: &capyctl_cli::roles::SettingOverrides,
) -> capyctl_cli::roles::App {
    capyctl_cli::roles::start_standalone_configured_with_overrides(
        state_dir,
        None,
        Arc::new(PortedProvider {
            ports: engine_ports(),
            deep_park: true,
            members: Some(members),
            models_root: None,
            kv_cache: None,
            source_origin: None,
        }),
        test_memory(),
        overrides,
    )
    .await
    .expect("standalone boots")
}

/// As [`boot`], with the role document named by `--config`; the result is
/// returned so a test can assert a refusal.
pub async fn boot_configured(
    state_dir: &std::path::Path,
    config: &std::path::Path,
) -> Result<capyctl_cli::roles::App, capyctl_cli::roles::StartError> {
    boot_configured_on(state_dir, config, engine_ports()).await
}

/// As [`boot_configured`], on the engine port range `ports`. The range is part
/// of the published host document, so a test that restarts the role passes
/// the same range again, as an operator's unchanged environment would.
pub async fn boot_configured_on(
    state_dir: &std::path::Path,
    config: &std::path::Path,
    ports: (u16, u16),
) -> Result<capyctl_cli::roles::App, capyctl_cli::roles::StartError> {
    capyctl_cli::roles::start_standalone_configured(
        state_dir,
        Some(config),
        Arc::new(PortedProvider {
            ports,
            deep_park: false,
            members: None,
            models_root: None,
            kv_cache: None,
            source_origin: None,
        }),
        test_memory(),
    )
    .await
}

/// Owner decision 2026-09-25: boot with the role document `config` (if any),
/// this run's model flags, and an installation whose models directory is
/// `models_root` (an empty path names none, as `CAPYCTL_MODELS_ROOT` unset does;
/// `None` is the testkit's store). Model-source downloads are served from
/// `source_origin`, else from an origin nothing listens on; the GPUs are
/// sampled through `gpu`.
pub async fn boot_with_models(
    state_dir: &std::path::Path,
    config: Option<&std::path::Path>,
    models_root: Option<std::path::PathBuf>,
    flags: &capyctl_cli::roles::ModelOverrides,
    source_origin: Option<String>,
    gpu: impl Fn() -> Option<capyctl_agent::gpu_memory::GpuSample> + Send + Sync + 'static,
) -> Result<capyctl_cli::roles::App, capyctl_cli::roles::StartError> {
    capyctl_cli::roles::start_standalone_configured_with_models(
        state_dir,
        config,
        Arc::new(PortedProvider {
            ports: engine_ports(),
            deep_park: false,
            members: None,
            models_root,
            kv_cache: None,
            source_origin,
        }),
        test_memory(),
        Arc::new(gpu),
        flags,
    )
    .await
}

/// Boot standalone with the role document `config` on the production
/// provider with no engine variable set, so it publishes only the profiles
/// registered beside `config`. The models directory is one under
/// `state_dir`, never the developer's.
pub async fn boot_registered_only(
    state_dir: &std::path::Path,
    config: &std::path::Path,
) -> Result<capyctl_cli::roles::App, capyctl_cli::roles::StartError> {
    for name in [
        "CAPYCTL_VLLM_BIN",
        "CAPYCTL_SGLANG_BIN",
        "CAPYCTL_TENSORFOLD_BIN",
    ] {
        std::env::remove_var(name);
    }
    let models = state_dir.join("models");
    std::fs::create_dir_all(&models)?;
    let flags = capyctl_cli::roles::ModelOverrides {
        models_root: Some(models),
        ..Default::default()
    };
    capyctl_cli::roles::start_standalone_configured_with_models(
        state_dir,
        Some(config),
        Arc::new(capyctl_cli::roles::EnvEngineProvider::with_managed_runtime(
            state_dir.join("runtime"),
        )),
        test_memory(),
        Arc::new(|| None),
        &flags,
    )
    .await
}

/// Owner decision 2026-09-25: boot with the role document `config` (if any)
/// and this run's generic overrides (`--set`, `CAPYCTL_SET__…`).
pub async fn boot_with_overrides(
    state_dir: &std::path::Path,
    config: Option<&std::path::Path>,
    overrides: &capyctl_cli::roles::SettingOverrides,
) -> Result<capyctl_cli::roles::App, capyctl_cli::roles::StartError> {
    boot_with_overrides_on(state_dir, config, overrides, engine_ports()).await
}

/// As [`boot_with_overrides`], on the engine port range `ports`, so a restart
/// keeps the published policy's shape.
pub async fn boot_with_overrides_on(
    state_dir: &std::path::Path,
    config: Option<&std::path::Path>,
    overrides: &capyctl_cli::roles::SettingOverrides,
    ports: (u16, u16),
) -> Result<capyctl_cli::roles::App, capyctl_cli::roles::StartError> {
    capyctl_cli::roles::start_standalone_configured_with_overrides(
        state_dir,
        config,
        Arc::new(PortedProvider {
            ports,
            deep_park: false,
            members: None,
            models_root: None,
            kv_cache: None,
            source_origin: None,
        }),
        test_memory(),
        overrides,
    )
    .await
}

/// The explicit host capacity every standalone test boots with: 32 GiB, all
/// of it free. Standalone derives its limits and the default deployment's
/// footprints from the observed capacity and admits against observed free
/// memory (SPEC §7), so reading the suite machine's own `/proc/meminfo` made
/// these tests fail on a machine with little memory free (found 2026-09-23 on
/// control-host: `a1_gate` and three standalone tests).
pub const TEST_CAPACITY_BYTES: i64 = 32 << 30;

/// The capacity a test that drives the real `capyctl` binary sizes its
/// deployment documents from. The binary observes [`TEST_CAPACITY_BYTES`]
/// ([`pin_host_memory`]); documents stay sized from a smaller capacity so
/// several deployments fit its derived limits at once.
pub const BINARY_TEST_CAPACITY_BYTES: i64 = 4 << 30;

/// The template memory a test that drives the real `capyctl` binary sizes its
/// deployment document from.
///
/// Design §1: the binary samples this machine's GPUs with `nvidia-smi`, and a
/// test cannot hand a sampler to another process, so it samples them the same
/// way and generates the template for the shape the binary will publish: a
/// machine without a GPU (or a unified one) gets the unified template sized
/// from [`BINARY_TEST_CAPACITY_BYTES`]; a discrete one gets a device request
/// sized for a small stated card, so it fits under the limits the binary
/// derives from any real card and the memory free on a busy one. The toy
/// checkpoint's weights are negligible.
pub fn binary_template_memory() -> capyctl_cli::standalone_config::TemplateMemory {
    use capyctl_agent::gpu_memory::{shape, GpuMemory, HostShape};
    match shape(capyctl_agent::gpu_memory::sample().as_ref()) {
        Ok(HostShape::Discrete(_)) => {
            let card = GpuMemory {
                total_bytes: BINARY_TEST_DEVICE_BYTES,
                used_bytes: 0,
                free_bytes: BINARY_TEST_DEVICE_BYTES,
            };
            let limits = capyctl_cli::standalone_config::device_limits(&card, 4);
            capyctl_cli::standalone_config::TemplateMemory::Device {
                managed_limit: limits.managed_limit,
                device_total: card.total_bytes,
                weights_bytes: Some(0),
                system_parked_limit: BINARY_TEST_CAPACITY_BYTES / 4,
                kv_cache_bytes: None,
            }
        }
        _ => capyctl_cli::standalone_config::TemplateMemory::Unified {
            capacity_bytes: BINARY_TEST_CAPACITY_BYTES,
        },
    }
}

/// The card a binary test sizes a discrete deployment for (see
/// [`binary_template_memory`]).
pub const BINARY_TEST_DEVICE_BYTES: i64 = 10 << 30;

pub fn test_memory() -> capyctl_cli::host_observation::MemoryReader {
    capyctl_cli::host_observation::fixed_memory(TEST_CAPACITY_BYTES, TEST_CAPACITY_BYTES)
}

/// A per-test engine port range: eight consecutive loopback ports that were
/// free when chosen, below the kernel's ephemeral range (`process::free_ports`).
/// Standalone leases engine endpoints from the bottom of its range, so a fixed
/// range (the 8100 default) makes every test that stands a stub engine up at
/// its leased endpoint collide with every other such test running in parallel.
pub fn engine_ports() -> (u16, u16) {
    let ports = process::free_ports(8, true);
    (ports[0], ports[7])
}

/// The testkit's Fake installation on a per-test engine port range.
struct PortedProvider {
    ports: (u16, u16),
    /// The host's deep-park switch; the testkit's installation opts out.
    deep_park: bool,
    /// Real processes the Fake reports as its launched group, if any.
    members: Option<Vec<capyctl_domain::completion::ProcessIdentity>>,
    /// The model store, when the test states one.
    models_root: Option<std::path::PathBuf>,
    /// The KV cache the operator stated (`CAPYCTL_KV_CACHE_BYTES`), if any.
    kv_cache: Option<&'static str>,
    /// The loopback origin model-source downloads are served from; unset,
    /// one nothing listens on, so no test reaches the network.
    source_origin: Option<String>,
}

impl capyctl_controller::EngineProvider for PortedProvider {
    fn installation(
        &self,
    ) -> Result<capyctl_controller::EngineInstallation, capyctl_controller::ProviderError> {
        let mut installation = capyctl_testkit::fake_installation();
        installation.engine_ports = self.ports;
        installation.deep_park = self.deep_park;
        if let Some(root) = &self.models_root {
            installation.models_root = root.clone();
        }
        if let Some(kv) = self.kv_cache {
            installation.engine_config["memory"]["kv_cache"] = kv.into();
            installation.kv_cache_declared = true;
        }
        Ok(installation)
    }

    fn bindings(
        &self,
        clock: capyctl_controller::coordinator::ServiceClock,
        log_dir: std::path::PathBuf,
        runtime_dir: std::path::PathBuf,
    ) -> Arc<dyn capyctl_controller::coordinator::EngineBindings> {
        match &self.members {
            Some(members) => capyctl_testkit::fake_bindings_with_members(
                clock,
                log_dir,
                runtime_dir,
                members.clone(),
            ),
            None => capyctl_testkit::fake_bindings(clock, log_dir, runtime_dir),
        }
    }

    fn tools_factory(&self) -> capyctl_controller::coordinator::ToolsFactory {
        capyctl_testkit::fake_tools_factory()
    }

    fn model_source_origin(&self) -> Option<String> {
        Some(
            self.source_origin
                .clone()
                .unwrap_or_else(|| capyctl_testkit::NO_NETWORK_ORIGIN.into()),
        )
    }
}

/// A minimal engine at the endpoint the coordinator recorded for a deployment.
///
/// The Fake engine answers in process and listens on nothing, while the router now
/// forwards to the address the launch recorded (SPEC §3). Standing a server up at
/// that address is what lets these tests exercise the real dispatch path, and it
/// also proves the router forwards to the recorded endpoint rather than to anything
/// it held from boot.
pub async fn stub_engine(
    controller: &Arc<capyctl_controller::CoordinatorLifecycle>,
    deployment: &str,
) -> tokio::task::JoinHandle<()> {
    use capyctl_controller::LifecyclePort as _;
    let runtime = controller
        .runtime_endpoint(deployment)
        .expect("the runtime endpoint is readable")
        .expect("a ready deployment has a recorded runtime");
    let url: reqwest::Url = runtime.endpoint.parse().expect("the endpoint is a URL");
    let address = format!(
        "{}:{}",
        url.host_str().expect("the endpoint names a host"),
        url.port().expect("the endpoint names a port")
    );
    let served = runtime.served_model.clone();
    // Spec §3: the forwarder sends the per-launch key on every request, and a real
    // engine guards every `/v1` route with it. The stub enforces the same thing, so
    // a forwarder that stopped sending the key fails here on CPU instead of on the
    // host.
    let expected = runtime
        .engine_key
        .clone()
        .map(|key| format!("Bearer {key}"));
    let authorized = move |headers: &axum::http::HeaderMap| -> bool {
        let Some(expected) = &expected else {
            return true;
        };
        headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|presented| presented == expected)
    };
    let models = {
        let served = served.clone();
        let authorized = authorized.clone();
        axum::routing::get(move |headers: axum::http::HeaderMap| {
            let served = served.clone();
            let authorized = authorized.clone();
            async move {
                if !authorized(&headers) {
                    return axum::http::StatusCode::UNAUTHORIZED.into_response();
                }
                axum::Json(serde_json::json!({
                    "object": "list",
                    "data": [{"id": served, "object": "model"}]
                }))
                .into_response()
            }
        })
    };
    // The forwarder always asks the engine to stream, and it validates every event
    // it is sent (SPEC §10), so the stub answers in the protocol an engine answers
    // in rather than with a single completion object.
    let chat = {
        let served = served.clone();
        axum::routing::post(move |headers: axum::http::HeaderMap| {
            let served = served.clone();
            let authorized = authorized.clone();
            async move {
                if !authorized(&headers) {
                    return axum::http::StatusCode::UNAUTHORIZED.into_response();
                }
                let chunk = |delta: serde_json::Value, finish: serde_json::Value| {
                    serde_json::json!({
                        "id": "stub-1",
                        "object": "chat.completion.chunk",
                        "created": 1,
                        "model": served,
                        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
                    })
                };
                let body = format!(
                    "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                    chunk(
                        serde_json::json!({"role": "assistant", "content": "ready"}),
                        serde_json::Value::Null
                    ),
                    chunk(serde_json::json!({}), serde_json::json!("stop")),
                );
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    body,
                )
                    .into_response()
            }
        })
    };
    let app = axum::Router::new()
        .route("/v1/models", models)
        .route("/v1/chat/completions", chat);
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .unwrap_or_else(|error| panic!("the recorded endpoint {address} is bindable: {error}"));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    })
}
