//! ADR 0033 (T45): the real coordinator worker driving launches whose store
//! plans LMCache on, against a scripted host that starts each launch's engine
//! and its LMCache server and reports them as the embedded adapter does
//! (`LmcacheState` filled by the adapter itself) or as a remote host does (a
//! `MemberExecutionResult` converted by the real remote conversions). The
//! store underneath is a copy of the testkit fixture whose vLLM profile is
//! 0.32.0 with LMCache 0.5.6 registered and whose deployment is
//! `restart_only` with a 1 GiB pool, as in the store's LMCache tests. CPU
//! only: nothing here qualifies an engine, LMCache or a host.
use super::*;
use capyctl_adapters::lmcache::{LmcacheReport, LmcacheState};
use capyctl_config::lmcache::LmcacheOffReason;
use capyctl_domain::completion::{
    lmcache_members, EffectObservation, ExecutionIdentities, Milestone, ProcessIdentity,
    StepExecutionContext, LMCACHE_ROLE,
};
use capyctl_protocol::execution::LaunchLmcache;
use capyctl_protocol::pb;
use capyctl_store::ordinary_lifecycle::lmcache::{LmcacheGrant, PlannedLmcache};
use serde_json::{json, Value};
use std::sync::atomic::AtomicU32;

const GIB: i64 = 1 << 30;
const MIB: i64 = 1 << 20;
const DIGEST: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
/// ADR 0033 §6 and A1 on unified memory: the 1 GiB pool, the 1280 MiB
/// overhead allowance and the 512 MiB device allowance, all in the one pool.
const SERVER_CHARGE: i64 = GIB + 1280 * MIB + 512 * MIB;
/// The redacted engine-log tail the engine leaves when it fails with the
/// connector (ADR 0033 §8).
const TAIL: &str = "RuntimeError: LMCacheMPConnector handshake with 127.0.0.1 failed";
/// What a server that failed its listener check reports (ADR 0033 §3).
const EXPOSED: &str =
    "the LMCache server listens on 0.0.0.0:25002, not on loopback; the server was stopped";

/// How the scripted host reports what became of a launch's LMCache.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Report {
    /// As the embedded vLLM adapter: it fills the launch's `LmcacheState`
    /// itself, and a launch planned off carries the store's reason.
    Adapter,
    /// As a remote host: a `MemberExecutionResult` with its `lmcache` report
    /// and, for a park or restore, its `residency` evidence, read through
    /// `remote_execution::lmcache_report` and `residency_evidence`.
    HostResult,
}

/// One step the scripted host carried out: (action, the identities the step
/// retained, the identities its evidence named or, for a failed Initialize,
/// the ones recorded before it failed).
type Step = (RuntimeAction, Vec<ProcessIdentity>, Vec<ProcessIdentity>);

/// The scripted host: it starts each launch's engine (and the LMCache server
/// the store granted it), parks and restores it, and answers the cleanup and
/// the failed-launch settlement with what it observed gone.
struct CacheHost {
    report: Report,
    owner: SharedCoordinatorState,
    clock: Arc<AtomicI64>,
    /// An engine launched with the connector fails before Ready.
    connector_fails: AtomicBool,
    /// ADR 0033 §§3, 9: the server fails its listener check and is stopped
    /// (verified) before the engine is spawned.
    server_exposed: AtomicBool,
    /// What the store planned for each launch the coordinator bound, in order.
    planned: Mutex<Vec<(String, PlannedLmcache)>>,
    /// Real processes the next launch's engine group and server group (its
    /// `lmcache` leader first) are recorded as (one boot, as a launch must
    /// be), so an exit is one this machine observes.
    live_launch: Mutex<Option<(Vec<ProcessIdentity>, Vec<ProcessIdentity>)>>,
    next_pid: AtomicU32,
    steps: Mutex<Vec<Step>>,
    settlements: Mutex<Vec<SettlementContext>>,
    cleanups: Mutex<Vec<CleanupExecutionContext>>,
    /// A remote host's view of the launch's processes alive now, which its
    /// answer to a wake canary reports.
    alive: Mutex<Vec<ProcessIdentity>>,
    /// Whether each wake canary a remote host answered counted.
    canaries: Mutex<Vec<bool>>,
}

impl CacheHost {
    fn new(report: Report, owner: SharedCoordinatorState) -> Arc<Self> {
        Arc::new(Self {
            report,
            owner,
            clock: Arc::new(AtomicI64::new(1900)),
            connector_fails: AtomicBool::new(false),
            server_exposed: AtomicBool::new(false),
            planned: Mutex::new(Vec::new()),
            live_launch: Mutex::new(None),
            next_pid: AtomicU32::new(1000),
            steps: Mutex::new(Vec::new()),
            settlements: Mutex::new(Vec::new()),
            cleanups: Mutex::new(Vec::new()),
            alive: Mutex::new(Vec::new()),
            canaries: Mutex::new(Vec::new()),
        })
    }

    fn now(&self) -> i64 {
        self.clock.load(Ordering::SeqCst)
    }

    fn identity(&self, role: &str, pid: u32) -> ProcessIdentity {
        ProcessIdentity {
            role: role.into(),
            pid,
            boot_id: "scripted-host-boot".into(),
            start_ticks: u64::from(pid) + 7,
        }
    }

    /// A new engine group: its `api` leader and one worker.
    fn engine(&self) -> Vec<ProcessIdentity> {
        let base = self.next_pid.fetch_add(10, Ordering::SeqCst);
        vec![
            self.identity("api", base),
            self.identity("worker-0", base + 1),
        ]
    }

    /// ADR 0033 §3: a leader recorded on the launch's binding before it runs,
    /// as the embedded association (or the remote ownership observer) does.
    fn associate(&self, c: &StepExecutionContext, identity: &ProcessIdentity) {
        let o = self.owner.lock().unwrap();
        o.store()
            .record_launch_identity(o.session(), &fence_of(c), &c.binding_id, identity)
            .unwrap();
    }

    /// What the store planned for each launch, in order: `None` for LMCache
    /// on, else the reason it was planned off.
    fn plans(&self) -> Vec<Option<LmcacheOffReason>> {
        self.planned
            .lock()
            .unwrap()
            .iter()
            .map(|(_, planned)| match planned {
                PlannedLmcache::On(_) => None,
                PlannedLmcache::Off(off) => Some(off.reason),
            })
            .collect()
    }

    fn steps(&self, action: RuntimeAction) -> Vec<(Vec<ProcessIdentity>, Vec<ProcessIdentity>)> {
        self.steps
            .lock()
            .unwrap()
            .iter()
            .filter(|step| step.0 == action)
            .map(|step| (step.1.clone(), step.2.clone()))
            .collect()
    }

    /// The binding the coordinator resolves for one launch: its engine, the
    /// cleanup and settlement Terminates, and the launch's LMCache state.
    fn bind(self: &Arc<Self>, work: &InitializeWork) -> ExecutionBinding {
        let plan = work
            .lmcache()
            .expect("a single launch planned after ADR 0033 has an LMCache plan");
        self.planned
            .lock()
            .unwrap()
            .push((work.binding_id().to_owned(), plan.planned.clone()));
        let state = match (&plan.planned, self.report) {
            // As `ProfileBindings`: a launch planned off carries the store's
            // reason to its adapter.
            (PlannedLmcache::Off(off), Report::Adapter) => {
                LmcacheState::off(off.reason, off.detail.clone())
            }
            // As `remote_execution::binding`: filled from the host's report.
            _ => Arc::new(LmcacheState::default()),
        };
        let engine = Arc::new(Launch {
            host: self.clone(),
            grant: plan.grant().copied(),
            state: state.clone(),
        });
        let (cleanup, settle) = (self.clone(), self.clone());
        ExecutionBinding::remote(
            engine,
            Arc::new(move |context: CleanupExecutionContext| {
                let host = cleanup.clone();
                Box::pin(async move {
                    host.cleanups.lock().unwrap().push(context.clone());
                    Ok(CleanupEvidence {
                        binding_id: context.binding_id,
                        incarnation: context.incarnation,
                        identities: context.identities,
                        observed_at_ms: host.now(),
                        receipt: "scripted host observed the owned groups gone".into(),
                    })
                })
            }),
        )
        .with_settlement(Arc::new(move |context: SettlementContext| {
            let host = settle.clone();
            Box::pin(async move {
                host.settlements.lock().unwrap().push(context.clone());
                Ok(CleanupEvidence {
                    binding_id: context.binding_id,
                    incarnation: context.incarnation,
                    identities: context.identities,
                    observed_at_ms: host.now(),
                    receipt: "scripted host terminated the launch and observed it gone".into(),
                })
            })
        }))
        .with_lmcache(state)
    }
}

