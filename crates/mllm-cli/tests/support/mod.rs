//! Shared wiring for the standalone integration tests.
//!
//! Every test binary that includes this module uses some of it, so unused items
//! here are expected rather than a sign of dead code.
#![allow(dead_code)]

pub mod process;

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
pub async fn boot(state_dir: &std::path::Path) -> mllm_cli::roles::App {
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
) -> Result<mllm_cli::roles::App, mllm_cli::roles::StartError> {
    mllm_cli::roles::start_standalone_with_memory(
        state_dir,
        Arc::new(PortedProvider {
            ports,
            deep_park: false,
            members: None,
            models_root: None,
            kv_cache: None,
        }),
        test_memory(),
    )
    .await
}

/// As [`try_boot_on`], sampling the host's GPUs through `gpu` (design §1)
/// instead of the machine's own `nvidia-smi`.
pub async fn try_boot_with_gpu(
    state_dir: &std::path::Path,
    gpu: impl Fn() -> Option<mllm_agent::gpu_memory::GpuSample> + Send + Sync + 'static,
) -> Result<mllm_cli::roles::App, mllm_cli::roles::StartError> {
    mllm_cli::roles::start_standalone_with_gpu(
        state_dir,
        Arc::new(PortedProvider {
            ports: engine_ports(),
            deep_park: false,
            members: None,
            models_root: None,
            kv_cache: None,
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
    gpu: impl Fn() -> Option<mllm_agent::gpu_memory::GpuSample> + Send + Sync + 'static,
    models_root: &std::path::Path,
    deep_park: bool,
) -> Result<mllm_cli::roles::App, mllm_cli::roles::StartError> {
    try_boot_discrete_with_kv(state_dir, gpu, models_root, deep_park, None).await
}

/// As [`try_boot_discrete`], with the KV cache the operator stated
/// (`MLLM_KV_CACHE_BYTES`) when `kv_cache` is `Some`.
pub async fn try_boot_discrete_with_kv(
    state_dir: &std::path::Path,
    gpu: impl Fn() -> Option<mllm_agent::gpu_memory::GpuSample> + Send + Sync + 'static,
    models_root: &std::path::Path,
    deep_park: bool,
    kv_cache: Option<&'static str>,
) -> Result<mllm_cli::roles::App, mllm_cli::roles::StartError> {
    mllm_cli::roles::start_standalone_with_gpu(
        state_dir,
        Arc::new(PortedProvider {
            ports: engine_ports(),
            deep_park,
            members: None,
            models_root: Some(models_root.to_path_buf()),
            kv_cache,
        }),
        test_memory(),
        Arc::new(gpu),
    )
    .await
}

/// As [`boot`], on a Fake installation whose host leaves deep parking on
/// (ADR 0012: the product default, `MLLM_DEEP_PARK` unset), with every Fake
/// reporting `members` as its launched group. The coordinator checks after a
/// park or restore that the recorded processes are the ones alive (SPEC
/// §13.2), so a park test hands it real processes it owns. Only the adapter is
/// the Fake: the generated deployment, its resolution and the coordinator's
/// park and wake are the product's own. Not qualification of a native recipe
/// (SPEC §18).
pub async fn boot_deep_parking(
    state_dir: &std::path::Path,
    members: Vec<mllm_domain::completion::ProcessIdentity>,
) -> mllm_cli::roles::App {
    mllm_cli::roles::start_standalone_with_memory(
        state_dir,
        Arc::new(PortedProvider {
            ports: engine_ports(),
            deep_park: true,
            members: Some(members),
            models_root: None,
            kv_cache: None,
        }),
        test_memory(),
    )
    .await
    .expect("standalone boots")
}

/// As [`boot`], with the role document named by `--config`; the result is
/// returned so a test can assert a refusal.
pub async fn boot_configured(
    state_dir: &std::path::Path,
    config: &std::path::Path,
) -> Result<mllm_cli::roles::App, mllm_cli::roles::StartError> {
    boot_configured_on(state_dir, config, engine_ports()).await
}

/// As [`boot_configured`], on the engine port range `ports`. The range is part
/// of the published host document, so a test that restarts the role passes
/// the same range again, as an operator's unchanged environment would.
pub async fn boot_configured_on(
    state_dir: &std::path::Path,
    config: &std::path::Path,
    ports: (u16, u16),
) -> Result<mllm_cli::roles::App, mllm_cli::roles::StartError> {
    mllm_cli::roles::start_standalone_configured(
        state_dir,
        Some(config),
        Arc::new(PortedProvider {
            ports,
            deep_park: false,
            members: None,
            models_root: None,
            kv_cache: None,
        }),
        test_memory(),
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

/// The capacity a test that drives the real `mllm` binary sizes its
/// deployment documents from. The binary observes this machine's own memory,
/// which a test cannot state, so the deployment is sized from a small explicit
/// capacity instead of from `/proc/meminfo`: its footprints then fit under the
/// limits standalone derives from any real machine's capacity and under the
/// memory free on a busy one (a document sized from the whole machine asked
/// for a fifth of it at cold start).
pub const BINARY_TEST_CAPACITY_BYTES: i64 = 4 << 30;

/// The template memory a test that drives the real `mllm` binary sizes its
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
pub fn binary_template_memory() -> mllm_cli::standalone_config::TemplateMemory {
    use mllm_agent::gpu_memory::{shape, GpuMemory, HostShape};
    match shape(mllm_agent::gpu_memory::sample().as_ref()) {
        Ok(HostShape::Discrete(_)) => {
            let card = GpuMemory {
                total_bytes: BINARY_TEST_DEVICE_BYTES,
                used_bytes: 0,
                free_bytes: BINARY_TEST_DEVICE_BYTES,
            };
            let limits = mllm_cli::standalone_config::device_limits(&card, 4);
            mllm_cli::standalone_config::TemplateMemory::Device {
                managed_limit: limits.managed_limit,
                device_total: card.total_bytes,
                weights_bytes: Some(0),
                system_parked_limit: BINARY_TEST_CAPACITY_BYTES / 4,
                kv_cache_bytes: None,
            }
        }
        _ => mllm_cli::standalone_config::TemplateMemory::Unified {
            capacity_bytes: BINARY_TEST_CAPACITY_BYTES,
        },
    }
}

/// The card a binary test sizes a discrete deployment for (see
/// [`binary_template_memory`]).
pub const BINARY_TEST_DEVICE_BYTES: i64 = 4 << 30;

pub fn test_memory() -> mllm_cli::host_observation::MemoryReader {
    mllm_cli::host_observation::fixed_memory(TEST_CAPACITY_BYTES, TEST_CAPACITY_BYTES)
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
    members: Option<Vec<mllm_domain::completion::ProcessIdentity>>,
    /// The model store, when the test states one.
    models_root: Option<std::path::PathBuf>,
    /// The KV cache the operator stated (`MLLM_KV_CACHE_BYTES`), if any.
    kv_cache: Option<&'static str>,
}

impl mllm_controller::EngineProvider for PortedProvider {
    fn installation(
        &self,
    ) -> Result<mllm_controller::EngineInstallation, mllm_controller::ProviderError> {
        let mut installation = mllm_testkit::fake_installation();
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
        clock: mllm_controller::coordinator::ServiceClock,
        log_dir: std::path::PathBuf,
        runtime_dir: std::path::PathBuf,
    ) -> Arc<dyn mllm_controller::coordinator::EngineBindings> {
        match &self.members {
            Some(members) => mllm_testkit::fake_bindings_with_members(
                clock,
                log_dir,
                runtime_dir,
                members.clone(),
            ),
            None => mllm_testkit::fake_bindings(clock, log_dir, runtime_dir),
        }
    }

    fn tools_factory(&self) -> mllm_controller::coordinator::ToolsFactory {
        mllm_testkit::fake_tools_factory()
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
    controller: &Arc<mllm_controller::CoordinatorLifecycle>,
    deployment: &str,
) -> tokio::task::JoinHandle<()> {
    use mllm_controller::LifecyclePort as _;
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
