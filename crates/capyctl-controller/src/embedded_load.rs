//! SPEC §§10, 17, ADR 0013 §10 (owner decision D9; owner decision
//! 2026-10-08): engine load of the standalone role's embedded engines.
//!
//! An enrolled host's agent samples its engines and reports them over its
//! control session into the server's [`LoadTable`]. The standalone role runs
//! its engines itself, with no agent, ingress or session, so it samples them in
//! process with the agent's own reporter ([`LoadReporter`]): every Ready
//! embedded launch with open dispatch, on its recorded loopback endpoint and
//! with its per-launch key, `/metrics` each tick and SGLang's `/v1/loads` once
//! per launch. Each report is validated and accepted exactly as a host's
//! (`LoadReport::try_from`, [`LoadTable::accept`]) under the embedded host's
//! name. A sample stays a routing hint and a status figure: never readiness,
//! admission or release evidence, and never journaled.
use crate::load_table::LoadTable;
use crate::ownership::SharedCoordinatorState;
use capyctl_agent::ingress::{IngressScope, LoadTarget};
use capyctl_agent::load::{LoadError, LoadReporter, LoadSource};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

/// The router's outstanding requests on one instance incarnation
/// (deployment, generation).
pub type InFlightCount = Arc<dyn Fn(&str, i64) -> usize + Send + Sync>;

/// The embedded host's Ready launches, read from the coordinator's store.
struct EmbeddedTargets {
    owner: SharedCoordinatorState,
    host_id: String,
    in_flight: InFlightCount,
}

impl EmbeddedTargets {
    /// This session's Ready embedded launches whose dispatch is open (as a
    /// host ingress reports only open scopes), each with its recorded endpoint
    /// and inference key read as one answer. A launch whose binding changed,
    /// whose endpoint names no IP address or that has no key is left out.
    fn launches(&self) -> Vec<LoadTarget> {
        let Ok(owner) = self.owner.lock() else {
            return Vec::new();
        };
        let store = owner.store();
        let Ok(launches) = store.local_ready_launches(owner.session()) else {
            return Vec::new();
        };
        launches
            .into_iter()
            .filter(|launch| launch.dispatch_enabled)
            .filter_map(|launch| {
                let binding = store
                    .retained_binding(&launch.binding_id)
                    .ok()
                    .flatten()
                    .filter(|b| b.id == launch.binding_id && b.incarnation == launch.incarnation)?;
                let url = crate::port::engine_url(&binding.endpoint)?;
                // An IPv6 host is bracketed in a URL.
                let ip: IpAddr = url
                    .host_str()?
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse()
                    .ok()?;
                let native = store
                    .engine_key(
                        &binding.id,
                        &binding.incarnation,
                        capyctl_store::secrets::SecretRole::Inference,
                    )
                    .ok()
                    .flatten()?;
                Some(LoadTarget {
                    scope: IngressScope {
                        host_id: self.host_id.clone(),
                        deployment_id: launch.fence.deployment_id,
                        // An embedded launch's one member is its binding.
                        member_id: launch.binding_id.clone(),
                        binding_id: launch.binding_id,
                        incarnation: launch.incarnation,
                        generation: launch.fence.generation,
                        revision: launch.fence.revision,
                        instance_index: launch.instance_index,
                    },
                    owned_handle: launch.step_id,
                    target: SocketAddr::new(ip, url.port_or_known_default()?),
                    native,
                    in_flight: 0,
                })
            })
            .collect()
    }
}

impl LoadSource for EmbeddedTargets {
    fn targets(&self) -> Vec<LoadTarget> {
        let mut targets = self.launches();
        // Read after the store lock is released: the router's count has its
        // own lock. The router is the role's only forwarding tier, so its
        // count stands where a host reports its ingress's.
        for target in &mut targets {
            target.in_flight =
                (self.in_flight)(&target.scope.deployment_id, target.scope.generation);
        }
        targets
    }
}

/// Samples the embedded engines and records them in its own [`LoadTable`].
pub struct EmbeddedLoad {
    reporter: LoadReporter,
    table: Arc<LoadTable>,
    host_id: String,
}

impl EmbeddedLoad {
    /// The embedded host `host_id` (its published name, the one its instances
    /// are placed on), sampled at the D9 default period.
    pub fn new(
        owner: SharedCoordinatorState,
        host_id: String,
        in_flight: InFlightCount,
    ) -> Result<Self, LoadError> {
        let source = Arc::new(EmbeddedTargets {
            owner,
            host_id: host_id.clone(),
            in_flight,
        });
        Ok(Self {
            reporter: LoadReporter::new(source, host_id.clone())?,
            table: Arc::new(LoadTable::new()),
            host_id,
        })
    }

    /// The table the management load read and the router read.
    pub fn table(&self) -> Arc<LoadTable> {
        self.table.clone()
    }

    /// One tick: sample every Ready embedded launch and record the samples.
    pub async fn tick(&self) {
        for report in self.reporter.reports().await {
            let Ok(report) = capyctl_protocol::reports::LoadReport::try_from(report) else {
                continue;
            };
            let _ = self
                .table
                .accept(&self.host_id, report, capyctl_protocol::now_unix_ms());
        }
    }

    /// Sample once per period until `cancel` reads true (or the task is
    /// aborted).
    pub fn spawn_until(
        self,
        mut cancel: tokio::sync::watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                self.tick().await;
                tokio::select! {
                    _ = crate::supervised::cancelled(&mut cancel) => break,
                    _ = tokio::time::sleep(self.reporter.interval()) => {}
                }
            }
        })
    }
}
