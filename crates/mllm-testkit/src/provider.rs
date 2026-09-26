//! The Fake engine as an installation a host offers, and as the adapter a
//! frozen binding resolves to.
//!
//! A test that boots the whole graph needs an engine the test can predict. It
//! gets one here without the product gaining a Fake lane: the installation it
//! publishes is an ordinary vLLM installation, the launch plan is built from the
//! frozen profile exactly as production builds it, the per-launch engine key is
//! stored exactly as production stores it, and only the last step — constructing
//! the adapter — hands back a Fake instead of a vLLM adapter.
//!
//! Passing against this proves that mllm's own decisions are the expected ones.
//! It proves nothing about a native engine recipe (SPEC §18).

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use mllm_adapters::resolve::AdapterSpec;
use mllm_adapters::traits::{EngineAdapter, OwnedProcessLaunch, RuntimeError};
use mllm_config::engine_policy::Engine;
use mllm_controller::coordinator::{
    CoordinatorError, CoordinatorOptions, EngineBindings, OwnedCoordinator, ServiceClock,
    ServiceObservation, ToolsFactory,
};
use mllm_controller::engine_provider::{EngineInstallation, EngineProvider, ProviderError};
use mllm_controller::ownership::SharedCoordinatorState;
use mllm_controller::ProfileBindings;
use mllm_store::ordinary_lifecycle::worker::InitializeWork;

use crate::{FakeEngine, ScriptedTool};

/// A model store that exists for as long as the process does.
///
/// Spec §7 resolves a relative model path against the host's store, and the host
/// policy is refused unless the store is an absolute directory. Nothing reads a
/// weight out of it, so an empty temporary directory is enough — but it has to
/// outlive every test in the binary, or a later one would publish a policy naming
/// a directory that has already been removed.
fn models_root() -> PathBuf {
    static ROOT: OnceLock<tempfile::TempDir> = OnceLock::new();
    ROOT.get_or_init(|| tempfile::tempdir().expect("a temporary model store"))
        .path()
        .to_path_buf()
}

/// mllm's own guard middleware, in the checkout this test is running from.
///
/// It is a property of the installation rather than of the deployment, and the
/// repository's `runtime/` directory is the installation a developer has.
fn runtime_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../runtime")
        .canonicalize()
        .expect("the checkout carries runtime/")
}

/// The installation a test host publishes: a vLLM installation that starts
/// `/bin/true`, because the injected builder starts nothing at all.
pub fn fake_installation() -> EngineInstallation {
    EngineInstallation {
        engine: Engine::Vllm,
        executable: PathBuf::from("/bin/true"),
        build_fingerprint: "fake-v1".into(),
        engine_config: crate::vllm_engine_config_json(),
        kv_cache_declared: false,
        // SPEC §9.1, T21: this test host opts out of deep park; the product
        // default is enabled (owner decision 2026-09-17). Opting out is what keeps
        // a Fake-driven launch from rendering flags no Fake honours.
        deep_park: false,
        trust_remote_code: false,
        models_root: models_root(),
        runtime_dir: runtime_dir(),
        args: Vec::new(),
        installation_drift: Default::default(),
        cuda_home: None,
        engine_ports: (8100, 8199),
    }
}

/// Bindings that build every plan the product builds and then drive a Fake.
///
/// `clock` is the service's own: the Fake stamps the milestones it observes with
/// it, so its evidence is dated by the authority that reads it back.
pub fn fake_bindings(
    clock: ServiceClock,
    log_dir: PathBuf,
    runtime_dir: PathBuf,
) -> Arc<dyn EngineBindings> {
    Arc::new(FakeBindings {
        profile: ProfileBindings::new(log_dir, runtime_dir).with_device_totals(fake_cards()),
        clock,
        members: None,
    })
}

/// The card total every discrete GPU a Fake "runs on" states (16 GB), so the
/// product's plan for a launch on a device domain builds (discrete GPU design
/// §6). The Fake runs on no card; the total only has to be a card's.
pub const FAKE_CARD_BYTES: i64 = 16376 << 20;

/// [`FAKE_CARD_BYTES`] for the driver indices a test host names (`gpu0` to
/// `gpu7`).
fn fake_cards() -> std::collections::BTreeMap<u32, i64> {
    (0..8).map(|index| (index, FAKE_CARD_BYTES)).collect()
}

