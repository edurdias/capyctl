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
//! admission or release evidence, and never journaled. SPEC §17 (M80): the
//! engine latency histograms each report carries are added to the role's own
//! [`LatencyTable`] ([`LatencyTable::accept`]), as a server adds a host's, so
//! the latency read shows `source: engine` series on standalone too.
use crate::latency_table::LatencyTable;
use crate::load_table::LoadTable;
use crate::ownership::SharedCoordinatorState;
use capyctl_agent::ingress::{IngressScope, LoadTarget};
use capyctl_agent::load::{LmcacheTarget, LoadError, LoadReporter, LoadSource};
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
    /// and inference key read as one answer. A launch whose binding changed or
    /// whose endpoint names no IP address is left out. A launch with no
    /// recorded key is sampled without one: an embedded llama.cpp launch takes
    /// none (ADR 0029 §4; found live 2026-10-09, when its load read stayed
    /// `sample: null`), and any other engine refuses an unkeyed scrape.
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
                    .ok()?;
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
                    signal: capyctl_agent::load::LaunchSignal::Builtin,
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

    /// ADR 0033 §12 (lifecycle design §8): every Ready or parked embedded
    /// launch that runs an LMCache server, read on the server's loopback
    /// admin port as a host reads its own. The server's GPU memory is
    /// unknown here; its resident memory is read with the sample.
    fn lmcache_targets(&self) -> Vec<LmcacheTarget> {
        let Ok(owner) = self.owner.lock() else {
            return Vec::new();
        };
        let Ok(launches) = owner.store().local_lmcache_launches(owner.session()) else {
            return Vec::new();
        };
        launches
            .into_iter()
            .map(|launch| LmcacheTarget {
                deployment_id: launch.fence.deployment_id,
                instance_index: launch.instance_index,
                generation: launch.fence.generation,
                owned_handle: launch.step_id,
                http: (std::net::Ipv4Addr::LOCALHOST, launch.http_port).into(),
                server: launch.server,
                pool_bytes: launch.pool_bytes,
                device_bytes: None,
            })
            .collect()
    }
}

