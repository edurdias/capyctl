//! ADR 0033 (lifecycle design §§2.3, 4.2, 5, 9): the remote host's half of a
//! launch's LMCache: the plan with this host's own inputs, the launch-time
//! check, the server a launch starts, the re-proof of a running server, and
//! the restart-park Park and Restore of a `restart_only` launch.
use super::{group_member, LaunchVerdict, NativeHostExecution, PROBE_MARGIN_MS};
use crate::{ingress::IngressScope, journal::ResidencyOutcome, session::SessionError};
use capyctl_adapters::{
    lmcache::{LmcacheLaunch, LmcacheReport, LmcacheState, ServerPlan},
    traits::{OwnedProcessLaunch, RuntimeAction, RuntimeCommand, RuntimeError},
};
use capyctl_config::{
    effective::{EffectiveDeployment, Residency},
    engine_policy::Engine,
    lmcache::{
        apply_charge, charge_domains, launch_gpu, launch_model_type, plan_with_table, HostLmcache,
        LmcacheMode, LmcacheOff, LmcacheOffReason, LmcachePlan, PlanInputs, QualifiedParking,
        Retention, GIB, QUALIFIED_PARKING, SERVER_START_BOUND_MS,
    },
};
use capyctl_domain::completion::{
    lmcache_members, ExecutionIdentities, ProcessIdentity, StepExecutionContext, TransitionToken,
};
use capyctl_protocol::{
    execution::{LaunchLmcache, LmcacheGrant, MemberAction, MemberCommand, SingleLaunchPlan},
    pb,
};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// The launch-time check's verdict for each admitted command, by its
/// canonical digest: `None` when LMCache may start, else why it is off.
pub(super) type LmcacheVerdicts = Mutex<HashMap<[u8; 32], (Instant, Option<LmcacheOff>)>>;

/// The longest SIGTERM grace a restart-park gives the engine it stops; half
/// the step's remaining budget when that is less.
const ENGINE_STOP_GRACE: Duration = Duration::from_secs(10);

/// ADR 0033 §14: the closed code a `lmcache: required` launch is refused
/// with when its LMCache is off for `reason` (`reason.required_refusal()`,
/// as the static text a refusal carries).
pub(super) fn required_refusal(reason: LmcacheOffReason) -> &'static str {
    use LmcacheOffReason as R;
    match reason {
        R::Disabled => "lmcache_unavailable:disabled",
        R::HostOff => "lmcache_unavailable:host_off",
        R::EngineUnsupported => "lmcache_unavailable:engine_unsupported",
        R::EngineVersionUnsupported => "lmcache_unavailable:engine_version_unsupported",
        R::NotInstalled => "lmcache_unavailable:not_installed",
        R::NotRegistered => "lmcache_unavailable:not_registered",
        R::VersionUnsupported => "lmcache_unavailable:version_unsupported",
        R::CheckFailed => "lmcache_unavailable:check_failed",
        R::GroupUnsupported => "lmcache_unavailable:group_unsupported",
        R::MultiGpuUnqualified => "lmcache_unavailable:multi_gpu_unqualified",
        R::ParkUnqualified => "lmcache_unavailable:park_unqualified",
        R::KvTransferConfigSet => "lmcache_unavailable:kv_transfer_config_set",
        R::RetentionZero => "retention_zero:lmcache",
        R::HostCapabilityMissing => "host_capability_missing:lmcache",
        R::ServerStartFailed => "lmcache_unavailable:server_start_failed",
        R::ListenerExposed => "lmcache_unavailable:listener_exposed",
        R::EngineStartFailed => "lmcache_unavailable:engine_start_failed",
    }
}

/// The wire report of what became of a launch's LMCache.
pub(super) fn launch_report(report: &LmcacheReport) -> LaunchLmcache {
    match report {
        LmcacheReport::On {
            rpc_port,
            http_port,
            pool_bytes,
            ..
        } => LaunchLmcache::on(*rpc_port, *http_port, *pool_bytes),
        LmcacheReport::Off {
            reason,
            detail,
            connector_failed,
        } => LaunchLmcache::off(*reason, detail, *connector_failed),
    }
}

