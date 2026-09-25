//! ADR 0018 §5: standalone's answers to `mllm engine add`, `remove` and
//! `list`, in one process. Add re-reads engines.yaml, rebuilds the embedded
//! host document and swaps it; remove retires through the store (the
//! ordinary stop path when drained) before rewriting engines.yaml. The
//! standalone document is never written. Nothing here reaches an engine.
use mllm_agent::control_socket::{ControlHandler, ControlRequest};
use mllm_config::registration::{
    check_profile, lock_engines, write_engines, EnginesFile, ENVIRONMENT_PROFILES,
};
use mllm_controller::coordinator::CoordinatorCommands;
use mllm_controller::engine_provider::{EngineProvider, NamedInstallation, ProviderError};
use mllm_controller::installation_gate::EmbeddedInstallations;
use mllm_controller::profile_retirement::{ProfileRetirements, RetirementStep, RETIREMENT_POLL};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::device_inventory::InventoryPublication;

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
}

impl EmbeddedHost {
    /// Register (measure) every installation and build the host document.
    /// Blocking: registration reads each installation's files.
    pub fn new(
        named: Vec<NamedInstallation>,
        environment_fingerprint: String,
        capacity_bytes: i64,
        inventory: Option<InventoryPublication>,
    ) -> Arc<Self> {
        let document = crate::standalone_config::host_policy(
            &named,
            &environment_fingerprint,
            capacity_bytes,
            inventory.as_ref(),
        );
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
        crate::standalone_config::host_policy(
            named,
            &self.environment_fingerprint,
            self.capacity_bytes,
            self.inventory.as_ref(),
        )
    }

    /// Swap in `named`: the installation registry first (so a launch on a new
    /// profile is measured), then the host document. Blocking.
    pub fn replace(&self, named: Vec<NamedInstallation>) -> Result<(), String> {
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

/// Re-read engines.yaml and publish it if it changed. Blocking.
fn reload(engines: &Path, provider: &dyn EngineProvider, host: &EmbeddedHost) -> Value {
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
        let (engines, provider, host) = (
            self.engines.clone(),
            self.provider.clone(),
            self.host.clone(),
        );
        tokio::task::spawn_blocking(move || reload(&engines, provider.as_ref(), &host))
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

    async fn remove(&self, profile: &str, drain: bool) -> Value {
        if ENVIRONMENT_PROFILES.contains(&profile) {
            return refused(
                "invalid_config",
                format!("{profile} comes from MLLM_VLLM_BIN / MLLM_SGLANG_BIN; unset it and restart the role"),
            );
        }
        match EnginesFile::load(&self.engines) {
            Ok(file) if file.profiles.contains_key(profile) => {}
            Ok(_) => {
                return refused(
                    "invalid_config",
                    format!("no registered profile named {profile}"),
                )
            }
            Err(e) => return refused("invalid_config", format!("{}: {}", e.path, e.detail)),
        }
        // The role keeps at least one engine: refused before anything is retired.
        if let Err(reply) = self.without(profile).await {
            return reply;
        }
        let key = format!("{}:{}", self.host_id, ulid::Ulid::new());
        // Owner decision 2026-09-25 (design rule 3): a published profile is
        // removed only once the store confirms nothing on this host uses it.
        let published = self.host.profiles().iter().any(|p| p == profile);
        if published {
            match self.retire(profile, &key, drain).await {
                RetirementStep::Confirmed => {}
                RetirementStep::InUse(deployments) | RetirementStep::Holding(deployments) => {
                    return json!({"ok": false, "code": "profile_in_use", "deployments": deployments,
                        "message": "deployments on this host use the profile; stop them, or use --drain"})
                }
                RetirementStep::Refused(reason) => return refused("publish_rejected", reason),
                RetirementStep::Draining(_) => {
                    return refused("internal", "the retirement did not finish")
                }
            }
        }
        let written = lock_engines(&self.engines).and_then(|lock| {
            let mut file = EnginesFile::load(&self.engines)?;
            file.profiles.remove(profile);
            write_engines(&file, &lock, None)
        });
        if let Err(e) = written {
            // The confirmed retirement stands, so nothing is placed on the
            // profile; a second `engine remove` finishes it.
            return refused("internal", format!("{}: {}", e.path, e.detail));
        }
        let mut reply = self.reload().await;
        if reply["ok"] == true {
            reply["removed"] = profile.into();
            // The profile is gone from the document now; the confirmed
            // retirement is cleared so the name can be registered again, as
            // an accepted publication clears it on a server.
            if published {
                let (host, name) = (self.host_id.clone(), profile.to_owned());
                let _ = self
                    .commands
                    .read(|store| store.cancel_profile_retirement(&host, &name, &key));
            }
        }
        reply
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
                    "engine": match n.installation.engine {
                        mllm_config::engine_policy::Engine::Vllm => "vllm",
                        mllm_config::engine_policy::Engine::Sglang => "sglang",
                    },
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
                self.remove(&profile, drain).await
            }
            ControlRequest::List => self.list(),
        }
    }
}
