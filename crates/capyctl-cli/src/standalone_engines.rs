//! ADR 0018 §5: standalone's answers to `capyctl engine add`, `remove` and
//! `list`, in one process. Add re-reads engines.yaml, rebuilds the embedded
//! host document and swaps it; remove retires through the store (the
//! ordinary stop path when drained). review decision C1: the role never
//! writes engines.yaml; the CLI rewrites it once the retirement is confirmed
//! and then asks for the reload. The standalone document is never written.
//! Nothing here reaches an engine.
use capyctl_agent::control_socket::{ControlHandler, ControlRequest};
use capyctl_config::model_settings::ModelSettings;
use capyctl_config::registration::{check_profile, EnginesFile, ENVIRONMENT_PROFILES};
use capyctl_controller::coordinator::CoordinatorCommands;
use capyctl_controller::engine_provider::{EngineProvider, NamedInstallation, ProviderError};
use capyctl_controller::installation_gate::EmbeddedInstallations;
use capyctl_controller::profile_retirement::{ProfileRetirements, RetirementStep, RETIREMENT_POLL};
use capyctl_store::host_publication::RepublishRefusal;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use crate::device_inventory::InventoryPublication;
use capyctl_agent::gpu_memory::HostShape;

/// `named` with the resolved models directory: a provider's fresh
/// installations name none of their own (owner decision 2026-09-25).
fn with_models(
    mut named: Vec<NamedInstallation>,
    models: &ModelSettings,
) -> Vec<NamedInstallation> {
    for n in &mut named {
        n.installation.models_root = models.models_root.clone();
    }
    named
}

/// What the embedded host publishes, and how to rebuild it.
pub struct EmbeddedHost {
    /// The host document the management configuration source composes every
    /// deployment against (`SharedConfigurationSource::new_shared`).
    document: Arc<RwLock<Value>>,
    installations: Arc<EmbeddedInstallations>,
    named: RwLock<Vec<NamedInstallation>>,
    environment_fingerprint: String,
    capacity_bytes: i64,
    inventory: Option<InventoryPublication>,
    /// The GPU shape sampled at boot (design §1), which decides the domains.
    shape: HostShape,
    /// The models directory and model-source policy resolved at boot (owner
    /// decision 2026-09-25), stated in every document and installation.
    models: ModelSettings,
}

impl EmbeddedHost {
    /// Register (measure) every installation and build the host document.
    /// Blocking: registration reads each installation's files.
    pub fn new(
        named: Vec<NamedInstallation>,
        environment_fingerprint: String,
        capacity_bytes: i64,
        inventory: Option<InventoryPublication>,
        shape: HostShape,
        models: ModelSettings,
    ) -> Arc<Self> {
        let named = with_models(named, &models);
        let mut document = crate::standalone_config::host_policy(
            &named,
            &environment_fingerprint,
            capacity_bytes,
            inventory.as_ref(),
            &shape,
        );
        models.write_into(&mut document);
        let installations = EmbeddedInstallations::new();
        for n in &named {
            installations.register(
                &n.profile,
                n.installation.engine,
                &n.installation.executable,
            );
        }
        Arc::new(Self {
            document: Arc::new(RwLock::new(document)),
            installations,
            named: RwLock::new(named),
            environment_fingerprint,
            capacity_bytes,
            inventory,
            shape,
            models,
        })
    }

    /// The published profile names, in publication order.
    pub fn profiles(&self) -> Vec<String> {
        self.named
            .read()
            .map(|n| n.iter().map(|i| i.profile.clone()).collect())
            .unwrap_or_default()
    }

    /// The installations the host publishes now.
    pub fn named(&self) -> Vec<NamedInstallation> {
        self.named.read().map(|n| n.clone()).unwrap_or_default()
    }

    /// A copy of the current host document.
    pub fn document(&self) -> Value {
        self.document
            .read()
            .map(|d| d.clone())
            .unwrap_or(Value::Null)
    }

    /// The shared document, for the management configuration source.
    pub fn shared_document(&self) -> Arc<RwLock<Value>> {
        self.document.clone()
    }