/// Samples the embedded engines and records them in its own [`LoadTable`] and
/// [`LatencyTable`].
pub struct EmbeddedLoad {
    reporter: LoadReporter,
    table: Arc<LoadTable>,
    latency: Arc<LatencyTable>,
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
            latency: Arc::new(LatencyTable::new()),
            host_id,
        })
    }

    /// The table the management load read and the router read.
    pub fn table(&self) -> Arc<LoadTable> {
        self.table.clone()
    }

    /// SPEC §17 (M80): the engine latency the embedded engines reported, for
    /// the management latency read and the timing header's engine family.
    pub fn latency_table(&self) -> Arc<LatencyTable> {
        self.latency.clone()
    }

    /// One tick: sample every Ready embedded launch and record its sample and
    /// engine latency, as a server records a host's report (latency first,
    /// then load, `agent_sessions`).
    pub async fn tick(&self) {
        for report in self.reporter.reports().await {
            let Ok(report) = capyctl_protocol::reports::LoadReport::try_from(report) else {
                continue;
            };
            let now = capyctl_protocol::now_unix_ms();
            self.latency.accept(&self.host_id, &report, now);
            let _ = self.table.accept(&self.host_id, report, now);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load_table::InstanceKey;
    use capyctl_domain::completion::{
        CompletionEvidence, EffectObservation, Milestone, OwnedLaunchReceipt, ProcessIdentity,
    };
    use capyctl_scheduler::residency::AdmissionContext;
    use capyctl_store::lifecycle::DeploymentFence;
    use capyctl_store::ordinary_lifecycle::lmcache::LmcacheOutcome;
    use capyctl_store::ordinary_lifecycle::park::{ResidencyArm, ResidencyKind};
    use capyctl_testkit::fixture::{self, Fixture};
    use serde_json::{json, Value};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    const GIB: i64 = 1 << 30;
    const DIGEST: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
    const NOW: i64 = 1_500;

    /// The standalone host as both its policy and its deployments resolve
    /// it: vLLM 0.32.0 with LMCache 0.5.6 registered (ADR 0033 §1), room for
    /// four launches, its engine and server ports from `ports`.
    fn lmcache_host(host: &mut Value, ports: (u16, u16)) {
        let profile = &mut host["runtime_profiles"]["local"];
        profile["build_fingerprint"] = json!("0.32.0");
        profile["lmcache"] = json!({"version": "0.5.6", "digest": DIGEST});
        let policy = &mut host["resource_policy"];
        policy["domains"]["unified"]["managed_limit"] = json!("96GiB");
        policy["domains"]["unified"]["host_kv_limit"] = json!("16GiB");
        policy["domains"]["unified"]["parked_limit"] = json!("16GiB");
        policy["endpoint_port_range"] = json!({"start": ports.0, "end": ports.1});
    }

    /// A `restart_only` deployment with a 1 GiB pool, so LMCache plans on.
    fn deploy(f: &Fixture, name: &str, ports: (u16, u16)) -> DeploymentFence {
        fixture::managed_edit(f, name, |deployment, host| {
            lmcache_host(host, ports);
            deployment["residency"] = json!("restart_only");
            deployment["lmcache_pool"] = json!("1GiB");
        })
    }

    struct Lab {
        _dir: tempfile::TempDir,
        owner: SharedCoordinatorState,
        observations: Vec<capyctl_domain::resources::MemoryObservation>,
        limits: Vec<capyctl_domain::resources::MemoryLimit>,
        fences: Vec<DeploymentFence>,
    }

    /// The coordinator's state, as the standalone role opens it, holding
    /// one stopped deployment per name.
    fn lab(names: &[&str]) -> Lab {
        let ports = capyctl_testkit::ports::free_ports(32, true);
        let ports = (ports[0], ports[31]);
        let f = fixture::fixture_with(|_, host| lmcache_host(host, ports));
        let fences = names.iter().map(|n| deploy(&f, n, ports)).collect();
        let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("srv.sqlite3");
        f.sql
            .execute("VACUUM INTO ?1", [path.to_str().unwrap()])
            .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let owner = Arc::new(Mutex::new(
            crate::ownership::OwnedCoordinatorState::open(dir.path()).unwrap(),
        ));
        Lab {
            _dir: dir,
            owner,
            observations: f
                .observations
                .iter()
                .map(|o| capyctl_domain::resources::MemoryObservation {
                    sampled_at_ms: NOW,
                    ..o.clone()
                })
                .collect(),
            limits: f.limits.clone(),
            fences,
        }
    }

    fn identity(role: &str, pid: u32) -> ProcessIdentity {
        ProcessIdentity {
            role: role.into(),
            pid,
            boot_id: "boot-1".into(),
            start_ticks: 1,
        }
    }

    impl Lab {
        fn admission(&self) -> AdmissionContext<'_> {
            AdmissionContext::new(&self.observations, &self.limits, NOW, 2_000, 16)
        }

        /// Start `fence` on this session; armed, not yet Ready.
        fn start(
            &self,
            fence: &DeploymentFence,
        ) -> capyctl_domain::completion::StepExecutionContext {
            let o = self.owner.lock().unwrap();
            let accepted = o
                .store()
                .accept_start(o.session(), fence, NOW, NOW + 100_000)
                .unwrap();
            o.store()
                .arm_step(o.session(), &accepted.step_id, self.admission())
                .unwrap();
            o.store()
                .initialize_execution(o.session(), &accepted.step_id)
                .unwrap()
        }

        /// Bring `fence` Ready after its host reported `outcome`; the engine
        /// from `pid`, and with LMCache on its server as `lmcache` `pid + 50`.
        fn ready(
            &self,
            fence: &DeploymentFence,
            outcome: LmcacheOutcome,
            pid: u32,
        ) -> capyctl_domain::completion::StepExecutionContext {
            let context = self.start(fence);
            let o = self.owner.lock().unwrap();
            let mut identities = vec![identity("api", pid), identity("worker-0", pid + 1)];
            if outcome == LmcacheOutcome::On {
                let server = identity("lmcache", pid + 50);
                o.store()
                    .record_launch_identity(o.session(), fence, &context.binding_id, &server)
                    .unwrap();
                identities.push(server);
            }
            o.store()
                .record_launch_lmcache(o.session(), &context.binding_id, &outcome, NOW)
                .unwrap();
            o.store()
                .record_owned_launch(
                    o.session(),
                    &context.token.step_id,
                    &OwnedLaunchReceipt {
                        binding_id: context.binding_id.clone(),
                        incarnation: context.incarnation.clone(),
                        identities: identities.clone(),
                        observed_at_ms: NOW,
                        receipt: "fake ready".into(),
                    },
                    NOW,
                )
                .unwrap();
            o.store()
                .complete_step(
                    o.session(),
                    &context.token.step_id,
                    &CompletionEvidence {
                        token: context.token.clone(),
                        identities,
                        observed_at_ms: NOW,
                        control_receipt: Some("fake ready".into()),
                        milestones: ResidencyKind::Restore.facts().to_vec(),
                    },
                    NOW,
                    2_000,
                )
                .unwrap();
            context
        }

        /// ADR 0033 §7: park a Ready restart_only launch by stopping its
        /// engine; the server from `pid + 50` stays.
        fn park(&self, fence: &DeploymentFence, pid: u32) {
            let o = self.owner.lock().unwrap();
            let park = o
                .store()
                .accept_park_command(
                    o.session(),
                    "operator",
                    &fence.deployment_id,
                    fence.revision,
                    "park-1",
                    NOW,
                    NOW + 60_000,
                )
                .unwrap();
            let ResidencyArm::New(context) = o
                .store()
                .arm_residency(o.session(), &park.step_id, self.admission())
                .unwrap()
            else {
                panic!("expected a fresh arm");
            };
            o.store()
                .complete_residency(
                    o.session(),
                    &park.step_id,
                    &EffectObservation {
                        token: context.token.clone(),
                        binding_id: context.binding_id.clone(),
                        incarnation: context.incarnation.clone(),
                        identities: vec![identity("lmcache", pid + 50)],
                        observed_at_ms: NOW,
                        receipt: "fake host".into(),
                        facts: vec![Milestone::Quiesced, Milestone::MemoryReleased],
                        kernel_builds: Vec::new(),
                    },
                    NOW,
                )
                .unwrap();
        }

        fn http_port(&self, binding: &str) -> u16 {
            let o = self.owner.lock().unwrap();
            o.store()
                .launch_lmcache_grant(binding)
                .unwrap()
                .unwrap()
                .http_port
        }

        fn targets(&self) -> Vec<LmcacheTarget> {
            EmbeddedTargets {
                owner: self.owner.clone(),
                host_id: "lab".into(),
                in_flight: Arc::new(|_, _| 0),
            }
            .lmcache_targets()
        }
    }

    // T45: ADR 0033 §12 (lifecycle design §8). The standalone role reads only
    // its own servers: each Ready or restart-parked launch running one, on
    // its granted loopback admin port, named by its launch and with its
    // server's recorded process; a launch whose host reported LMCache off
    // and one still starting are not read.
    #[test]
    fn the_standalone_role_reads_only_its_ready_and_parked_servers() {
        let lab = lab(&["ready", "parked", "off", "starting"]);
        let [ready, parked, off, starting] = &lab.fences[..] else {
            unreachable!()
        };
        let ready_launch = lab.ready(ready, LmcacheOutcome::On, 10);
        let parked_launch = lab.ready(parked, LmcacheOutcome::On, 20);
        lab.park(parked, 20);
        lab.ready(
            off,
            LmcacheOutcome::Off {
                reason: capyctl_config::lmcache::LmcacheOffReason::ServerStartFailed,
                detail: "no /status within 30 s".into(),
            },
            30,
        );
        lab.start(starting);

        let mut targets = lab.targets();
        targets.sort_by(|a, b| a.server[0].pid.cmp(&b.server[0].pid));
        let shown: Vec<_> = targets
            .iter()
            .map(|t| {
                (
                    t.deployment_id.clone(),
                    t.instance_index,
                    t.generation,
                    t.owned_handle.clone(),
                    t.http,
                    t.server.clone(),
                    t.pool_bytes,
                    t.device_bytes,
                )
            })
            .collect();
        let expected = |fence: &DeploymentFence,
                        launch: &capyctl_domain::completion::StepExecutionContext,
                        pid: u32| {
            (
                fence.deployment_id.clone(),
                0,
                launch.token.generation,
                launch.token.step_id.clone(),
                SocketAddr::from((
                    std::net::Ipv4Addr::LOCALHOST,
                    lab.http_port(&launch.binding_id),
                )),
                vec![identity("lmcache", pid + 50)],
                GIB,
                None,
            )
        };
        assert_eq!(
            shown,
            [
                expected(ready, &ready_launch, 10),
                expected(parked, &parked_launch, 20)
            ]
        );
    }

    // T45: ADR 0033 §12. One tick reads each server's `/metrics` on its
    // loopback admin port and records its cache figures in the role's load
    // table under the instance's incarnation, as a host's report would be:
    // the used bytes, and lookups and hits summed over their label sets.
    #[tokio::test]
    async fn a_tick_records_the_servers_cache_figures_for_its_instance() {
        let lab = lab(&["ready"]);
        let ready = lab.fences[0].clone();
        let launch = lab.ready(&ready, LmcacheOutcome::On, 10);
        let port = lab.http_port(&launch.binding_id);
        let metrics = "\
# TYPE lmcache_mp_lookup_requested_tokens_total counter
lmcache_mp_lookup_requested_tokens_total{model_name=\"m\",cache_salt=\"aa\"} 200
lmcache_mp_lookup_requested_tokens_total{model_name=\"m\",cache_salt=\"bb\"} 60
lmcache_mp_lookup_hit_tokens_total{model_name=\"m\",cache_salt=\"aa\"} 200
lmcache_mp_lookup_hit_tokens_total{model_name=\"m\",cache_salt=\"bb\"} 12
lmcache_mp_l1_memory_usage_bytes 1395864371
";
        let app = axum::Router::new().route(
            "/metrics",
            axum::routing::get(move || async move { metrics }),
        );
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let load = EmbeddedLoad::new(lab.owner.clone(), "lab".into(), Arc::new(|_, _| 0)).unwrap();
        load.tick().await;
        let key = InstanceKey::new(ready.deployment_id.clone(), launch.token.generation);
        let view = load
            .table()
            .lmcache_at(&key, capyctl_protocol::now_unix_ms())
            .unwrap();
        assert_eq!(
            (
                view.host_id.as_str(),
                view.owned_handle.as_str(),
                view.fresh,
                view.scrape_ok
            ),
            ("lab", launch.token.step_id.as_str(), true, true)
        );
        assert_eq!(
            (view.used_bytes, view.lookups, view.hits),
            (Some(1_395_864_371), Some(260), Some(212))
        );
        // Another generation of the deployment reads nothing.
        let other = InstanceKey::new(ready.deployment_id.clone(), launch.token.generation + 1);
        assert!(load
            .table()
            .lmcache_at(&other, capyctl_protocol::now_unix_ms())
            .is_none());
    }
}