fn fence_of(c: &StepExecutionContext) -> DeploymentFence {
    DeploymentFence {
        deployment_id: c.token.deployment_id.clone(),
        revision: c.token.revision,
        generation: c.token.generation,
    }
}

fn observed(identities: &[ProcessIdentity], presence: &str) -> Vec<pb::OwnedProcessObservation> {
    identities
        .iter()
        .map(|p| pb::OwnedProcessObservation {
            role: p.role.clone(),
            pid: p.pid,
            boot_id: p.boot_id.clone(),
            start_ticks: p.start_ticks,
            presence: presence.into(),
            rank: None,
        })
        .collect()
}

fn alive(result: &pb::MemberExecutionResult) -> Vec<ProcessIdentity> {
    result
        .processes
        .iter()
        .filter(|p| p.presence == "alive")
        .map(|p| ProcessIdentity {
            role: p.role.clone(),
            pid: p.pid,
            boot_id: p.boot_id.clone(),
            start_ticks: p.start_ticks,
        })
        .collect()
}

const USABLE: [Milestone; 4] = [
    Milestone::AllocationsRestored,
    Milestone::WeightsUsable,
    Milestone::CacheValid,
    Milestone::ModelUsable,
];

/// One launch's engine on the scripted host.
struct Launch {
    host: Arc<CacheHost>,
    grant: Option<LmcacheGrant>,
    state: Arc<LmcacheState>,
}

impl Launch {
    fn result(&self, c: &StepExecutionContext) -> pb::MemberExecutionResult {
        pb::MemberExecutionResult {
            owned_handle: c.token.step_id.clone(),
            binding_id: c.binding_id.clone(),
            incarnation: c.incarnation.clone(),
            observed_at_unix_ms: self.host.now(),
            ..Default::default()
        }
    }

    fn observation(
        &self,
        c: &StepExecutionContext,
        identities: Vec<ProcessIdentity>,
        facts: Vec<Milestone>,
    ) -> EffectObservation {
        EffectObservation {
            token: c.token.clone(),
            binding_id: c.binding_id.clone(),
            incarnation: c.incarnation.clone(),
            identities,
            observed_at_ms: self.host.now(),
            receipt: "scripted host".into(),
            facts,
            kernel_builds: Vec::new(),
        }
    }

    async fn initialize(
        &self,
        c: &StepExecutionContext,
    ) -> Result<EffectObservation, RuntimeError> {
        let host = &self.host;
        let (engine, live_server) = match host.live_launch.lock().unwrap().take() {
            Some((engine, server)) => (engine, Some(server)),
            None => (host.engine(), None),
        };
        let server: Option<Vec<ProcessIdentity>> = self.grant.map(|_| {
            live_server.unwrap_or_else(|| vec![host.identity(LMCACHE_ROLE, engine[0].pid + 5)])
        });
        // ADR 0033 §§3, 9: a server that failed its listener check was
        // stopped (verified) and, under `auto`, left the launch's record
        // before the engine was spawned: on the embedded path through the
        // store (as the coordinator's association does), on a remote one in
        // the host's journal, so its result never names it.
        let exposed = server.is_some() && host.server_exposed.load(Ordering::SeqCst);
        // ADR 0033 §3: the server is recorded before it runs, and is started
        // before the engine.
        if let Some(server) = &server {
            if !exposed || host.report == Report::Adapter {
                host.associate(c, &server[0]);
            }
        }
        let server = if exposed {
            if host.report == Report::Adapter {
                let o = host.owner.lock().unwrap();
                o.store()
                    .depart_lmcache_server(
                        o.session(),
                        &fence_of(c),
                        &c.binding_id,
                        server.as_deref().unwrap(),
                    )
                    .unwrap();
            }
            None
        } else {
            server
        };
        let fails = server.is_some() && host.connector_fails.load(Ordering::SeqCst);
        let mut launched = engine.clone();
        launched.extend(server.iter().flatten().cloned());
        if fails {
            host.associate(c, &engine[0]);
            host.steps.lock().unwrap().push((
                RuntimeAction::Initialize,
                Vec::new(),
                launched.clone(),
            ));
        }
        match host.report {
            Report::Adapter => {
                if fails {
                    // ADR 0033 §8: what `Connector::engine_failed` reports.
                    self.state.record(LmcacheReport::Off {
                        reason: LmcacheOffReason::EngineStartFailed,
                        detail: TAIL.into(),
                        connector_failed: true,
                    });
                    return Err(RuntimeError::LaunchFailed(format!(
                        "the engine exited before readiness (exit code 1); log tail:\n{TAIL}"
                    )));
                }
                if exposed {
                    // What `lmcache::connect` reports under `auto`.
                    self.state.record(LmcacheReport::Off {
                        reason: LmcacheOffReason::ListenerExposed,
                        detail: EXPOSED.into(),
                        connector_failed: false,
                    });
                }
                if let (Some(server), Some(grant)) = (&server, &self.grant) {
                    // What `Connector::ready` reports.
                    self.state.record(LmcacheReport::On {
                        server: server.clone(),
                        rpc_port: grant.rpc_port,
                        http_port: grant.http_port,
                        pool_bytes: grant.pool_bytes,
                    });
                }
            }
            Report::HostResult => {
                let mut result = self.result(c);
                result.claim_retained = true;
                if fails {
                    result.state = "launched".into();
                    result.processes = observed(&launched, "gone");
                    result.launch_failure = "exit code 1".into();
                    result.lmcache = Some(
                        LaunchLmcache::off(LmcacheOffReason::EngineStartFailed, TAIL, true)
                            .to_wire(),
                    );
                } else {
                    result.state = "completed".into();
                    result.model_usable = true;
                    result.processes = observed(&launched, "alive");
                    result.lmcache = if exposed {
                        Some(
                            LaunchLmcache::off(LmcacheOffReason::ListenerExposed, EXPOSED, false)
                                .to_wire(),
                        )
                    } else {
                        self.grant.map(|grant| {
                            LaunchLmcache::on(grant.rpc_port, grant.http_port, grant.pool_bytes)
                                .to_wire()
                        })
                    };
                }
                // As `RemoteEngine::execute_persisted`: the host's report is
                // recorded on the launch's state, Ready or failed.
                if let Some(report) = crate::remote_execution::lmcache_report(&result) {
                    self.state.record(report);
                }
                if fails {
                    assert!(crate::agent_sessions::launch_ended_before_readiness(
                        &result
                    ));
                    return Err(RuntimeError::LaunchFailed(result.launch_failure));
                }
                launched = alive(&result);
                *host.alive.lock().unwrap() = launched.clone();
            }
        }
        host.steps
            .lock()
            .unwrap()
            .push((RuntimeAction::Initialize, Vec::new(), launched.clone()));
        Ok(self.observation(c, launched, USABLE.to_vec()))
    }

    /// ADR 0033 §7: a restart-park stops the engine and keeps the server; the
    /// wake starts a new engine on that same server.
    async fn residency(
        &self,
        action: RuntimeAction,
        c: &StepExecutionContext,
    ) -> Result<EffectObservation, RuntimeError> {
        let host = &self.host;
        let ExecutionIdentities::Retained(recorded) = &c.identities else {
            return Err(RuntimeError::StaleRevision);
        };
        let server = lmcache_members(recorded);
        let park = action == RuntimeAction::Park;
        let (named, facts) = match (host.report, park) {
            // What `lmcache::park_engine` reports.
            (Report::Adapter, true) => (
                server.clone(),
                vec![Milestone::Quiesced, Milestone::MemoryReleased],
            ),
            (Report::Adapter, false) => {
                let mut woken = host.engine();
                // ADR 0033 §7: the wake's engine is recorded before it runs.
                {
                    let o = host.owner.lock().unwrap();
                    o.store()
                        .record_wake_identity(
                            o.session(),
                            &fence_of(c),
                            &c.binding_id,
                            &c.token.step_id,
                            &woken[0],
                        )
                        .unwrap();
                }
                woken.extend(server.clone());
                (woken, USABLE.to_vec())
            }
            (Report::HostResult, _) => {
                let mut result = self.result(c);
                result.state = "completed".into();
                result.claim_retained = true;
                result.model_usable = !park;
                result.processes = if park {
                    let engine: Vec<_> = recorded
                        .iter()
                        .filter(|p| !p.is_lmcache())
                        .cloned()
                        .collect();
                    let mut processes = observed(&engine, "gone");
                    processes.extend(observed(&server, "alive"));
                    processes
                } else {
                    let mut woken = host.engine();
                    woken.extend(server.clone());
                    observed(&woken, "alive")
                };
                result.residency = Some(pb::ResidencyEvidence {
                    state: if park { "parked" } else { "restored" }.into(),
                    mem_available_before_bytes: -1,
                    mem_available_after_bytes: -1,
                    engine_stopped: park,
                    ..Default::default()
                });
                let evidence =
                    crate::remote_execution::residency_evidence(park, recorded, &result)?;
                *host.alive.lock().unwrap() = evidence.0.clone();
                evidence
            }
        };
        host.steps
            .lock()
            .unwrap()
            .push((action, recorded.clone(), named.clone()));
        Ok(self.observation(c, named, facts))
    }
}