    pub fn installations(&self) -> Arc<EmbeddedInstallations> {
        self.installations.clone()
    }

    /// The document `named` would publish.
    fn document_for(&self, named: &[NamedInstallation]) -> Value {
        let mut document = crate::standalone_config::host_policy(
            named,
            &self.environment_fingerprint,
            self.capacity_bytes,
            self.inventory.as_ref(),
            &self.shape,
        );
        self.models.write_into(&mut document);
        document
    }

    /// Swap in `named`: the installation registry first (so a launch on a new
    /// profile is measured), then the host document. Blocking.
    pub fn replace(&self, named: Vec<NamedInstallation>) -> Result<(), String> {
        let named = with_models(named, &self.models);
        let document = self.document_for(&named);
        for n in &named {
            self.installations.register(
                &n.profile,
                n.installation.engine,
                &n.installation.executable,
            );
        }
        let executables: Vec<&Path> = named
            .iter()
            .map(|n| n.installation.executable.as_path())
            .collect();
        self.installations.retain(&executables);
        *self
            .document
            .write()
            .map_err(|_| "host document lock poisoned")? = document;
        *self
            .named
            .write()
            .map_err(|_| "installation list lock poisoned")? = named;
        Ok(())
    }
}

/// The standalone role's control-socket handler.
pub struct StandaloneControl {
    /// The role's engines.yaml (`engines_path` of its `--config`).
    engines: PathBuf,
    provider: Arc<dyn EngineProvider>,
    host: Arc<EmbeddedHost>,
    retirements: Arc<dyn ProfileRetirements>,
    commands: CoordinatorCommands,
    /// The embedded host's published name.
    host_id: String,
    /// One add or remove at a time; `list` never waits for them.
    mutation: tokio::sync::Mutex<()>,
}

fn refused(code: &str, message: impl Into<String>) -> Value {
    json!({"ok": false, "code": code, "message": message.into()})
}

/// What `registered` plus the environment would publish, or the refusal.
fn resolve(
    provider: &dyn EngineProvider,
    registered: &Map<String, Value>,
) -> Result<Vec<NamedInstallation>, Value> {
    for (name, profile) in registered {
        check_profile(name, profile).map_err(|e| {
            refused(
                "publish_rejected",
                format!("profile {name}: {}: {}", e.path, e.detail),
            )
        })?;
    }
    provider.installations(registered).map_err(|e| match e {
        ProviderError::ProfileExists(name) => refused(
            "profile_exists",
            format!("{name} is already an environment profile; remove it and register it under another --name"),
        ),
        other => refused("publish_rejected", other.to_string()),
    })
}

/// How often a running standalone expires abandoned retirements.
pub const RETIREMENT_EXPIRY_TICK: Duration = Duration::from_secs(30);

/// ADR 0018 §4, §5 (review decisions I2, I3): at standalone's start, what a
/// server does at a host's start. A retirement abandoned past its deadline
/// (the role stopped mid-drain) ends unconfirmed, and the embedded host's
/// profiles are recorded as its publication, which keeps a profile it no
/// longer publishes out of placement and clears the confirmed retirement of
/// every profile it does not list.
pub fn publish_at_start(
    commands: &CoordinatorCommands,
    host_id: &str,
    profiles: &[String],
) -> Result<(), String> {
    let now = capyctl_protocol::now_unix_ms();
    commands
        .read(|store| {
            store.expire_profile_retirements(now)?;
            store
                .publish_embedded_profiles(host_id, profiles, now, true)
                .map_err(|_| capyctl_store::StoreError::Conflict)
        })
        .map(|_| ())
        .map_err(|_| "the embedded host's profiles could not be recorded".to_owned())
}

/// review decision I2: while standalone runs, expire abandoned retirements
/// as the server does before each placement, until `shutdown`.
pub async fn expire_retirements(
    commands: CoordinatorCommands,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { return }
            }
            () = tokio::time::sleep(RETIREMENT_EXPIRY_TICK) => {
                let now = capyctl_protocol::now_unix_ms();
                let _ = commands.read(|store| store.expire_profile_retirements(now));
            }
        }
    }
}