/// ADR 0033 §§3, 6: check the grant's ports against the host's endpoint
/// range and lay its charge over the launch's phases with
/// [`apply_charge`], so admission, co-residence and retained claims reserve
/// the bytes the store reserved. `false` when the grant cannot be honoured.
pub(super) fn apply_grant(
    effective: &mut EffectiveDeployment,
    grant: &LmcacheGrant,
    service_port: u16,
) -> bool {
    let range = &effective.host.endpoint_port_range;
    let ports = [grant.rpc_port, grant.http_port];
    if grant.rpc_port == grant.http_port
        || ports
            .iter()
            .any(|port| *port < range.start || *port > range.end || *port == service_port)
    {
        return false;
    }
    let Some(domains) = charge_domains(&effective.host, &effective.selected_devices) else {
        return false;
    };
    // ADR 0033 §7: a `restart_only` launch parks with its server alone.
    let restart_park = effective.residency == Residency::RestartOnly;
    apply_charge(
        &mut effective.resources,
        &grant.charge(),
        restart_park,
        &domains,
    );
    true
}

/// Process tools that spawn nothing: they observe and stop a retained
/// launch's recorded groups, for its re-proof and its restart-park.
fn group_tools() -> Arc<dyn OwnedProcessLaunch> {
    struct NoSpawn;
    impl capyctl_launchers::LaunchAssociation for NoSpawn {
        fn persist_api_identity(
            &self,
            _: &ProcessIdentity,
        ) -> Result<(), capyctl_launchers::AssociationError> {
            Err(capyctl_launchers::AssociationError::Uncertain(
                "these tools spawn nothing".into(),
            ))
        }
    }
    Arc::new(capyctl_launchers::DurableProcessLaunch::new(Arc::new(
        NoSpawn,
    )))
}

/// What a Park or Restore ended with, and the group its claim holds for
/// when it is not the one the step began from (a restart-park's).
pub(super) type Drive = (
    ResidencyOutcome,
    pb::ResidencyEvidence,
    Option<Vec<ProcessIdentity>>,
);

/// How a restart-park step ended: its outcome, its evidence (memory before
/// and now, milestones, whether the engine was stopped) and the group its
/// claim holds for.
fn finish(
    outcome: ResidencyOutcome,
    before: i64,
    milestones: Vec<String>,
    held: Option<Vec<ProcessIdentity>>,
    engine_stopped: bool,
) -> Result<Drive, SessionError> {
    let mut evidence =
        super::residency::evidence(before, super::residency::mem_available(), milestones);
    evidence.engine_stopped = engine_stopped;
    Ok((outcome, evidence, held))
}

/// [`NativeHostExecution::lmcache_plan`] against `table` in place of
/// [`QUALIFIED_PARKING`]. ADR 0033 §7 (amendment A12): the checkpoint's
/// `model_type` is read from its `config.json` here, where the launch reads
/// it, and the GPU is the one this host published for the launch's device.
pub(super) fn lmcache_plan_in(
    effective: &EffectiveDeployment,
    topology: bool,
    table: &[QualifiedParking],
) -> LmcachePlan {
    let args: Vec<String> = effective
        .profile
        .args
        .iter()
        .chain(effective.engine_config.extra_args())
        .cloned()
        .collect();
    let model_type = launch_model_type(effective);
    plan_with_table(
        &PlanInputs {
            mode: effective.lmcache,
            pool_bytes: effective.lmcache_pool_bytes,
            retention: effective.retention,
            host_lmcache: effective.host.lmcache,
            host_retention: effective.host.retention,
            // This host implements ADR 0033 and declares both capabilities.
            host_declares_lmcache: true,
            host_declares_retention: true,
            engine: effective.profile.engine,
            engine_version: &effective.profile.build_fingerprint,
            api: effective.api,
            topology,
            // ADR 0035 §9: LMCache stays off on several GPUs of one host.
            multi_gpu: effective.local_tensor_parallel() > 1,
            residency: effective.residency,
            model_type: model_type.as_deref(),
            gpu: launch_gpu(effective),
            registration: effective.profile.lmcache.as_ref(),
            kv_transfer_config_set: capyctl_config::engine_policy::kv_transfer_config_set(
                effective.profile.engine,
                &args,
            ),
            radix_cache_disabled: capyctl_config::engine_policy::radix_cache_disabled(
                effective.profile.engine,
                &args,
            ),
            engine_start_failed: false,
            discrete_gpu: charge_domains(&effective.host, &effective.selected_devices)
                .is_some_and(|domains| domains.device.is_some()),
        },
        table,
    )
}

