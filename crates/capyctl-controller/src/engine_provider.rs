//! Where a host's engine installations come from.
//!
//! The lifecycle needs three things that are properties of the installation rather
//! than of any deployment: what the host may publish about the engine it has, how a
//! frozen binding becomes an adapter spec, and which process tools a launch is given.
//! A provider supplies all three together, so a caller cannot publish a policy for
//! one engine and then drive a different one.
//!
//! The trait lives here rather than in the CLI because the test double and the
//! environment-driven implementation have no reason to share a crate, and because
//! nothing that implements it should have to depend on a command-line binary.

use std::path::PathBuf;
use std::sync::Arc;

use capyctl_config::engine_policy::Engine;

use crate::coordinator::{EngineBindings, ServiceClock, ToolsFactory};

/// One engine a host actually has, described in the terms the host policy publishes.
///
/// Spec §7: everything the published table needs. ADR 0014 §1 moved engine
/// tuning to the deployment; the one exception is [`Self::engine_config`], the
/// block standalone's generated deployment carries, because standalone has no
/// deployment file and its environment describes both documents.
#[derive(Debug, Clone)]
pub struct EngineInstallation {
    /// The engine family this installation is.
    pub engine: Engine,
    /// The program that starts it.
    pub executable: PathBuf,
    /// What the host says this build is. It pins the recipe, so it must identify
    /// the installed engine rather than the host that happens to run it.
    pub build_fingerprint: String,
    /// The `engine_config` block (ADR 0014 §2) of the deployment standalone
    /// generates, as JSON: the configuration layer validates it, so a typed
    /// value here would duplicate that validation where it could drift. Not
    /// part of the published host policy.
    pub engine_config: serde_json::Value,
    /// Whether the operator stated the KV cache in `engine_config`
    /// (`CAPYCTL_KV_CACHE_BYTES`) rather than taking the unified default. A
    /// discrete host sizes its template's KV cache from the card unless the
    /// operator stated one, which it then honours within the card or refuses
    /// (review decision, discrete GPU design §3).
    pub kv_cache_declared: bool,
    /// Whether the deep-park controls may be called on this engine (SPEC §9.1, T21).
    pub deep_park: bool,
    /// Whether this installation may run an engine flag that executes Python
    /// shipped inside a checkpoint (Spec §3).
    pub trust_remote_code: bool,
    /// The directory the host keeps model weights under. Spec §7 resolves a
    /// relative model path against it. Empty when the installation does not
    /// name one: the role then resolves `model_store.path` or `~/models`
    /// (owner decision 2026-09-25, `capyctl_config::model_settings`).
    pub models_root: PathBuf,
    /// Where capyctl's own guard middleware lives. It is not part of the frozen
    /// effective configuration: it is a property of this installation.
    pub runtime_dir: PathBuf,
    /// Startup flags the profile passes to the engine, beyond the ones capyctl owns.
    /// ADR 0014 §1: host-fixed arguments belong to the installation; deployment
    /// arguments go in `engine_config.extra_args`.
    pub args: Vec<String>,
    /// ADR 0008 (owner decision 2026-09-23): what a launch does when this
    /// installation no longer measures to the fingerprint registered at boot
    /// (`security.installation_drift`, default `warn`).
    pub installation_drift: capyctl_config::effective::InstallationDrift,
    /// SPEC §13.3 amendment (owner decision 2026-09-25): the CUDA toolkit root
    /// published as the profile's `cuda_home` (`<cuda_home>/bin` joins the
    /// engine PATH). `None` keeps the minimal PATH.
    pub cuda_home: Option<PathBuf>,
    /// SPEC §3: the loopback ports this host leases its engines, inclusive
    /// (`resource_policy.endpoint_port_range`). A second role on the same
    /// machine names its own range so their engines never collide.
    pub engine_ports: (u16, u16),
}

/// The settings a role holds whether or not it has an engine: every
/// installation it publishes shares them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleSettings {
    pub runtime_dir: PathBuf,
    pub engine_ports: (u16, u16),
    /// Empty when the role names none (see [`EngineInstallation::models_root`]).
    pub models_root: PathBuf,
    pub cuda_home: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// Nothing on this host names an engine that could be started. The message
    /// says what was expected, because a bare refusal leaves an operator guessing.
    #[error("{0}")]
    NoEngineInstallation(String),
    /// ADR 0018 §5: an environment-variable profile and a registered one share
    /// a name. Refused at start rather than letting one shadow the other.
    #[error("profile {0} is declared twice (an environment variable and engines.yaml); rename the registered one")]
    ProfileExists(String),
}

/// ADR 0018 §5: one engine installation under the profile name it publishes.
#[derive(Debug, Clone)]
pub struct NamedInstallation {
    pub profile: String,
    pub installation: EngineInstallation,
}