/// Re-read engines.yaml and publish it if it changed. Blocking.
fn reload(
    engines: &Path,
    provider: &dyn EngineProvider,
    host: &EmbeddedHost,
    commands: &CoordinatorCommands,
    host_id: &str,
) -> Value {
    let registered = match EnginesFile::load(engines) {
        Ok(file) => file.profiles,
        Err(e) => return refused("invalid_config", format!("{}: {}", e.path, e.detail)),
    };
    let named = match resolve(provider, &registered) {
        Ok(named) => named,
        Err(reply) => return reply,
    };
    if host.document_for(&named) == host.document() {
        return json!({"ok": true, "published": "unchanged"});
    }
    // ADR 0018 §4 (review decision I3): the server's rule. A published
    // profile leaves the embedded host only after its retirement was
    // confirmed; the previous publication stays otherwise.
    let profiles: Vec<String> = named.iter().map(|n| n.profile.clone()).collect();
    let now = capyctl_protocol::now_unix_ms();
    match commands.read(|store| Ok(store.publish_embedded_profiles(host_id, &profiles, now, false)))
    {
        Ok(Ok(())) => {}
        Ok(Err(RepublishRefusal::NotRetired(name))) => {
            return refused(
                "publish_rejected",
                format!("engines.yaml no longer declares {name}, which is published; only `capyctl engine remove {name}` drops a published profile"),
            )
        }
        _ => return refused("internal", "the embedded host's profiles could not be recorded"),
    }
    match host.replace(named) {
        Ok(()) => json!({"ok": true, "published": "published"}),
        Err(e) => refused("internal", e),
    }
}

impl StandaloneControl {
    pub fn new(
        engines: PathBuf,
        provider: Arc<dyn EngineProvider>,
        host: Arc<EmbeddedHost>,
        retirements: Arc<dyn ProfileRetirements>,
        commands: CoordinatorCommands,
        host_id: String,
    ) -> Arc<Self> {
        Arc::new(Self {
            engines,
            provider,
            host,
            retirements,
            commands,
            host_id,
            mutation: tokio::sync::Mutex::new(()),
        })
    }

    async fn reload(&self) -> Value {
        let (engines, provider, host, commands, host_id) = (
            self.engines.clone(),
            self.provider.clone(),
            self.host.clone(),
            self.commands.clone(),
            self.host_id.clone(),
        );
        tokio::task::spawn_blocking(move || {
            reload(&engines, provider.as_ref(), &host, &commands, &host_id)
        })
        .await
        .unwrap_or_else(|_| refused("internal", "reloading the engines failed"))
    }

    /// engines.yaml without `profile`, checked to still publish something.
    async fn without(&self, profile: &str) -> Result<(), Value> {
        let (engines, provider, name) = (
            self.engines.clone(),
            self.provider.clone(),
            profile.to_owned(),
        );
        tokio::task::spawn_blocking(move || {
            let mut registered = EnginesFile::load(&engines)
                .map_err(|e| refused("invalid_config", format!("{}: {}", e.path, e.detail)))?
                .profiles;
            registered.remove(&name);
            resolve(provider.as_ref(), &registered).map(|_| ())
        })
        .await
        .unwrap_or_else(|_| Err(refused("internal", "checking the engines failed")))
    }

    /// Phase one and, with `drain`, the ordinary stops until a terminal step.
    async fn retire(&self, profile: &str, key: &str, drain: bool) -> RetirementStep {
        let (service, host, name, k) = (
            self.retirements.clone(),
            self.host_id.clone(),
            profile.to_owned(),
            key.to_owned(),
        );
        let mut step = tokio::task::spawn_blocking(move || service.begin(&host, &name, &k, drain))
            .await
            .unwrap_or_else(|_| RetirementStep::Refused("the retirement failed".into()));
        // Bounded: the service answers `Holding` once its window passes.
        while let RetirementStep::Draining(_) = step {
            tokio::time::sleep(RETIREMENT_POLL).await;
            let (service, host, name, k) = (
                self.retirements.clone(),
                self.host_id.clone(),
                profile.to_owned(),
                key.to_owned(),
            );
            if let Ok(Some(next)) =
                tokio::task::spawn_blocking(move || service.poll(&host, &name, &k)).await
            {
                step = next;
            }
        }
        step
    }