/// As [`fake_bindings`], with every Fake reporting `members` as its launched
/// group ([`FakeEngine::with_members`]), so the coordinator's park and restore
/// checks that the recorded processes are the ones alive can pass.
pub fn fake_bindings_with_members(
    clock: ServiceClock,
    log_dir: PathBuf,
    runtime_dir: PathBuf,
    members: Vec<mllm_domain::completion::ProcessIdentity>,
) -> Arc<dyn EngineBindings> {
    Arc::new(FakeBindings {
        profile: ProfileBindings::new(log_dir, runtime_dir).with_device_totals(fake_cards()),
        clock,
        members: Some(members),
    })
}

/// The recorded identity of the live process `pid` under `role`: this boot's
/// id and the process's start time, as a launcher records them.
pub fn live_identity(role: &str, pid: u32) -> mllm_domain::completion::ProcessIdentity {
    let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .expect("the boot id is readable")
        .trim()
        .to_owned();
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("the process is alive");
    let close = stat.rfind(')').expect("a stat line names its command");
    let start_ticks = stat[close + 2..]
        .split_whitespace()
        .nth(19)
        .and_then(|field| field.parse().ok())
        .expect("the stat line carries a start time");
    mllm_domain::completion::ProcessIdentity {
        role: role.to_owned(),
        pid,
        boot_id,
        start_ticks,
    }
}

/// Process tools for a builder that starts nothing.
///
/// The Fake launches no process, so there is never a recorded identity for these
/// to signal; they exist because the coordinator gives every launch its tools.
pub fn fake_tools_factory() -> ToolsFactory {
    Arc::new(|_association| ScriptedTool::proving())
}

/// A coordinator that drives the Fake on every binding it is given.
///
/// It is spawned the way the product spawns one: the bindings build the frozen
/// launch plan, the per-launch engine key is stored, the process tools are handed
/// to the builder and cleanup proves the recorded group gone. Only the adapter is
/// the Fake. A test therefore exercises the coordinator's own path rather than a
/// lane that exists for it.
pub fn spawn_fake_coordinator(
    owner: SharedCoordinatorState,
    observations: Arc<dyn ServiceObservation>,
    clock: ServiceClock,
    options: CoordinatorOptions,
) -> Result<OwnedCoordinator, CoordinatorError> {
    let bindings = fake_bindings(
        clock.clone(),
        std::env::temp_dir().join("mllm-testkit-logs"),
        runtime_dir(),
    );
    OwnedCoordinator::spawn_resolved(
        owner,
        observations,
        clock,
        options,
        bindings,
        fake_tools_factory(),
    )
}

/// A loopback origin nothing listens on: a model-source download a test
/// provider starts fails at once with `network` instead of reaching the
/// internet (ADR 0008; tests never download).
pub const NO_NETWORK_ORIGIN: &str = "http://127.0.0.1:9";

/// The Fake as a host's one engine installation.
pub fn fake_provider() -> Arc<dyn EngineProvider> {
    Arc::new(FakeProvider)
}

struct FakeProvider;

impl EngineProvider for FakeProvider {
    fn installation(&self) -> Result<EngineInstallation, ProviderError> {
        Ok(fake_installation())
    }

    fn bindings(
        &self,
        clock: ServiceClock,
        log_dir: PathBuf,
        runtime_dir: PathBuf,
    ) -> Arc<dyn EngineBindings> {
        fake_bindings(clock, log_dir, runtime_dir)
    }

    fn tools_factory(&self) -> ToolsFactory {
        fake_tools_factory()
    }

    fn model_source_origin(&self) -> Option<String> {
        Some(NO_NETWORK_ORIGIN.into())
    }
}

struct FakeBindings {
    profile: ProfileBindings,
    clock: ServiceClock,
    members: Option<Vec<mllm_domain::completion::ProcessIdentity>>,
}

impl EngineBindings for FakeBindings {
    /// The product's own spec, built from the frozen profile. Building it is the
    /// point: a test deployment that could not produce a real launch plan would
    /// pass here and refuse on the host.
    fn spec(&self, work: &InitializeWork) -> Result<AdapterSpec, CoordinatorError> {
        self.profile.spec(work)
    }

    /// The Fake, ignoring the plan the spec carries.
    fn adapter(
        &self,
        _declared: Engine,
        _spec: AdapterSpec,
        _tools: Arc<dyn OwnedProcessLaunch>,
    ) -> Result<Arc<dyn EngineAdapter>, CoordinatorError> {
        let clock = self.clock.clone();
        let engine = FakeEngine::with_lifecycle_clock(Arc::new(move || {
            clock().map_err(|_| RuntimeError::Uncertain("service observation clock failed".into()))
        }));
        Ok(Arc::new(match &self.members {
            Some(members) => engine.with_members(members.clone()),
            None => engine,
        }))
    }
}