#[async_trait::async_trait]
impl EngineAdapter for Launch {
    async fn execute_persisted(
        &self,
        command: &RuntimeCommand,
    ) -> Result<EffectObservation, RuntimeError> {
        match command.action {
            RuntimeAction::Initialize => self.initialize(&command.context).await,
            action @ (RuntimeAction::Park | RuntimeAction::Restore) => {
                self.residency(action, &command.context).await
            }
            _ => Err(RuntimeError::Unsupported),
        }
    }
    async fn inspect(&self, _: &MemberRef) -> Result<EngineState, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn render_plan(&self, _: &PlanInput) -> Result<RenderedCommand, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn check_readiness(&self, _: &MemberRef) -> Result<Readiness, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn prepare_park(&self, _: &MemberRef) -> Result<Quiescence, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn park(&self, _: &MemberRef, _: ParkLevel) -> Result<ParkOutcome, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn restore(&self, _: &MemberRef) -> Result<RestoreOutcome, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn reload_weights(&self, _: &MemberRef) -> Result<ReloadOutcome, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn observe_work(&self, _: &MemberRef) -> Result<WorkObservation, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    async fn cancel_work(
        &self,
        _: &MemberRef,
        _: &RequestRef,
        _: bool,
    ) -> Result<CancellationOutcome, AdapterError> {
        Err(AdapterError::UnsupportedCapability)
    }
    /// Owner decision 2026-10-09: a remote host answers the wake canary with
    /// a completion and the launch's processes alive now, judged as
    /// `RemoteEngine::wake_canary` judges it. The embedded adapter's canary
    /// probes its own engine, which this scripted one has not.
    async fn wake_canary(
        &self,
        c: &StepExecutionContext,
        _: u32,
        _: Duration,
    ) -> Result<capyctl_adapters::completion_probe::CanaryAnswer, RuntimeError> {
        use capyctl_adapters::completion_probe::{CanaryAnswer, CanaryEvidence, ProbeAnswer};
        if self.host.report == Report::Adapter {
            return Err(RuntimeError::Unsupported);
        }
        let mut result = self.result(c);
        result.state = "completed".into();
        result.claim_retained = true;
        result.processes = observed(&self.host.alive.lock().unwrap(), "alive");
        let counted = crate::remote_execution::canary_engine_alive(&c.identities, &result);
        self.host.canaries.lock().unwrap().push(counted.is_ok());
        counted?;
        Ok(CanaryAnswer {
            answer: CanaryEvidence::Generated(ProbeAnswer {
                tokens: vec![1, 2, 3],
                text: "Ready.".into(),
            }),
            observed_at_ms: result.observed_at_unix_ms,
        })
    }
}

fn worker(
    owner: SharedCoordinatorState,
    observations: Vec<MemoryObservation>,
    host: Arc<CacheHost>,
) -> OwnedCoordinator {
    let clock = host.clock.clone();
    OwnedCoordinator::spawn_with_execution_bindings(
        owner,
        Arc::new(Observations(observations)),
        Arc::new(move || Ok(clock.load(Ordering::SeqCst))),
        CoordinatorOptions {
            retry_cooldown: Duration::from_millis(50),
            ..Default::default()
        },
        Arc::new(move |work: &InitializeWork| Ok(host.bind(work))),
    )
    .unwrap()
}

/// The host of the store's LMCache tests: vLLM 0.32.0 with LMCache 0.5.6
/// registered on its profile, room for the pool in `host_kv_limit`.
fn lab_host(host: &mut Value) {
    let profile = &mut host["runtime_profiles"]["local"];
    profile["build_fingerprint"] = json!("0.32.0");
    profile["lmcache"] = json!({"version": "0.5.6", "digest": DIGEST});
    host["resource_policy"]["domains"]["unified"]["host_kv_limit"] = json!("8GiB");
}

/// A copied store with one stopped `restart_only` deployment whose launches
/// the store plans with LMCache on (a 1 GiB pool), edited by `edit`.
async fn lab(
    edit: impl FnOnce(&mut Value),
) -> (
    tempfile::TempDir,
    SharedCoordinatorState,
    DeploymentFence,
    Vec<MemoryObservation>,
) {
    use std::os::unix::fs::PermissionsExt;
    let f = fixture::fixture_with(|_, host| lab_host(host));
    let fence = fixture::managed_edit(&f, "cached", |deployment, host| {
        lab_host(host);
        deployment["residency"] = json!("restart_only");
        deployment["lmcache_pool"] = json!("1GiB");
        edit(deployment);
    });
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
    (dir, owner, fence, f.observations.clone())
}

fn sql(dir: &tempfile::TempDir) -> rusqlite::Connection {
    rusqlite::Connection::open(dir.path().join("srv.sqlite3")).unwrap()
}