    /// ADR 0018 §4, phase one for `capyctl engine remove`: retire `profile` if
    /// the embedded host publishes it. `{"retired": true}` once the store
    /// confirmed nothing uses it, `{"retired": false}` when it is not
    /// published. Nothing is written here (review decision C1).
    async fn retire_for_removal(&self, profile: &str, drain: bool) -> Value {
        if ENVIRONMENT_PROFILES.contains(&profile) {
            return refused(
                "invalid_config",
                format!("{profile} comes from the role's own installation (--vllm-bin / --sglang-bin / --tensorfold-bin, CAPYCTL_VLLM_BIN / CAPYCTL_SGLANG_BIN / CAPYCTL_TENSORFOLD_BIN or host.local_engine); unset it and restart the role"),
            );
        }
        // The role keeps at least one engine: refused before anything is retired.
        if let Err(reply) = self.without(profile).await {
            return reply;
        }
        if !self.host.profiles().iter().any(|p| p == profile) {
            return json!({"ok": true, "retired": false});
        }
        // review decision I1: a retirement already standing for the
        // profile is resumed under its own key, so a retried remove finishes
        // it instead of conflicting.
        let key = format!("{}:{}", self.host_id, ulid::Ulid::new());
        // Owner decision 2026-09-25 (design rule 3): a published profile is
        // removed only once the store confirms nothing on this host uses it.
        match self.retire(profile, &key, drain).await {
            RetirementStep::Confirmed => json!({"ok": true, "retired": true}),
            RetirementStep::InUse(deployments) | RetirementStep::Holding(deployments) => {
                json!({"ok": false, "code": "profile_in_use", "deployments": deployments,
                    "message": "deployments on this host use the profile; stop them, or use --drain"})
            }
            RetirementStep::Refused(reason) => refused("publish_rejected", reason),
            RetirementStep::Draining(_) => refused("internal", "the retirement did not finish"),
        }
    }

    fn list(&self) -> Value {
        let mut accepted = Map::new();
        let mut users = Map::new();
        for n in self.host.named() {
            let view = self
                .host
                .installations
                .for_executable(&n.installation.executable)
                .map(|i| i.view())
                .unwrap_or(Value::Null);
            accepted.insert(
                n.profile.clone(),
                json!({
                    "engine": n.installation.engine.name(),
                    "executable": n.installation.executable,
                    "build_fingerprint": n.installation.build_fingerprint,
                    "installation": {"version": view["version"], "digest": view["digest"], "state": view["state"]},
                    "deep_park": if n.installation.deep_park { "enabled" } else { "disabled" },
                    "deep_park_probe": "unknown",
                }),
            );
            let (host, name) = (self.host_id.clone(), n.profile.clone());
            if let Ok(found) = self
                .commands
                .read(|store| store.profile_candidates(&host, &name))
            {
                let mut names: Vec<String> = found.into_iter().map(|c| c.name).collect();
                names.dedup();
                if !names.is_empty() {
                    users.insert(n.profile.clone(), json!(names));
                }
            }
        }
        json!({"ok": true, "connected": true, "live_profile_update": true,
            "accepted": accepted, "users": users})
    }
}

#[async_trait::async_trait]
impl ControlHandler for StandaloneControl {
    async fn handle(&self, request: ControlRequest) -> Value {
        match request {
            ControlRequest::Add => {
                let _one = self.mutation.lock().await;
                self.reload().await
            }
            ControlRequest::Remove { profile, drain } => {
                let _one = self.mutation.lock().await;
                self.retire_for_removal(&profile, drain).await
            }
            ControlRequest::List => self.list(),
        }
    }
}