/// ADR 0018 §5: a registered profile (engines.yaml) over the role's settings
/// (models root, ports, KV default, runtime directory). The profile states the
/// engine, its executable, its version, its security switches and its args.
pub fn from_profile(base: &EngineInstallation, profile: &serde_json::Value) -> EngineInstallation {
    let engine = profile["engine"]
        .as_str()
        .and_then(Engine::from_name)
        .unwrap_or(Engine::Vllm);
    EngineInstallation {
        engine,
        executable: profile["executable"].as_str().unwrap_or_default().into(),
        build_fingerprint: profile["build_fingerprint"]
            .as_str()
            .unwrap_or("unknown")
            .into(),
        // SPEC §9.1 / ADR 0012: deep parking is on unless the profile opts out.
        deep_park: profile["security"]["deep_park"].as_str() != Some("disabled"),
        trust_remote_code: profile["security"]["trust_remote_code"] == true,
        installation_drift: if profile["security"]["installation_drift"].as_str() == Some("refuse")
        {
            capyctl_config::effective::InstallationDrift::Refuse
        } else {
            capyctl_config::effective::InstallationDrift::Warn
        },
        args: profile["args"]
            .as_array()
            .map(|args| {
                args.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        cuda_home: profile["cuda_home"].as_str().map(PathBuf::from),
        ..base.clone()
    }
}

/// Supplies the one engine installation a host offers, and the two seams the
/// coordinator needs in order to drive it.
pub trait EngineProvider: Send + Sync {
    /// What this host has, or a refusal naming what it expected to find.
    fn installation(&self) -> Result<EngineInstallation, ProviderError>;

    /// The role-level settings, which a role with no engine still has. A
    /// provider that states one installation takes them from it.
    fn role_settings(&self) -> Result<RoleSettings, ProviderError> {
        let base = self.installation()?;
        Ok(RoleSettings {
            runtime_dir: base.runtime_dir,
            engine_ports: base.engine_ports,
            models_root: base.models_root,
            cuda_home: base.cuda_home,
        })
    }

    /// ADR 0018 §5: every installation this host publishes: the environment's
    /// (`local`) and the profiles registered in engines.yaml, which reuse its
    /// role settings. A name declared twice is refused.
    fn installations(
        &self,
        registered: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Vec<NamedInstallation>, ProviderError> {
        let base = self.installation()?;
        let mut all = vec![NamedInstallation {
            profile: "local".into(),
            installation: base.clone(),
        }];
        for (name, profile) in registered {
            if all.iter().any(|n| &n.profile == name) {
                return Err(ProviderError::ProfileExists(name.clone()));
            }
            all.push(NamedInstallation {
                profile: name.clone(),
                installation: from_profile(&base, profile),
            });
        }
        Ok(all)
    }

    /// How a frozen binding becomes an adapter spec. `log_dir` is where each
    /// engine's own output is written and `runtime_dir` holds the guard middleware.
    fn bindings(
        &self,
        clock: ServiceClock,
        log_dir: PathBuf,
        runtime_dir: PathBuf,
    ) -> Arc<dyn EngineBindings>;

    /// [`Self::bindings`] for a host whose discrete GPUs have these totals, by
    /// driver index (discrete GPU design §6): an engine on a device domain is
    /// sized against its card's total. A provider whose bindings size nothing
    /// against a card keeps the default.
    fn bindings_for_devices(
        &self,
        clock: ServiceClock,
        log_dir: PathBuf,
        runtime_dir: PathBuf,
        _device_totals: std::collections::BTreeMap<u32, i64>,
    ) -> Arc<dyn EngineBindings> {
        self.bindings(clock, log_dir, runtime_dir)
    }

    /// The process tools a launch is given, built per launch around the
    /// association that records its API identity (Spec §3).
    fn tools_factory(&self) -> ToolsFactory;

    /// Owner rule 2026-09-25 (every setting three ways): the role document's
    /// `host:` block, whose `local_engine`, `runtime_dir` and
    /// `resource_policy.endpoint_port_range` are the YAML layer of the
    /// installation settings (flag > environment > YAML > default). Called once
    /// at boot before [`Self::installations`]. A provider that states its
    /// installation itself (a test double) ignores it.
    fn configure(&self, _host: &serde_json::Value) -> Result<(), ProviderError> {
        Ok(())
    }

    /// Test seam: a loopback `http://127.0.0.1:<port>` origin that serves
    /// every model-source download instead of the network (ADR 0008). A
    /// production provider names none.
    #[doc(hidden)]
    fn model_source_origin(&self) -> Option<String> {
        None
    }
}