/// (observed state, dispatch open, generation) of instance 0.
fn instance(dir: &tempfile::TempDir, deployment: &str) -> (String, bool, i64) {
    sql(dir)
        .query_row(
            "SELECT observed_state,dispatch_enabled=1,generation FROM deployment_instances WHERE deployment_id=?1 AND instance_index=0",
            [deployment],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
}

/// The payloads of the deployment's `kind` events, in order.
fn events(dir: &tempfile::TempDir, kind: &str, deployment: &str) -> Vec<Value> {
    sql(dir)
        .prepare("SELECT payload_json FROM management_events WHERE kind=?1 AND deployment_id=?2 ORDER BY sequence")
        .unwrap()
        .query_map([kind, deployment], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|raw| serde_json::from_str(&raw.unwrap()).unwrap())
        .collect()
}

/// The journal entries in `state` that name the deployment.
fn journal(dir: &tempfile::TempDir, state: &str, deployment: &str) -> Vec<String> {
    sql(dir)
        .prepare("SELECT evidence FROM journal_entries WHERE state=?1 ORDER BY rowid")
        .unwrap()
        .query_map([state], |r| r.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .filter(|evidence| evidence.contains(deployment))
        .collect()
}

/// The deployment's `initialize` operations.
fn initializes(dir: &tempfile::TempDir, deployment: &str) -> i64 {
    sql(dir)
        .query_row(
            "SELECT COUNT(*) FROM operations WHERE deployment_id=?1 AND kind='initialize'",
            [deployment],
            |r| r.get(0),
        )
        .unwrap()
}

/// (state, error code) of an operation.
fn operation(owner: &SharedCoordinatorState, id: &str) -> (String, Option<String>) {
    let o = owner.lock().unwrap();
    let row = o.store().get_operation(id).unwrap().unwrap();
    (
        match row.state {
            capyctl_store::deployments::OpState::Pending => "pending",
            capyctl_store::deployments::OpState::Running => "running",
            capyctl_store::deployments::OpState::Succeeded => "succeeded",
            capyctl_store::deployments::OpState::Failed => "failed",
        }
        .to_string(),
        row.error_code,
    )
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while !done() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

/// Instance 0's charge on the unified pool as `resource_snapshot` holds it:
/// (phase, bytes, host KV bytes); `None` once released.
fn charge(owner: &SharedCoordinatorState, deployment: &str) -> Option<(ResourcePhase, i64, i64)> {
    let o = owner.lock().unwrap();
    let snapshot = o.store().resource_snapshot().unwrap();
    let footprint = snapshot.owners.get(deployment)?;
    let unified = footprint
        .allocations
        .iter()
        .find(|a| a.domain == "unified")
        .unwrap();
    Some((footprint.phase, unified.bytes, unified.host_kv_bytes))
}

/// The retained binding of instance 0: (id, recorded identities).
fn binding(owner: &SharedCoordinatorState, deployment: &str) -> (String, Vec<ProcessIdentity>) {
    let o = owner.lock().unwrap();
    let b = o.store().runtime_binding(deployment).unwrap().unwrap();
    (b.id, b.identities)
}

/// Instance 0 as status shows it.
fn status(
    owner: &SharedCoordinatorState,
    deployment: &str,
) -> capyctl_store::snapshot::InstanceSnapshot {
    let o = owner.lock().unwrap();
    let snapshot = o.store().snapshot().unwrap();
    snapshot
        .deployments
        .into_iter()
        .find(|d| d.id == deployment)
        .unwrap()
        .instances
        .into_iter()
        .find(|i| i.index == 0)
        .unwrap()
}

async fn ready(w: &OwnedCoordinator, fence: &DeploymentFence) {
    let start = w.start(fence, 100_000).unwrap();
    assert_eq!(
        start.wait(Duration::from_secs(30)).await.unwrap(),
        InitializeStatus::Completed
    );
}

/// The engine's Ready charge without LMCache (the fixture's declared 8 GiB,
/// 1 GiB of it host KV), and with its server beside it.
const ENGINE_READY: (i64, i64) = (8 * GIB, GIB);
const CACHED_READY: (i64, i64) = (8 * GIB + SERVER_CHARGE, 2 * GIB);

/// ADR 0033 §8: an engine that fails before Ready with the connector is
/// settled on the host's evidence (its server among what is proven gone),
/// released with the sticky fallback, `lmcache_off` (`engine_start_failed`)
/// recorded once and `lmcache_fallback` journaled, and started exactly once
/// more without LMCache, which serves. A stop and a start afterwards still go
/// without LMCache: the fallback sticks for the revision on the profile, so
/// the engine is never offered the connector again.
async fn the_fallback_relaunches_once_and_sticks(report: Report) {
    let (dir, owner, fence, observations) = lab(|_| {}).await;
    let deployment = fence.deployment_id.clone();
    let host = CacheHost::new(report, owner.clone());
    host.connector_fails.store(true, Ordering::SeqCst);
    let w = worker(owner.clone(), observations, host.clone());
    let start = w.start(&fence, 100_000).unwrap();
    // The release accepted the relaunch on the instance's claim in its own
    // transaction, so the failed start reads superseded by it.
    assert_eq!(
        start.wait(Duration::from_secs(30)).await.unwrap(),
        InitializeStatus::Superseded
    );
    assert_eq!(
        operation(&owner, start.operation_id()),
        ("failed".into(), Some("launch_failed".into()))
    );
    // Verified: the host proved the engine and its server gone.
    let settled = host.settlements.lock().unwrap().clone();
    assert_eq!(settled.len(), 1);
    let failed = host.steps(RuntimeAction::Initialize)[0].1.clone();
    let mut proven = settled[0].identities.clone();
    proven.sort_by_key(|p| p.pid);
    assert!(proven.iter().any(ProcessIdentity::is_lmcache), "{proven:?}");
    assert!(proven.iter().any(|p| p.role == "api"), "{proven:?}");
    assert!(proven.iter().all(|p| failed.contains(p)), "{proven:?}");

    until("the relaunch serves", || {
        instance(&dir, &deployment).0 == "ready"
    })
    .await;
    assert!(instance(&dir, &deployment).1, "the relaunch dispatches");
    assert_eq!(
        host.plans(),
        [None, Some(LmcacheOffReason::EngineStartFailed)],
        "started exactly twice, the second time without LMCache"
    );
    let (_, relaunched) = binding(&owner, &deployment);
    assert!(relaunched.iter().all(|p| !p.is_lmcache()), "{relaunched:?}");
    // ADR 0033 §6: the relaunch charges the engine alone.
    let (phase, bytes, host_kv) = charge(&owner, &deployment).unwrap();
    assert_eq!(
        (phase, (bytes, host_kv)),
        (ResourcePhase::Ready, ENGINE_READY)
    );
    let off = events(&dir, "lmcache_off", &deployment);
    assert_eq!(off.len(), 1, "{off:?}");
    assert_eq!(off[0]["reason"], "engine_start_failed");
    assert_eq!(off[0]["binding_id"], host.planned.lock().unwrap()[0].0);
    assert!(off[0]["detail"]
        .as_str()
        .unwrap()
        .contains("handshake with 127.0.0.1 failed"));
    assert_eq!(journal(&dir, "lmcache_fallback", &deployment).len(), 1);
    let shown = status(&owner, &deployment).lmcache.unwrap();
    assert_eq!(
        (shown.state.as_str(), shown.reason.as_deref()),
        ("off", Some("engine_start_failed"))
    );

    // Sticky: a stop and a new start of the revision on this profile go
    // straight to the launch without LMCache, though the engine would fail
    // with the connector again.
    let stop = w
        .commands()
        .stop("operator", &deployment, fence.revision, "stop-1", 100_000)
        .unwrap();
    until("the stop settled", || {
        operation(&owner, &stop.operation_id).0 == "succeeded"
    })
    .await;
    assert_eq!(charge(&owner, &deployment), None);
    w.commands()
        .start("operator", &deployment, fence.revision, "start-2", 100_000)
        .unwrap();
    until("the new start serves", || {
        instance(&dir, &deployment).0 == "ready"
    })
    .await;
    assert_eq!(
        host.plans(),
        [
            None,
            Some(LmcacheOffReason::EngineStartFailed),
            Some(LmcacheOffReason::EngineStartFailed)
        ]
    );
    let started = host.steps(RuntimeAction::Initialize);
    assert_eq!(started.len(), 3);
    assert!(started[1..]
        .iter()
        .all(|(_, named)| named.iter().all(|p| !p.is_lmcache())));
    assert_eq!(host.settlements.lock().unwrap().len(), 1);
    assert_eq!(journal(&dir, "lmcache_fallback", &deployment).len(), 1);
    w.shutdown().await.unwrap();
}

// T45 T30 T36 T37 (ADR 0033 §8): the embedded adapter's report.
#[tokio::test]
async fn an_engine_failing_with_the_connector_relaunches_once_without_lmcache() {
    the_fallback_relaunches_once_and_sticks(Report::Adapter).await;
}

// T45 T30 T36 T37 (ADR 0033 §8, lifecycle design §10): the same through a
// remote host's LaunchSingle result and its `lmcache` report.
#[tokio::test]
async fn a_remote_engine_failing_with_the_connector_relaunches_once_without_lmcache() {
    the_fallback_relaunches_once_and_sticks(Report::HostResult).await;
}

/// ADR 0033 §§3, 9 (found live on SGLang: the launch failed `lifecycle
/// conflict` at Ready and `start --wait` exited 13): a server that failed
/// its listener check under `auto` is stopped (verified) and leaves the
/// launch's record before the engine is spawned, so the engine launched
/// without the connector reaches Ready on the first launch, charged alone,
/// and status shows LMCache off with the reason.
async fn an_exposed_server_leaves_the_engine_ready_alone(report: Report) {
    let (dir, owner, fence, observations) = lab(|_| {}).await;
    let deployment = fence.deployment_id.clone();
    let host = CacheHost::new(report, owner.clone());
    host.server_exposed.store(true, Ordering::SeqCst);
    let w = worker(owner.clone(), observations, host.clone());
    ready(&w, &fence).await;
    assert_eq!(host.plans(), [None], "one launch, planned with LMCache");
    assert_eq!(initializes(&dir, &deployment), 1);
    let (state, open, _) = instance(&dir, &deployment);
    assert_eq!((state.as_str(), open), ("ready", true));
    let (_, recorded) = binding(&owner, &deployment);
    assert!(recorded.iter().all(|p| !p.is_lmcache()), "{recorded:?}");
    assert!(recorded.iter().any(|p| p.role == "api"), "{recorded:?}");
    let (phase, bytes, host_kv) = charge(&owner, &deployment).unwrap();
    assert_eq!(
        (phase, (bytes, host_kv)),
        (ResourcePhase::Ready, ENGINE_READY)
    );
    let shown = status(&owner, &deployment).lmcache.unwrap();
    assert_eq!(
        (shown.state.as_str(), shown.reason.as_deref()),
        ("off", Some("listener_exposed"))
    );
    let off = events(&dir, "lmcache_off", &deployment);
    assert_eq!(off.len(), 1, "{off:?}");
    assert_eq!(off[0]["reason"], "listener_exposed");
    // Embedded, the store journals the departure; a remote host journals it
    // in its own journal, and its result never names the server.
    let departed = sql(&dir)
        .query_row(
            "SELECT COUNT(*) FROM journal_entries WHERE state='lmcache_server_departed'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(departed, i64::from(report == Report::Adapter));
    w.shutdown().await.unwrap();
}

// T45 T37 T14 T36 (ADR 0033 §§3, 9): the embedded adapter's path.
#[tokio::test]
async fn an_exposed_server_is_stopped_and_the_engine_is_ready_without_lmcache() {
    an_exposed_server_leaves_the_engine_ready_alone(Report::Adapter).await;
}

// T45 T37 T14 T36 (ADR 0033 §§3, 9): the same through a remote host's
// LaunchSingle result and its `lmcache` report.
#[tokio::test]
async fn a_remote_exposed_server_is_stopped_and_the_engine_is_ready_without_lmcache() {
    an_exposed_server_leaves_the_engine_ready_alone(Report::HostResult).await;
}

// T45 T36 T14 (ADR 0033 §§8, 9): under `lmcache: required` the same failure
// is not relaunched: the launch fails `lmcache_unavailable:engine_start_failed`
// and nothing is left running: the host proved the engine and its server
// gone, the binding and its charge are released and the instance does not
// serve.
#[tokio::test]
async fn a_required_launch_failing_with_the_connector_fails_with_nothing_left() {
    let (dir, owner, fence, observations) =
        lab(|deployment| deployment["lmcache"] = json!("required")).await;
    let deployment = fence.deployment_id.clone();
    let host = CacheHost::new(Report::Adapter, owner.clone());
    host.connector_fails.store(true, Ordering::SeqCst);
    let w = worker(owner.clone(), observations, host.clone());
    let start = w.start(&fence, 100_000).unwrap();
    assert_eq!(
        start.wait(Duration::from_secs(30)).await.unwrap(),
        InitializeStatus::Closed
    );
    assert_eq!(
        operation(&owner, start.operation_id()),
        (
            "failed".into(),
            Some("lmcache_unavailable:engine_start_failed".into())
        )
    );
    // Nothing started again: the release would have accepted the relaunch
    // in its own transaction.
    assert_eq!(initializes(&dir, &deployment), 1);
    assert_eq!(host.plans(), [None]);
    let settled = host.settlements.lock().unwrap().clone();
    assert_eq!(settled.len(), 1);
    assert!(settled[0]
        .identities
        .iter()
        .any(ProcessIdentity::is_lmcache));
    assert!(settled[0].identities.iter().any(|p| p.role == "api"));
    let live: i64 = sql(&dir)
        .query_row(
            "SELECT COUNT(*) FROM runtime_bindings WHERE deployment_id=?1 AND state!='released'",
            [&deployment],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(live, 0);
    assert_eq!(charge(&owner, &deployment), None, "the charge is released");
    let (state, open, _) = instance(&dir, &deployment);
    assert_ne!(state, "ready");
    assert!(!open);
    let off = events(&dir, "lmcache_off", &deployment);
    assert_eq!(off.len(), 1, "{off:?}");
    assert_eq!(off[0]["reason"], "engine_start_failed");
    assert!(journal(&dir, "lmcache_fallback", &deployment).is_empty());
    w.shutdown().await.unwrap();
}

/// A real process killed on drop, so a test that fails leaves nothing behind.
struct Process(std::process::Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// T45 T24 T37 T10 T14 (ADR 0033 §3, LC9; found live on SGLang): the server
// is its group's leader. A short-lived child of the server, recorded at
// Ready (`lmcache-0`), exits (a real process, observed by the embedded exit
// watch): the instance keeps serving and nothing is recorded. Then the
// server's leader exits: dispatch closes, `lmcache_server_exited` is
// recorded once, and the exit stop settles on verified cleanup of the
// engine and the whole recorded server group: the instance reads failed and
// its charge is released. On-demand activation then relaunches the engine
// and a new server.
#[tokio::test]
async fn a_server_exit_fails_the_instance_and_recovery_starts_a_new_server() {
    let (dir, owner, fence, observations) = lab(|_| {}).await;
    let deployment = fence.deployment_id.clone();
    let host = CacheHost::new(Report::Adapter, owner.clone());
    let processes: Vec<_> = (0..4)
        .map(|_| {
            Process(
                std::process::Command::new("sleep")
                    .arg("120")
                    .spawn()
                    .unwrap(),
            )
        })
        .collect();
    let live = |role: &str, n: usize| capyctl_testkit::live_identity(role, processes[n].0.id());
    let engine = vec![live("api", 0), live("worker-0", 1)];
    let server = live(LMCACHE_ROLE, 2);
    let member = live("lmcache-0", 3);
    *host.live_launch.lock().unwrap() =
        Some((engine.clone(), vec![server.clone(), member.clone()]));
    // The engine's processes stay alive throughout; only the server exits.
    let [_api, _worker, mut leader, mut child] = <[Process; 4]>::try_from(processes).ok().unwrap();
    let w = worker(owner.clone(), observations, host.clone());
    ready(&w, &fence).await;
    let (first, recorded) = binding(&owner, &deployment);
    assert!(recorded.contains(&server), "{recorded:?}");
    assert!(recorded.contains(&member), "{recorded:?}");
    let (phase, bytes, host_kv) = charge(&owner, &deployment).unwrap();
    assert_eq!(
        (phase, (bytes, host_kv)),
        (ResourcePhase::Ready, CACHED_READY)
    );
    assert!(instance(&dir, &deployment).1);

    child.0.kill().unwrap();
    child.0.wait().unwrap();
    let exits = crate::engine_exit::EngineExits::new(w.commands());
    let watch = exits.clone();
    let handled = tokio::task::spawn_blocking(move || watch.local_pass())
        .await
        .unwrap();
    assert!(handled.is_empty(), "a server child's exit: {handled:?}");
    let (state, open, _) = instance(&dir, &deployment);
    assert_eq!((state.as_str(), open), ("ready", true));
    assert!(events(&dir, "lmcache_server_exited", &deployment).is_empty());

    leader.0.kill().unwrap();
    leader.0.wait().unwrap();
    let watch = exits.clone();
    let handled = tokio::task::spawn_blocking(move || watch.local_pass())
        .await
        .unwrap();
    let [crate::engine_exit::ExitHandled::Settling {
        operation_id: Some(stop),
    }] = handled.as_slice()
    else {
        panic!("expected the server's exit to be settling, got {handled:?}");
    };
    until("the exit stop settled", || {
        operation(&owner, stop).0 == "succeeded"
    })
    .await;
    let (state, open, _) = instance(&dir, &deployment);
    assert_eq!((state.as_str(), open), ("stopped", false));
    assert_eq!(status(&owner, &deployment).observed_state, "failed");
    assert_eq!(charge(&owner, &deployment), None, "released");
    let cleanups = host.cleanups.lock().unwrap().clone();
    assert_eq!(cleanups.len(), 1);
    assert!(cleanups[0].identities.contains(&server));
    assert!(cleanups[0].identities.contains(&member));
    assert!(cleanups[0].identities.iter().any(|p| p.role == "api"));
    let exited = events(&dir, "lmcache_server_exited", &deployment);
    assert_eq!(exited.len(), 1, "{exited:?}");
    assert_eq!(exited[0]["binding_id"], first.as_str());
    assert_eq!(journal(&dir, "engine_exited", &deployment).len(), 1);
    // Watched again, the settled launch is no longer there to fail.
    let watch = exits.clone();
    let again = tokio::task::spawn_blocking(move || watch.local_pass())
        .await
        .unwrap();
    assert!(again.is_empty(), "{again:?}");
    assert_eq!(events(&dir, "lmcache_server_exited", &deployment).len(), 1);

    let port = crate::coordinator_port::CoordinatorLifecycle::new(w.commands());
    use crate::port::LifecyclePort;
    let activation = port.auto_activate(&deployment).await.unwrap();
    assert_eq!(
        port.wait_terminal(&activation).await.unwrap(),
        capyctl_domain::LifecycleState::Ready
    );
    assert_eq!(host.plans(), [None, None], "LMCache planned on again");
    let (second, recorded) = binding(&owner, &deployment);
    assert_ne!(second, first);
    let fresh = lmcache_members(&recorded);
    assert_eq!(fresh.len(), 1, "{recorded:?}");
    assert_ne!(fresh[0], server, "a new server");
    let (phase, bytes, host_kv) = charge(&owner, &deployment).unwrap();
    assert_eq!(
        (phase, (bytes, host_kv)),
        (ResourcePhase::Ready, CACHED_READY)
    );
    let shown = status(&owner, &deployment).lmcache.unwrap();
    assert_eq!(
        (shown.state.as_str(), shown.server.as_deref()),
        ("on", Some("running"))
    );
    w.shutdown().await.unwrap();
}

/// The lab's observations with the per-process GPU memory a host's sample
/// attributes to its processes (ADR 0007), as `nvidia-smi` gives it on
/// unified memory and on a discrete GPU alike.
struct GpuResidents(
    Vec<MemoryObservation>,
    Vec<capyctl_domain::resources::ProcessResident>,
);
impl ServiceObservation for GpuResidents {
    fn observe(&self, _: String) -> ObservationFuture {
        let values = self.0.clone();
        Box::pin(async move { Ok(values) })
    }
    fn observe_with_residents(&self, _: String) -> ResidentObservationFuture {
        let (values, residents) = (self.0.clone(), self.1.clone());
        Box::pin(async move { Ok((values, residents)) })
    }
}

/// `/proc/<pid>/status` figures of a live process, in bytes: (`VmRSS`,
/// `RssAnon` + `RssShmem`).
fn resident_figures(pid: u32) -> (i64, i64) {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let kib = |name: &str| -> i64 {
        let line = status.lines().find(|l| l.starts_with(name)).unwrap();
        line[name.len()..]
            .split_whitespace()
            .next()
            .unwrap()
            .parse::<i64>()
            .unwrap()
            * 1024
    };
    (kib("VmRSS:"), kib("RssAnon:") + kib("RssShmem:"))
}

/// One embedded launch measured on unified memory, then started again:
/// `ready_gpu` is the server's GPU memory the host's sample names at Ready,
/// `running_gpu` what the exit watch's sampler names while it runs (`None`:
/// nothing names the server). Returns (the recorded overhead and device
/// figures, the server's `VmRSS` and `RssAnon` + `RssShmem` when measured,
/// the next launch's Ready charge).
async fn measured_then_started_again(
    ready_gpu: Option<i64>,
    running_gpu: Option<i64>,
) -> ((i64, i64), (i64, i64), (ResourcePhase, i64, i64)) {
    use rusqlite::OptionalExtension;
    let (dir, owner, fence, observations) = lab(|_| {}).await;
    let deployment = fence.deployment_id.clone();
    let host = CacheHost::new(Report::Adapter, owner.clone());
    let processes: Vec<_> = (0..3)
        .map(|_| {
            Process(
                std::process::Command::new("sleep")
                    .arg("120")
                    .spawn()
                    .unwrap(),
            )
        })
        .collect();
    let live = |role: &str, n: usize| capyctl_testkit::live_identity(role, processes[n].0.id());
    let engine = vec![live("api", 0), live("worker-0", 1)];
    let server = live(LMCACHE_ROLE, 2);
    *host.live_launch.lock().unwrap() = Some((engine, vec![server.clone()]));
    let [_api, _worker, mut child] = <[Process; 3]>::try_from(processes).ok().unwrap();
    let resident = |gpu: i64| capyctl_domain::resources::ProcessResident {
        pid: server.pid,
        boot_id: server.boot_id.clone(),
        start_ticks: server.start_ticks,
        bytes: gpu,
        device_bytes: gpu,
        host_bytes: 0,
        per_device: Vec::new(),
    };
    let clock = host.clock.clone();
    let bound = host.clone();
    let w = OwnedCoordinator::spawn_with_execution_bindings(
        owner.clone(),
        Arc::new(GpuResidents(
            observations,
            ready_gpu.map(resident).into_iter().collect(),
        )),
        Arc::new(move || Ok(clock.load(Ordering::SeqCst))),
        CoordinatorOptions {
            retry_cooldown: Duration::from_millis(50),
            ..Default::default()
        },
        Arc::new(move |work: &InitializeWork| Ok(bound.bind(work))),
    )
    .unwrap();
    ready(&w, &fence).await;
    let measured = || -> Option<(i64, i64)> {
        sql(&dir)
            .query_row(
                "SELECT overhead_bytes,device_bytes FROM lmcache_measurements WHERE deployment_id=?1",
                [&deployment],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .unwrap()
    };
    assert_eq!(
        measured(),
        ready_gpu.map(|gpu| (0, gpu)),
        "measured at Ready"
    );
    let mut exits = crate::engine_exit::EngineExits::new(w.commands());
    if let Some(gpu) = running_gpu {
        let pid = server.pid;
        let sampler = capyctl_agent::process_residency::ResidencySampler::with_collector(Arc::new(
            move || Some(vec![(pid, "gpu0".into(), gpu)]),
        ));
        // The exit watch reads the latest sample without waiting for one.
        assert_eq!(sampler.sample_fresh().len(), 1);
        exits = exits.with_process_residency(sampler);
    }
    // The exit watch's pass measures the running server: a `sleep` holds a
    // few MiB, within its 1 GiB pool, so only its GPU memory counts here.
    let watch = exits.clone();
    let handled = tokio::task::spawn_blocking(move || watch.local_pass())
        .await
        .unwrap();
    assert!(handled.is_empty(), "{handled:?}");
    assert_eq!(
        measured(),
        ready_gpu.max(running_gpu).map(|gpu| (0, gpu)),
        "the most seen while it ran"
    );
    // The same launch as the watch reads it, its pool lowered to one byte so
    // the server's whole resident set lies beyond it.
    let mut launch = {
        let o = owner.lock().unwrap();
        o.store().local_lmcache_launches(o.session()).unwrap()
    };
    assert_eq!(launch.len(), 1, "{launch:?}");
    let mut launch = launch.remove(0);
    launch.pool_bytes = 1;
    let figures = resident_figures(server.pid);
    let watch = exits.clone();
    tokio::task::spawn_blocking(move || watch.measure(&launch))
        .await
        .unwrap();
    let recorded = measured().expect("the running server was measured");

    // The server exits; its stop settles, and on-demand activation starts
    // the revision again on this host.
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    let watch = exits.clone();
    let handled = tokio::task::spawn_blocking(move || watch.local_pass())
        .await
        .unwrap();
    let [crate::engine_exit::ExitHandled::Settling {
        operation_id: Some(stop),
    }] = handled.as_slice()
    else {
        panic!("expected the server's exit to be settling, got {handled:?}");
    };
    until("the exit stop settled", || {
        operation(&owner, stop).0 == "succeeded"
    })
    .await;
    let port = crate::coordinator_port::CoordinatorLifecycle::new(w.commands());
    use crate::port::LifecyclePort;
    let activation = port.auto_activate(&deployment).await.unwrap();
    assert_eq!(
        port.wait_terminal(&activation).await.unwrap(),
        capyctl_domain::LifecycleState::Ready
    );
    let next = charge(&owner, &deployment).unwrap();
    w.shutdown().await.unwrap();
    (recorded, figures, next)
}

// T45 T25 T14 (ADR 0033 §6, A1; found live 2026-10-22, GB10, SGLang): the
// overhead is the server's whole resident set beyond its pool, file-backed
// pages included (the basis A1's allowance was measured on), and its GPU
// memory, which on unified memory lies outside that resident set, is
// recorded from the host's per-process sample, at Ready and while it runs.
// The next launch charges the pool, the measured overhead and the most GPU
// memory seen.
#[tokio::test]
async fn a_measured_launch_charges_its_servers_resident_set_and_gpu_memory() {
    let (recorded, (vm_rss, anon_shmem), next) =
        measured_then_started_again(Some(300 * MIB), Some(406 * MIB)).await;
    let (overhead, device) = recorded;
    assert_eq!(device, 406 * MIB, "the most seen while it ran");
    // `VmRSS` beyond the one-byte pool, read moments apart.
    assert!(
        (overhead - (vm_rss - 1)).abs() < 4 * MIB,
        "{overhead} vs VmRSS {vm_rss}"
    );
    assert!(
        overhead > anon_shmem,
        "file-backed pages counted: {overhead} vs {anon_shmem}"
    );
    assert_eq!(
        next,
        (
            ResourcePhase::Ready,
            ENGINE_READY.0 + GIB + overhead + 406 * MIB,
            CACHED_READY.1
        )
    );
}

// T45 T25 T14 (ADR 0033 §6, A1): a server whose GPU memory no sample names
// records its overhead alone, and the next launch keeps the device
// allowance: a measurement never replaces an allowance with zero.
#[tokio::test]
async fn a_launch_without_a_gpu_reading_keeps_the_device_allowance() {
    let ((overhead, device), _, next) = measured_then_started_again(None, None).await;
    assert!(overhead > 0);
    assert_eq!(device, 0, "no reading");
    assert_eq!(
        next,
        (
            ResourcePhase::Ready,
            ENGINE_READY.0 + GIB + overhead + 512 * MIB,
            CACHED_READY.1
        )
    );
}

/// ADR 0033 §§6, 7, A5: a `restart_only` launch with LMCache on parks by
/// stopping its engine: the park's evidence names the server alone, the
/// binding keeps the server, and the parked charge is the server's plus the
/// engine KV cache it keeps mapped. The wake starts a new engine beside the
/// same server, at the same generation on the same binding. A13: the Ready
/// charge keeps that KV until a GPU reading of the server below it, which
/// returns the Ready charge.
async fn a_restart_park_keeps_the_server_and_wakes_a_new_engine(report: Report) {
    let (dir, owner, fence, observations) = lab(|_| {}).await;
    let deployment = fence.deployment_id.clone();
    let host = CacheHost::new(report, owner.clone());
    let w = worker(owner.clone(), observations, host.clone());
    ready(&w, &fence).await;
    let (_, _, generation) = instance(&dir, &deployment);
    let (launch, ready_set) = binding(&owner, &deployment);
    let server = lmcache_members(&ready_set);
    assert_eq!(server.len(), 1, "{ready_set:?}");
    let engine: Vec<_> = ready_set
        .iter()
        .filter(|p| p.is_engine())
        .cloned()
        .collect();
    let (phase, bytes, host_kv) = charge(&owner, &deployment).unwrap();
    assert_eq!(
        (phase, (bytes, host_kv)),
        (ResourcePhase::Ready, CACHED_READY)
    );

    let park = w
        .commands()
        .park("operator", &deployment, fence.revision, "park-1", 100_000)
        .unwrap();
    until("parked", || instance(&dir, &deployment).0 == "parked").await;
    assert_eq!(operation(&owner, &park.operation_id).0, "succeeded");
    let parks = host.steps(RuntimeAction::Park);
    assert_eq!(parks.len(), 1);
    assert_eq!(
        (parks[0].0.len(), parks[0].1.clone()),
        (ready_set.len(), server.clone()),
        "the park retained the launch and names the server alone"
    );
    let (parked_binding, held) = binding(&owner, &deployment);
    assert_eq!(
        (parked_binding.as_str(), held),
        (launch.as_str(), server.clone())
    );
    assert!(
        !instance(&dir, &deployment).1,
        "a parked instance never dispatches"
    );
    // LC1, A5: the server's charge plus the stopped engine's 4 GiB of KV it
    // keeps mapped; the pool alone stays against host_kv_limit.
    let (phase, bytes, host_kv) = charge(&owner, &deployment).unwrap();
    assert_eq!(
        (phase, (bytes, host_kv)),
        (ResourcePhase::Parked, (SERVER_CHARGE + 4 * GIB, GIB))
    );
    let shown = status(&owner, &deployment).lmcache.unwrap();
    assert_eq!(
        (shown.server.as_deref(), shown.charge_bytes),
        (Some("running"), Some(SERVER_CHARGE + 4 * GIB))
    );

    let port = crate::coordinator_port::CoordinatorLifecycle::new(w.commands());
    use crate::port::LifecyclePort;
    let wake = port.auto_activate(&deployment).await.unwrap();
    assert_eq!(
        port.wait_terminal(&wake).await.unwrap(),
        capyctl_domain::LifecycleState::Ready
    );
    let restores = host.steps(RuntimeAction::Restore);
    assert_eq!(restores.len(), 1);
    assert_eq!(restores[0].0, server, "the wake retained the server alone");
    let woken = restores[0].1.clone();
    assert_eq!(lmcache_members(&woken), server, "the same server");
    assert!(woken.iter().any(|p| p.role == "api"));
    assert!(woken.iter().all(|p| !engine.contains(p)), "a new engine");
    let (state, open, now_generation) = instance(&dir, &deployment);
    assert_eq!((state.as_str(), open), ("ready", true));
    assert_eq!(now_generation, generation, "the same incarnation woke");
    let (woken_binding, recorded) = binding(&owner, &deployment);
    assert_eq!(woken_binding, launch);
    assert_eq!(lmcache_members(&recorded), server);
    assert!(recorded.iter().all(|p| !engine.contains(p)), "{recorded:?}");
    let (phase, bytes, host_kv) = charge(&owner, &deployment).unwrap();
    assert_eq!(
        (phase, (bytes, host_kv)),
        (
            ResourcePhase::Ready,
            (CACHED_READY.0 + 4 * GIB, CACHED_READY.1)
        )
    );
    let measured = {
        let o = owner.lock().unwrap();
        o.store()
            .record_lmcache_overhead(o.session(), &launch, 0, 406 * MIB, 1)
            .unwrap()
    };
    assert_eq!(measured.held_kv_bytes, None, "the reading released it");
    let (phase, bytes, host_kv) = charge(&owner, &deployment).unwrap();
    assert_eq!(
        (phase, (bytes, host_kv)),
        (ResourcePhase::Ready, CACHED_READY)
    );
    assert_eq!(host.plans(), [None], "never started again");
    if report == Report::HostResult {
        // Found live 2026-10-23: the wake's canary is judged on the new
        // engine the restore named, not on the server alone it recorded.
        assert_eq!(
            *host.canaries.lock().unwrap(),
            [true, true],
            "readiness and the wake"
        );
    }
    assert!(events(&dir, "lmcache_off", &deployment).is_empty());
    w.shutdown().await.unwrap();
}

// T45 T10 T16 T14 (ADR 0033 §§6, 7, A5): the embedded adapter's evidence.
#[tokio::test]
async fn a_restart_park_keeps_the_server_and_the_wake_starts_a_new_engine_on_it() {
    a_restart_park_keeps_the_server_and_wakes_a_new_engine(Report::Adapter).await;
}

// T45 T10 T16 T14 (ADR 0033 §7, lifecycle design §4.1): the same through a
// remote host's Park (`engine_stopped`) and Restore results.
#[tokio::test]
async fn a_remote_restart_park_keeps_the_server_and_the_wake_starts_a_new_engine_on_it() {
    a_restart_park_keeps_the_server_and_wakes_a_new_engine(Report::HostResult).await;
}

/// ADR 0033 amendment A15 (found live on SGLang 0.5.22 with LMCache 0.5.6,
/// 2026-10-23: the server kept every stopped engine's KV, 5,328 MiB more per
/// restart-park, while the launch stayed charged one KV, and a later wake was
/// refused for memory). A reading of the parked server above its charge
/// raises the parked charge, and the Ready charge after the wake keeps it.
/// With no reading since the wake showing the old KV released, the next park
/// stops the instance and its server (the cleanup names both, the charge is
/// released on that evidence, journaled `lmcache_server_stopped`); the next
/// activation is a new launch with a new server.
async fn a_park_while_the_kv_is_held_stops_the_server_too(report: Report) {
    let (dir, owner, fence, observations) = lab(|_| {}).await;
    let deployment = fence.deployment_id.clone();
    let host = CacheHost::new(report, owner.clone());
    let w = worker(owner.clone(), observations, host.clone());
    ready(&w, &fence).await;
    let (launch, _) = binding(&owner, &deployment);
    let park = |key: &str| {
        w.commands()
            .park("operator", &deployment, fence.revision, key, 100_000)
            .unwrap()
    };
    let first = park("park-1");
    until("parked", || instance(&dir, &deployment).0 == "parked").await;
    assert_eq!(operation(&owner, &first.operation_id).0, "succeeded");

    // The host reports the parked server 1 GiB above the KV it is charged.
    let (host_id, step): (String, String) = sql(&dir)
        .query_row(
            "SELECT l.host_id,st.id FROM launch_lmcache l
               JOIN lifecycle_steps st ON st.binding_id=l.binding_id
               JOIN operations o ON o.id=st.operation_id
              WHERE l.binding_id=?1 AND o.kind='initialize'",
            [&launch],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let measured = {
        let o = owner.lock().unwrap();
        o.store()
            .record_host_lmcache_measurement(
                o.session(),
                &host_id,
                &deployment,
                fence.generation,
                &step,
                GIB,
                512 * MIB + 5 * GIB,
                1,
            )
            .unwrap()
    };
    assert_eq!(measured.charged_device_bytes, Some(512 * MIB + 5 * GIB));
    let (phase, bytes, _) = charge(&owner, &deployment).unwrap();
    assert_eq!(
        (phase, bytes),
        (ResourcePhase::Parked, SERVER_CHARGE + 5 * GIB)
    );
    assert_eq!(journal(&dir, "lmcache_charge_raised", &deployment).len(), 1);

    let port = crate::coordinator_port::CoordinatorLifecycle::new(w.commands());
    use crate::port::LifecyclePort;
    let wake = port.auto_activate(&deployment).await.unwrap();
    assert_eq!(
        port.wait_terminal(&wake).await.unwrap(),
        capyctl_domain::LifecycleState::Ready
    );
    let (phase, bytes, _) = charge(&owner, &deployment).unwrap();
    assert_eq!(
        (phase, bytes),
        (ResourcePhase::Ready, CACHED_READY.0 + 5 * GIB)
    );

    // No reading released the old KV: this park stops it all.
    let second = park("park-2");
    until("stopped", || instance(&dir, &deployment).0 == "stopped").await;
    assert_eq!(operation(&owner, &second.operation_id).0, "succeeded");
    assert_eq!(
        host.steps(RuntimeAction::Park).len(),
        1,
        "never parked again"
    );
    let stopped = host
        .cleanups
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .identities
        .clone();
    assert!(
        stopped.iter().any(|p| p.role == LMCACHE_ROLE) && stopped.iter().any(|p| p.role == "api"),
        "the stop named the engine and the server: {stopped:?}"
    );
    assert_eq!(charge(&owner, &deployment), None);
    let why = journal(&dir, "lmcache_server_stopped", &deployment);
    assert_eq!(why.len(), 1);
    assert!(why[0].starts_with("held_kv_unreleased: "), "{why:?}");

    // The next activation starts a new launch with a new server.
    let start = port.auto_activate(&deployment).await.unwrap();
    assert_eq!(
        port.wait_terminal(&start).await.unwrap(),
        capyctl_domain::LifecycleState::Ready
    );
    assert_eq!(host.plans(), [None, None]);
    let (next, recorded) = binding(&owner, &deployment);
    assert_ne!(next, launch);
    assert!(lmcache_members(&recorded)
        .iter()
        .all(|p| !stopped.contains(p)));
    assert_eq!(
        charge(&owner, &deployment).map(|(phase, bytes, _)| (phase, bytes)),
        Some((ResourcePhase::Ready, CACHED_READY.0))
    );
    w.shutdown().await.unwrap();
}

// T45 T10 T16 T14 (ADR 0033 amendment A15): the embedded adapter's evidence.
#[tokio::test]
async fn a_park_while_the_kv_is_held_stops_the_instance_with_its_server() {
    a_park_while_the_kv_is_held_stops_the_server_too(Report::Adapter).await;
}

// T45 T10 T16 T14 (ADR 0033 amendment A15): the same through a remote
// host's results.
#[tokio::test]
async fn a_remote_park_while_the_kv_is_held_stops_the_instance_with_its_server() {
    a_park_while_the_kv_is_held_stops_the_server_too(Report::HostResult).await;
}

/// A loopback SGLang `/metrics` answering the launch's inference key with
/// the running gauge `running` holds (SGLang always runs with
/// `--enable-metrics` under CapyCTL). Returns its endpoint.
async fn sglang_gauges(running: Arc<AtomicU32>) -> String {
    use axum::{http::HeaderMap, http::StatusCode, response::IntoResponse};
    let app = axum::Router::new().route(
        "/metrics",
        axum::routing::get(move |headers: HeaderMap| {
            let running = running.load(Ordering::SeqCst);
            async move {
                if headers.get("authorization").and_then(|v| v.to_str().ok())
                    != Some("Bearer inference-secret")
                {
                    return StatusCode::UNAUTHORIZED.into_response();
                }
                format!("sglang:num_running_reqs {running}\nsglang:num_queue_reqs 0\n")
                    .into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    endpoint
}

/// An embedded SGLang launch: the scripted launch carries out every step,
/// and the real SGLang adapter of a frozen `restart_only` launch (no memory
/// saver, no saver observer), sharing the launch's LMCache state, answers
/// the coordinator's quiescence question before a park.
struct EmbeddedSglang {
    launch: Arc<dyn EngineAdapter>,
    sglang: capyctl_adapters::sglang::SglangAdapter,
}

#[async_trait::async_trait]
impl EngineAdapter for EmbeddedSglang {
    async fn execute_persisted(
        &self,
        command: &RuntimeCommand,
    ) -> Result<EffectObservation, RuntimeError> {
        self.launch.execute_persisted(command).await
    }
    async fn inspect(&self, m: &MemberRef) -> Result<EngineState, AdapterError> {
        self.launch.inspect(m).await
    }
    async fn render_plan(&self, p: &PlanInput) -> Result<RenderedCommand, AdapterError> {
        self.launch.render_plan(p).await
    }
    async fn check_readiness(&self, m: &MemberRef) -> Result<Readiness, AdapterError> {
        self.launch.check_readiness(m).await
    }
    async fn prepare_park(&self, m: &MemberRef) -> Result<Quiescence, AdapterError> {
        self.sglang.prepare_park(m).await
    }
    async fn park(&self, m: &MemberRef, level: ParkLevel) -> Result<ParkOutcome, AdapterError> {
        self.launch.park(m, level).await
    }
    async fn restore(&self, m: &MemberRef) -> Result<RestoreOutcome, AdapterError> {
        self.launch.restore(m).await
    }
    async fn reload_weights(&self, m: &MemberRef) -> Result<ReloadOutcome, AdapterError> {
        self.launch.reload_weights(m).await
    }
    async fn observe_work(&self, m: &MemberRef) -> Result<WorkObservation, AdapterError> {
        self.launch.observe_work(m).await
    }
    async fn cancel_work(
        &self,
        m: &MemberRef,
        r: &RequestRef,
        force: bool,
    ) -> Result<CancellationOutcome, AdapterError> {
        self.launch.cancel_work(m, r, force).await
    }
}

/// The frozen SGLang launch of a `restart_only` deployment: no memory saver.
fn restart_only_sglang(endpoint: String) -> capyctl_domain::launch::NativeLaunch {
    use capyctl_domain::launch::{NativeDeviceSelection, NativeLaunch, NativeLaunchMetadata};
    let mut settings = capyctl_testkit::sglang_launch_settings();
    settings.memory_saver = false;
    NativeLaunch::from_frozen_store(
        NativeLaunchMetadata {
            engine: "sglang".into(),
            recipe: capyctl_adapters::sglang::pinned::NATIVE_SGLANG_CONTRACT.into(),
            checkpoint_revision: "sha256:cached".into(),
            binding_id: "01K00000000000000000000001".into(),
            incarnation: "01K00000000000000000000002".into(),
            endpoint,
            served_name: "cached".into(),
            rendered_settings_digest: "a".repeat(64),
            placement_digest: None,
            device: NativeDeviceSelection {
                host_id: "lab".into(),
                hardware_fingerprint: "hw-01".into(),
                device_id: "gpu0".into(),
                memory_domain: "unified".into(),
                physical_gpu_uuid: None,
                cuda_pci_index: None,
            },
        },
        "/private/checkpoint".into(),
        "/usr/bin/python3".into(),
        "secret://engine-key".into(),
        "secret://admin-key".into(),
        settings,
    )
}

// T45 T10 T16 T20 (ADR 0033 §7, A8, SPEC §10 step 4; found live on SGLang
// 0.5.22): an embedded `restart_only` SGLang launch with LMCache on has no
// memory saver, so the coordinator's quiescence check before its park is the
// engine's own gauges. While a request runs on the engine the park is refused
// `park_refused` before anything is sent, and the instance serves again; once
// the gauges read idle the park stops the engine and keeps the server.
#[tokio::test]
async fn an_embedded_sglang_restart_park_is_proven_quiescent_on_the_engines_gauges() {
    let (dir, owner, fence, observations) = lab(|_| {}).await;
    let deployment = fence.deployment_id.clone();
    let host = CacheHost::new(Report::Adapter, owner.clone());
    // An embedded launch's evidence names processes that are alive.
    let processes: Vec<_> = (0..3)
        .map(|_| {
            Process(
                std::process::Command::new("sleep")
                    .arg("120")
                    .spawn()
                    .unwrap(),
            )
        })
        .collect();
    let live = |role: &str, n: usize| capyctl_testkit::live_identity(role, processes[n].0.id());
    *host.live_launch.lock().unwrap() = Some((
        vec![live("api", 0), live("worker-0", 1)],
        vec![live(LMCACHE_ROLE, 2)],
    ));
    let running = Arc::new(AtomicU32::new(0));
    let endpoint = sglang_gauges(running.clone()).await;
    let bound = host.clone();
    let clock = host.clock.clone();
    let w = OwnedCoordinator::spawn(
        owner.clone(),
        Arc::new(Observations(observations)),
        Arc::new(move || Ok(clock.load(Ordering::SeqCst))),
        CoordinatorOptions {
            retry_cooldown: Duration::from_millis(50),
            ..Default::default()
        },
        Arc::new(move |work: &InitializeWork| {
            let binding = bound.bind(work);
            let state = binding.lmcache.clone().unwrap();
            let frozen = restart_only_sglang(endpoint.clone());
            let sglang = capyctl_adapters::sglang::SglangAdapter::from_frozen(&frozen, None)
                .unwrap()
                .with_launch(frozen)
                .with_credentials("inference-secret".into(), "admin-secret".into())
                .with_lmcache(state);
            let tools: Arc<dyn OwnedProcessLaunch> = capyctl_testkit::ScriptedTool::proving();
            Ok(Arc::new(Driver {
                engine: Arc::new(EmbeddedSglang {
                    launch: binding.engine,
                    sglang,
                }),
                cleanup: binding.cleanup,
                tools: Some(tools),
                settle: None,
                lmcache: binding.lmcache,
            }))
        }),
    )
    .unwrap();
    ready(&w, &fence).await;

    running.store(1, Ordering::SeqCst);
    let busy = w
        .commands()
        .park(
            "operator",
            &deployment,
            fence.revision,
            "park-busy",
            100_000,
        )
        .unwrap();
    until("the busy park refused", || {
        operation(&owner, &busy.operation_id).0 == "failed"
    })
    .await;
    assert_eq!(
        operation(&owner, &busy.operation_id).1.as_deref(),
        Some("park_refused")
    );
    assert!(host.steps(RuntimeAction::Park).is_empty(), "nothing sent");
    let (state, open, _) = instance(&dir, &deployment);
    assert_eq!((state.as_str(), open), ("ready", true));

    running.store(0, Ordering::SeqCst);
    let park = w
        .commands()
        .park(
            "operator",
            &deployment,
            fence.revision,
            "park-idle",
            100_000,
        )
        .unwrap();
    until("parked", || instance(&dir, &deployment).0 == "parked").await;
    assert_eq!(operation(&owner, &park.operation_id).0, "succeeded");
    assert_eq!(host.steps(RuntimeAction::Park).len(), 1);
    w.shutdown().await.unwrap();
    drop(processes);
}