impl NativeHostExecution {
    /// Contract decision 2: whether LMCache runs for `effective` on this host,
    /// from this host's own inputs. A sticky `engine_start_failed` fallback
    /// is the store's: that launch arrives without a grant.
    pub(super) fn lmcache_plan(
        &self,
        effective: &EffectiveDeployment,
        topology: bool,
    ) -> LmcachePlan {
        lmcache_plan_in(effective, topology, QUALIFIED_PARKING)
    }

    /// ADR 0033 §§1, 9 (lifecycle design §2.3): the LMCache half of a
    /// granted launch's admission, after the installation verify. A drifted
    /// LMCache is handled as the engine package's drift; under `required`
    /// a launch whose LMCache would be off is refused before any effect;
    /// under `auto` the verdict is kept for the launch, which turns LMCache
    /// off before Initialize.
    pub(super) fn admit_lmcache(
        &self,
        command: &MemberCommand,
        plan: &SingleLaunchPlan,
        effective: &EffectiveDeployment,
        installations: &crate::installation::InstallationRegistry,
    ) -> Result<(), LaunchVerdict> {
        if plan.lmcache.is_none() {
            return Ok(());
        }
        let verdict = self
            .lmcache_verdict(command, plan, effective, installations)
            .map_err(LaunchVerdict::Refused)?;
        let mut verdicts = self
            .lmcache_verdicts
            .lock()
            .map_err(|_| LaunchVerdict::Uncertain)?;
        verdicts.retain(|_, (at, _)| at.elapsed() < super::PRE_ADMISSION_TTL);
        if verdicts.len() < super::MAX_PRE_ADMISSIONS {
            verdicts.insert(command.canonical_digest(), (Instant::now(), verdict));
        }
        Ok(())
    }

    /// The launch-time verdict of a granted launch: `Ok(None)` when its
    /// server may start, `Ok(Some(off))` when LMCache is off for it, `Err`
    /// with the closed refusal (`required`, or drift under `refuse`).
    fn lmcache_verdict(
        &self,
        command: &MemberCommand,
        plan: &SingleLaunchPlan,
        effective: &EffectiveDeployment,
        installations: &crate::installation::InstallationRegistry,
    ) -> Result<Option<LmcacheOff>, &'static str> {
        let host = self.lmcache_plan(effective, group_member(command).is_some());
        let off = match host.off() {
            Some(off) => Some(off.clone()),
            None if self.server_plan(effective, plan).is_none() => Some(LmcacheOff::with_detail(
                LmcacheOffReason::CheckFailed,
                "the host has no private LMCache directory for the launch",
            )),
            None => match installations.verify_lmcache(
                &plan.profile_name,
                effective.profile.engine,
                Path::new(&effective.profile.executable),
                effective.profile.lmcache.as_ref(),
                effective.profile.security.installation_drift,
            )? {
                Ok(_) => None,
                Err(failure) => Some(LmcacheOff::with_detail(failure.reason, failure.detail)),
            },
        };
        match off {
            Some(off) if effective.lmcache == LmcacheMode::Required => {
                Err(required_refusal(off.reason))
            }
            off => Ok(off),
        }
    }

    /// ADR 0033 §§3, 9: what the Initialize of `command` does about LMCache.
    /// A grant with this host's plan on starts a server, unless the
    /// launch-time check turned it off (the verdict admission kept, or the
    /// check run again now); without a grant the store planned it off, and
    /// the host's own reason is reported when it has one. `Ok(None)`:
    /// nothing to start or report. `Err`: a closed refusal found only now.
    pub(super) fn lmcache_state(
        &self,
        command: &MemberCommand,
        plan: &SingleLaunchPlan,
        effective: &EffectiveDeployment,
    ) -> Result<Option<Arc<LmcacheState>>, &'static str> {
        if !matches!(command.action, MemberAction::LaunchSingle(_)) {
            return Ok(None);
        }
        let host = self.lmcache_plan(effective, false);
        if let Some(off) = host.off() {
            return Ok(Some(LmcacheState::off(off.reason, off.detail.clone())));
        }
        if plan.lmcache.is_none() {
            return Ok(None);
        }
        let Some(server) = self.server_plan(effective, plan) else {
            return Ok(Some(LmcacheState::off(
                LmcacheOffReason::CheckFailed,
                "the host has no private LMCache directory for the launch".into(),
            )));
        };
        let cached = self.lmcache_verdicts.lock().ok().and_then(|verdicts| {
            verdicts
                .get(&command.canonical_digest())
                .filter(|(at, _)| at.elapsed() < super::PRE_ADMISSION_TTL)
                .map(|(_, verdict)| verdict.clone())
        });
        let verdict = match cached {
            Some(verdict) => verdict,
            None => {
                self.lmcache_verdict(command, plan, effective, &self.installations_for(plan))?
            }
        };
        let state = LmcacheState::new(LmcacheLaunch::Start(server));
        if let Some(off) = verdict {
            state.turn_off(off.reason, off.detail);
        }
        Ok(Some(state))
    }

    /// ADR 0033 §3: the server `plan`'s grant names, in the registered
    /// environment (`<environment>/bin/lmcache`, beside its interpreter),
    /// with the environment ADR 0033 §3 allows, its log beside the engine's
    /// and SGLang's configuration in the launch's private directory.
    pub(super) fn server_plan(
        &self,
        effective: &EffectiveDeployment,
        plan: &SingleLaunchPlan,
    ) -> Option<ServerPlan> {
        let grant = plan.lmcache?;
        let executable = Path::new(&effective.profile.executable);
        let bin = crate::installation::interpreter(executable)
            .as_deref()
            .and_then(Path::parent)
            .or_else(|| executable.parent())?
            .join("lmcache");
        let gpu_env = effective
            .cuda_namespace()
            .ok()
            .flatten()
            .map(|namespace| namespace.environment())
            .unwrap_or_default();
        let find = |wanted: &str| {
            gpu_env
                .iter()
                .find_map(|(name, value)| (*name == wanted).then(|| value.clone()))
        };
        let cuda_visible_devices = find("CUDA_VISIBLE_DEVICES");
        let cuda_device_order = find("CUDA_DEVICE_ORDER");
        Some(ServerPlan {
            lmcache_bin: bin,
            rpc_port: grant.rpc_port,
            http_port: grant.http_port,
            pool_gib: u64::try_from(grant.pool_bytes).ok()? / GIB,
            path: std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
            home: std::env::var("HOME").ok(),
            cuda_visible_devices,
            cuda_device_order,
            log: Some(capyctl_adapters::lmcache::server_log_path(
                &self.engine_log_path(&plan.incarnation),
            )),
            config_dir: self.lmcache_dirs.as_ref()?.launch_dir(&plan.incarnation)?,
            start_bound: Duration::from_millis(SERVER_START_BOUND_MS),
            proc_root: "/proc".into(),
            required: effective.lmcache == LmcacheMode::Required,
        })
    }

    /// ADR 0033 §§3, 5, 9: the re-proof of a retained launch's LMCache
    /// server, beside its engine's: refused while this host's approved
    /// document turns LMCache off or keeps zero retention (dispatch stays
    /// closed), else the server alive, answering `GET /status` and listening
    /// on loopback alone. An exposed server is stopped (verified) and the
    /// re-proof fails. A launch without a server has nothing to prove.
    pub(super) async fn reprove_lmcache(
        &self,
        plan: &SingleLaunchPlan,
        effective: &EffectiveDeployment,
        expected: &[ProcessIdentity],
    ) -> Result<(), SessionError> {
        let server = lmcache_members(expected);
        if server.is_empty() {
            return Ok(());
        }
        if self.lmcache_forbidden(effective) {
            return Err(SessionError);
        }
        let server_plan = self.server_plan(effective, plan).ok_or(SessionError)?;
        capyctl_adapters::lmcache::prove_server(group_tools(), &server_plan, &server)
            .await
            .map(drop)
            .map_err(|_| SessionError)
    }

    /// ADR 0033 §§9, 10: this host's approved document turns LMCache off or
    /// keeps zero retention (or the launch's own retention is zero), so no
    /// LMCache server of the launch may serve.
    pub(super) fn lmcache_forbidden(&self, effective: &EffectiveDeployment) -> bool {
        let (lmcache, retention) =
            capyctl_config::lmcache::host_settings(&self.profiles.accepted().config.document);
        lmcache == HostLmcache::Off
            || retention == Retention::Zero
            || effective.host.lmcache == HostLmcache::Off
            || effective.host.retention == Retention::Zero
            || effective.retention == Retention::Zero
    }

    /// ADR 0033 §7: whether a Park or Restore of `owner` is a restart-park:
    /// a `restart_only` vLLM or SGLang single launch granted an LMCache
    /// server. Read from the launch alone: the journal asks this under its
    /// own lock. The drive checks the launch reported LMCache on.
    pub(super) fn restart_parks(
        &self,
        owner: &MemberCommand,
        effective: &EffectiveDeployment,
    ) -> bool {
        matches!(&owner.action, MemberAction::LaunchSingle(plan) if plan.lmcache.is_some())
            && effective.residency == Residency::RestartOnly
            && matches!(effective.profile.engine, Engine::Vllm | Engine::Sglang)
    }

    /// ADR 0033 §7 (lifecycle design §4.2): the Park or Restore of a
    /// restart-parked launch, after its gate closed. Park proves ingress
    /// idle and the engine quiescent on its own gauges (A8), stops the
    /// engine's group and helpers (verified gone), keeps the
    /// server and claims `parked` for the server's group alone. Restore
    /// starts the engine again on that server through a fresh adapter (the
    /// server re-proved, the connector, one wake spawn, readiness and the
    /// probe) and claims `restored` for the new engine beside the server.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn restart_park(
        &self,
        session: u64,
        command: &MemberCommand,
        owner: &MemberCommand,
        plan: &SingleLaunchPlan,
        expected: &[ProcessIdentity],
        scope: &IngressScope,
        effective: EffectiveDeployment,
        mut milestones: Vec<String>,
    ) -> Result<Drive, SessionError> {
        let server = lmcache_members(expected);
        match &command.action {
            MemberAction::Park { .. } => {
                if self.ingress.current_requests(scope).ok() != Some(0) || server.is_empty() {
                    return finish(ResidencyOutcome::Unchanged, -1, milestones, None, false);
                }
                milestones.push("ingress_idle".into());
                // SPEC §10 step 4, ADR 0033 A8: then the engine's own gauges.
                if !self.engine_idle(owner, plan, scope).await {
                    return finish(ResidencyOutcome::Unchanged, -1, milestones, None, false);
                }
                milestones.push("engine_quiescent".into());
                let before = super::residency::mem_available();
                let journal = self.journal.clone();
                let handle = owner.identity.command_id.clone();
                let engine: Vec<ProcessIdentity> =
                    tokio::task::spawn_blocking(move || journal.inspect_owned(&handle))
                        .await
                        .map_err(|_| SessionError)?
                        .map_err(|_| SessionError)?
                        .into_iter()
                        .map(|(identity, _)| identity)
                        .filter(|identity| !identity.is_lmcache())
                        .collect();
                if !engine.iter().any(|identity| identity.role == "api") {
                    return finish(ResidencyOutcome::Unchanged, before, milestones, None, false);
                }
                let left = Duration::from_millis(
                    u64::try_from(
                        command.identity.deadline_ms
                            - PROBE_MARGIN_MS
                            - capyctl_protocol::now_unix_ms(),
                    )
                    .unwrap_or(0),
                );
                let grace = ENGINE_STOP_GRACE.min(left / 2);
                let journal = self.journal.clone();
                let handle = owner.identity.command_id.clone();
                let stopped = tokio::task::spawn_blocking(move || {
                    group_tools()
                        .stop_group(&engine, grace)
                        .map_err(|_| ())
                        .and_then(|()| journal.depart_stopped(&handle, &engine).map_err(|_| ()))
                })
                .await
                .map_err(|_| SessionError)?;
                if stopped.is_err() {
                    return finish(ResidencyOutcome::Uncertain, before, milestones, None, false);
                }
                milestones.push("engine_stopped".into());
                // SPEC §8.2 / T21: the stopped engine's file rendezvous is
                // stale; its next engine makes its own.
                if let Some(rendezvous) = &self.rendezvous {
                    rendezvous.retire(&plan.incarnation);
                }
                finish(
                    ResidencyOutcome::Parked,
                    before,
                    milestones,
                    Some(server),
                    true,
                )
            }
            MemberAction::Restore {
                checkpoint_digest, ..
            } => {
                let before = super::residency::mem_available();
                // ADR 0014 §7: the engine loads its weights from disk again.
                if !self
                    .checkpoint_unchanged(owner, plan, checkpoint_digest)
                    .await
                {
                    return finish(ResidencyOutcome::Unchanged, before, milestones, None, false);
                }
                milestones.push("checkpoint_verified".into());
                let Some(server_plan) = self.server_plan(&effective, plan) else {
                    return finish(ResidencyOutcome::Unchanged, before, milestones, None, false);
                };
                let journal = self.journal.clone();
                let restore_id = command.identity.command_id.clone();
                let spawn = super::spawn_policy(&effective);
                let Ok(Ok(tools)) = tokio::task::spawn_blocking(move || {
                    journal.wake_tools(&restore_id, session, capyctl_protocol::now_unix_ms(), spawn)
                })
                .await
                else {
                    return finish(ResidencyOutcome::Unchanged, before, milestones, None, false);
                };
                let keys = self
                    .identities
                    .load(scope, owner.identity.payload_digest)
                    .map_err(|_| SessionError)?;
                let served = effective.routes.first().cloned().ok_or(SessionError)?;
                let state = LmcacheState::attached(server.clone(), server_plan);
                let prepared = self.prepare(&effective, plan, &served, None)?;
                let adapter = self.launch_adapter(
                    prepared,
                    &effective,
                    plan,
                    Some(&keys),
                    &served,
                    tools,
                    Some(state.clone()),
                )?;
                let id = &command.identity;
                let step = RuntimeCommand {
                    action: RuntimeAction::Restore,
                    context: StepExecutionContext {
                        token: TransitionToken {
                            deployment_id: id.deployment_id.clone(),
                            revision: id.revision,
                            generation: id.generation,
                            operation_id: id.operation_id.clone(),
                            // The wake key's step: the Restore's own.
                            step_id: id.step_id.clone(),
                        },
                        binding_id: plan.binding_id.clone(),
                        incarnation: plan.incarnation.clone(),
                        issued_at_ms: capyctl_protocol::now_unix_ms(),
                        deadline_ms: id.deadline_ms - PROBE_MARGIN_MS,
                        identities: ExecutionIdentities::Retained(server),
                        completion_target: None,
                        grant_id: Some(plan.grant_id.clone()),
                        launch_settings: None,
                    },
                };
                let woken = adapter.execute_persisted(&step).await;
                // ADR 0033 §§8, 9: the Restore's own LMCache report.
                if let Some(report) = state.report() {
                    let journal = self.journal.clone();
                    let restore_id = id.command_id.clone();
                    tokio::task::spawn_blocking(move || {
                        journal.record_launch_lmcache(&restore_id, &launch_report(&report))
                    })
                    .await
                    .map_err(|_| SessionError)?
                    .map_err(|_| SessionError)?;
                }
                let observed = match woken {
                    Ok(observed) => observed,
                    Err(RuntimeError::Unsupported) => {
                        return finish(ResidencyOutcome::Unchanged, before, milestones, None, false)
                    }
                    Err(_) => {
                        return finish(ResidencyOutcome::Uncertain, before, milestones, None, false)
                    }
                };
                milestones.extend(["engine_started".into(), "model_usable".into()]);
                if !self
                    .unchanged_group(&owner.identity.command_id, &observed.identities)
                    .await
                {
                    return finish(ResidencyOutcome::Uncertain, before, milestones, None, false);
                }
                milestones.push("identity_unchanged".into());
                finish(
                    ResidencyOutcome::Restored,
                    before,
                    milestones,
                    Some(observed.identities),
                    false,
                )
            }
            _ => Err(SessionError),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // T45: the static refusal is the one `required_refusal` names.
    #[test]
    fn every_required_refusal_is_the_closed_code() {
        for reason in LmcacheOffReason::ALL {
            assert_eq!(required_refusal(reason), reason.required_refusal());
        }
    }

    /// ADR 0033 §7 (amendment A12): the fixture deployment deep-parking a
    /// checkpoint whose `config.json` names `model_type`, resolved on a host
    /// whose GPU publishes `NVIDIA GB10` and whose vLLM 0.32.0 profile has
    /// LMCache 0.5.6 registered.
    fn deep_on_gb10(model_type: &str) -> (EffectiveDeployment, tempfile::TempDir) {
        use serde_json::json;
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../capyctl-config/tests/fixtures/f2-deployment.json"
        ))
        .unwrap();
        let (mut config, mut host) = (fixture["deployment"].clone(), fixture["host"].clone());
        let profile = &mut host["runtime_profiles"]["local"];
        profile["build_fingerprint"] = json!("0.32.0");
        profile["lmcache"] =
            json!({"version": "0.5.6", "digest": format!("sha256:{}", "0".repeat(64))});
        host["resource_policy"]["devices"]["gpu0"]["product_name"] = json!("NVIDIA GB10");
        let checkpoint = tempfile::tempdir().unwrap();
        std::fs::write(
            checkpoint.path().join("config.json"),
            json!({ "model_type": model_type }).to_string(),
        )
        .unwrap();
        config["residency"] = json!("deep");
        config["lmcache_pool"] = json!("1GiB");
        config["model"]["path"] = json!(checkpoint.path().to_str().unwrap());
        let effective = capyctl_config::effective::resolve_effective(&config, &host).unwrap();
        (effective, checkpoint)
    }

    const QWEN3_ON_GB10: &[QualifiedParking] = &[QualifiedParking {
        engine: "vllm",
        engine_version: "0.32.0",
        model_type: "qwen3",
        gpu: "NVIDIA GB10",
        residency: "deep",
    }];

    // T45 (ADR 0033 §7, amendment A12): the host's own plan compares the
    // checkpoint's `config.json` model type and the product name it
    // published for the GPU. A qualified row runs LMCache; any other
    // combination stays off and names the real model type and GPU.
    #[test]
    fn the_host_plan_matches_a_qualified_row_and_names_any_other() {
        let (qwen3, _checkpoint) = deep_on_gb10("qwen3");
        assert_eq!(
            lmcache_plan_in(&qwen3, false, QWEN3_ON_GB10).state,
            capyctl_config::lmcache::LmcacheState::On
        );
        let off = |effective: &EffectiveDeployment, table: &[QualifiedParking], detail: &str| {
            assert_eq!(
                lmcache_plan_in(effective, false, table).off(),
                Some(&LmcacheOff::with_detail(
                    LmcacheOffReason::ParkUnqualified,
                    detail
                ))
            );
        };
        off(
            &qwen3,
            QUALIFIED_PARKING,
            "not qualified with deep park on vLLM 0.32.0 / qwen3 / NVIDIA GB10",
        );
        let (llama, _checkpoint) = deep_on_gb10("llama");
        off(
            &llama,
            QWEN3_ON_GB10,
            "not qualified with deep park on vLLM 0.32.0 / llama / NVIDIA GB10",
        );
    }
}
