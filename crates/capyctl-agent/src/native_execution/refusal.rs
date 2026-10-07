//! SPEC §13 (WE3 limit 1): a command this host's own policy refuses before any
//! effect is answered with a terminal, typed refusal instead of ending the
//! control session.
//!
//! Ending the session made the controller redeliver the same immutable command
//! until its deadline (up to the 900 s Initialize bound), and every redelivery
//! was refused the same way. A refusal is evidence that nothing happened:
//! nothing was journaled, no key was stored, no process was started, and no
//! claim was taken. A refused launch reports `completed` with no claim; a
//! refused Park or Restore reports its launch `unchanged` (SPEC §§9.1, 10).
//! The reason is one closed category (`capyctl_protocol::execution::POLICY_REFUSALS`).
use super::{GpuReading, NativeHostExecution};
use crate::{checkpoint::CheckpointError, journal::JournalError, session::SessionError};
use capyctl_config::effective::DomainMemory;
use capyctl_domain::group::GroupEngine;
use capyctl_protocol::{
    execution::{MemberAction, MemberCommand, SingleLaunchPlan},
    pb,
};

/// Why local policy did not admit a launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchVerdict {
    /// Refused before any effect, with a closed wire category.
    Refused(&'static str),
    /// The host could not observe what admission needs (its memory sample).
    /// That is not a policy decision, so it is never reported as a refusal.
    Uncertain,
}

impl From<LaunchVerdict> for JournalError {
    fn from(verdict: LaunchVerdict) -> Self {
        match verdict {
            LaunchVerdict::Refused(_) => JournalError::Unauthorized,
            LaunchVerdict::Uncertain => JournalError::Uncertain,
        }
    }
}

/// ADR 0014 §7: a checkpoint that no longer measures to the recorded digest is
/// a mismatch; one that could not be measured at all is unverified.
fn checkpoint_refusal(error: CheckpointError) -> &'static str {
    match error {
        CheckpointError::Mismatch => "checkpoint_mismatch",
        _ => "checkpoint_unverified",
    }
}

/// SPEC §7.2, discrete GPU design §4: the memory half of launch admission,
/// from one host memory sample and one GPU sample. Every allocation of the
/// cold footprint is checked against its own declared domain, read from that
/// domain's own source, never substituted: host memory for a `unified` or
/// `distinct` domain, the GPU for a `device` domain. The allocation must not
/// exceed the domain's managed limit, the limit must not exceed what the
/// domain observably holds, and the allocation plus the free reserve must
/// fit what is available now (on a device domain, the part of the reserve
/// that memory held outside CapyCTL does not already take, ADR 0019 §2). The switch planner charges the same domains,
/// so a switch it accepts passes here once its victims are released.
pub fn admit_memory_with(
    effective: &capyctl_config::effective::EffectiveDeployment,
    host: &crate::memory::HostMemorySample,
    gpu: Option<&crate::gpu_memory::GpuSample>,
) -> Result<(), LaunchVerdict> {
    let refused = LaunchVerdict::Refused;
    let allocations = &effective.resources.cold.allocations;
    if allocations.is_empty() {
        return Err(refused("unauthorized"));
    }
    for allocation in allocations {
        let limit = effective
            .host
            .domains
            .get(&allocation.domain)
            .ok_or(refused("unauthorized"))?;
        let (capacity, available, short, reserve) = match limit.memory {
            DomainMemory::Device => {
                // ADR 0019: a device domain is read from the GPU its `gpuN`
                // device names; any other id has no source.
                let index = limit
                    .device
                    .as_deref()
                    .and_then(crate::gpu_memory::device_index)
                    .ok_or(refused("unauthorized"))?;
                // SPEC §7.2: an unobserved device closes admission; it is
                // uncertainty, never a pass and never a policy refusal.
                let memory = gpu
                    .and_then(|sample| sample.devices.iter().find(|d| d.index == index))
                    .and_then(|device| device.memory.as_ref())
                    .ok_or(LaunchVerdict::Uncertain)?;
                // ADR 0019 §2: the reserve absorbs what the card holds outside
                // CapyCTL's accounting. This check has no process attribution,
                // so every byte in use counts as such; the ledger has already
                // kept every charge within the managed limit, and this check
                // then asks the card to physically hold the allocation.
                let reserve = capyctl_domain::resources::absorbing_reserve(
                    limit.free_reserve,
                    memory.total_bytes,
                    memory.free_bytes,
                    0,
                );
                (
                    memory.total_bytes,
                    memory.free_bytes,
                    "insufficient_device_memory",
                    reserve,
                )
            }
            DomainMemory::Unified | DomainMemory::Distinct => (
                host.memory.capacity_bytes,
                host.memory.available_bytes,
                "insufficient_memory",
                limit.free_reserve,
            ),
        };
        if allocation.bytes > limit.managed_limit || limit.managed_limit > capacity {
            return Err(refused("unauthorized"));
        }
        if allocation
            .bytes
            .checked_add(reserve)
            .is_none_or(|required| required > available)
        {
            return Err(refused(short));
        }
    }
    Ok(())
}

/// SPEC §3.1: the leased loopback engine port must be free when a launch is
/// admitted; a port another program listens on is `port_conflict` (the next
/// start leases a free one: the server's lease skips a port it cannot bind).
pub(crate) fn engine_port_free(port: u16) -> Result<(), LaunchVerdict> {
    std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
        .map(drop)
        .map_err(|_| LaunchVerdict::Refused("port_conflict"))
}

/// Whether any allocation of `effective`'s starting footprint lands on a device
/// domain, which is read from the GPU (discrete GPU design §4).
pub(crate) fn charges_device(effective: &capyctl_config::effective::EffectiveDeployment) -> bool {
    effective
        .resources
        .cold
        .allocations
        .iter()
        .any(|allocation| {
            effective
                .host
                .domains
                .get(&allocation.domain)
                .is_some_and(|limit| limit.memory == DomainMemory::Device)
        })
}

impl NativeHostExecution {
    /// ADR 0008 (owner decision 2026-09-23): refuse a launch whose declared
    /// residency depends on a capability the installation's launch-time probe
    /// found missing. Both parking tiers depend on `deep_park` (controller
    /// ruling, discrete GPU design §5): `deep`, and `host_backed`, which parks
    /// with SGLang's memory saver and weights CPU backup or vLLM's sleep mode,
    /// the shapes that probe covers. A `restart_only` deployment is never
    /// probed and serves on a build without them. An unknown report (no probe
    /// helper, a probe that did not answer) refuses nothing; the protected
    /// entry probes again.
    pub(super) fn launch_capability(
        &self,
        effective: &capyctl_config::effective::EffectiveDeployment,
        profile: &str,
        installation: &str,
        installations: &crate::installation::InstallationRegistry,
    ) -> Result<(), &'static str> {
        if !effective.residency.parks() {
            return Ok(());
        }
        // SPEC §§6.2, 9.1: the decision standalone's embedded host makes too.
        if let Some(reason) = effective.deep_wake_refusal() {
            return Err(reason);
        }
        let Some(report) = installations.capabilities(
            profile,
            installation,
            effective.profile.engine,
            std::path::Path::new(&effective.profile.executable),
            &self.runtime_dir,
        ) else {
            return Ok(());
        };
        if report.available("core") == Some(false) {
            return Err("capability_missing:core");
        }
        if report.available("deep_park") == Some(false) {
            return Err("capability_missing:deep_park");
        }
        Ok(())
    }

    /// ADR 0008: the closed reason a Park of this launch is refused for when
    /// its installation lacks what parking (`deep` or `host_backed`) drives,
    /// or `None`.
    pub(super) fn park_capability(
        &self,
        effective: &capyctl_config::effective::EffectiveDeployment,
        profile: &str,
    ) -> Option<&'static str> {
        if let Some(reason) = effective.deep_wake_refusal() {
            return Some(reason);
        }
        let executable = std::path::Path::new(&effective.profile.executable);
        // ADR 0018 §4: a running engine's profile never changes (removal waits
        // for it to stop), so the accepted set's registry describes it.
        let installations = self.profiles.accepted().installations.clone();
        // A Park never refuses on drift: the running engine loaded its code at
        // launch. The measurement only keys the capability cache.
        let installation = installations
            .verify(
                profile,
                effective.profile.engine,
                executable,
                capyctl_config::effective::InstallationDrift::Warn,
            )
            .ok()?;
        let report = installations.capabilities(
            profile,
            &installation,
            effective.profile.engine,
            executable,
            &self.runtime_dir,
        )?;
        if report.available("deep_park") == Some(false) {
            return Some("capability_missing:deep_park");
        }
        // SPEC §9.2: an SGLang release is verified by the enrolled saver
        // observation; a build without the shapes it binds cannot prove one.
        (effective.profile.engine == capyctl_config::engine_policy::Engine::Sglang
            && report.available("observation") == Some(false))
        .then_some("capability_missing:observation")
    }

    /// Local admission of one reserved launch, before anything is recorded.
    /// `authorize` runs this under the journal's lock: a command whose slow
    /// admission passed moments ago outside the lock (`pre_admit`) is rechecked
    /// cheaply (resolution, runtime integrity, memory); any other is admitted
    /// in full (SPEC §13.2).
    pub(super) fn admit_launch(
        &self,
        command: &MemberCommand,
        plan: &SingleLaunchPlan,
        reading: GpuReading,
    ) -> Result<(), LaunchVerdict> {
        // Found live 2026-10-03: another program listening on the leased
        // engine port answered the new engine's readiness probe, which then
        // treated it as an engine it could not prove its own and stopped the
        // launch. A port in use is refused before anything starts. ADR 0028
        // §8: a group worker other than SGLang's leases none.
        if let Some(port) = super::leased_port(command) {
            engine_port_free(port)?;
        }
        if self.pre_admitted(command) {
            let refused = LaunchVerdict::Refused;
            let effective = self.resolve(command).map_err(|_| refused("unauthorized"))?;
            crate::runtime_integrity::verify(
                &self.runtime_dir,
                crate::runtime_integrity::launch_required_files(&effective),
            )
            .map_err(|_| refused("runtime_integrity"))?;
            return self.admit_memory(&effective, command, reading);
        }
        self.admit_launch_full(command, plan, reading)
    }

    /// The whole admission of one reserved launch; provisioning and
    /// pre-admission run exactly this, outside the journal's locks.
    pub(super) fn admit_launch_full(
        &self,
        command: &MemberCommand,
        plan: &SingleLaunchPlan,
        reading: GpuReading,
    ) -> Result<(), LaunchVerdict> {
        let refused = LaunchVerdict::Refused;
        let effective = self.resolve(command).map_err(|_| refused("unauthorized"))?;
        // SPEC §9.1, §13.3 / T21 T37: the engine imports capyctl's modules from the
        // runtime directory; one another account could rewrite is refused.
        // ADR 0008: the capability probes are required where the launch's
        // entry imports them (every SGLang launch, vLLM with sleep mode).
        crate::runtime_integrity::verify(
            &self.runtime_dir,
            crate::runtime_integrity::launch_required_files(&effective),
        )
        .map_err(|_| refused("runtime_integrity"))?;
        // ADR 0008 (owner decision 2026-09-23): the installation is measured
        // again. Drift from its registered fingerprint is flagged in status,
        // and refused only when its host policy says `installation_drift:
        // refuse`. Then the internals this launch depends on are probed.
        // ADR 0018 §3: the registry of the document the plan was approved under.
        let installations = self.installations_for(plan);
        let installation = installations
            .verify(
                &plan.profile_name,
                effective.profile.engine,
                std::path::Path::new(&effective.profile.executable),
                effective.profile.security.installation_drift,
            )
            .map_err(refused)?;
        self.launch_capability(
            &effective,
            &plan.profile_name,
            &installation,
            &installations,
        )
        .map_err(refused)?;
        // ADR 0014 §7: refuse before any effect when the checkpoint is not the
        // recorded one (stat check and small-file rehash, full rehash on first
        // placement or any change).
        self.verify_checkpoint(&effective, plan)
            .map_err(|error| refused(checkpoint_refusal(error)))?;
        self.admit_memory(&effective, command, reading)
    }

    /// The memory half of admission: every allocation of the launch's starting
    /// footprint fits its own domain with that domain's free reserve kept
    /// ([`admit_memory_with`]). Host memory is read as before. The GPU is read
    /// only when some allocation lands on a device domain: sampled afresh
    /// outside the journal's locks, and under them the sample the command took
    /// just before the lock ([`GpuReading`]); never a collector run under a lock.
    fn admit_memory(
        &self,
        effective: &capyctl_config::effective::EffectiveDeployment,
        command: &MemberCommand,
        reading: GpuReading,
    ) -> Result<(), LaunchVerdict> {
        let host = crate::memory::read_host_memory().map_err(|_| LaunchVerdict::Uncertain)?;
        let gpu = if charges_device(effective) {
            self.gpu_for(command, reading)
        } else {
            None
        };
        admit_memory_with(effective, &host, gpu.as_ref())
    }

    /// SPEC §13: the terminal answer to a command the journal refused on local
    /// policy before recording it. Only a launch, Park or Restore has one; any
    /// other refusal still fails the effect. `reason` is a closed code already
    /// known (ADR 0028 §8: a group Launch's host checks); without one the
    /// policy category is recomputed.
    pub(super) async fn refused(
        &self,
        command: &MemberCommand,
        reason: Option<String>,
    ) -> Result<pb::MemberExecutionResult, SessionError> {
        let host = self.clone();
        let refused = command.clone();
        let reason = match reason {
            Some(reason) => reason,
            None => tokio::task::spawn_blocking(move || host.refusal_reason(&refused))
                .await
                .map_err(|_| SessionError)?
                .ok_or(SessionError)?
                .to_owned(),
        };
        let now = capyctl_protocol::now_unix_ms();
        let mut result = match &command.action {
            // ADR 0028 §8: a refused group Launch has the same shape, and
            // names no binding.
            MemberAction::LaunchSingle(_) | MemberAction::Launch { .. } => {
                pb::MemberExecutionResult {
                    owned_handle: command.identity.command_id.clone(),
                    ..Default::default()
                }
            }
            MemberAction::Park { owned_handle } | MemberAction::Restore { owned_handle, .. } => {
                // The launch it names, as this host currently holds it, when
                // it is this deployment's own; otherwise nothing is reported.
                let owned = self
                    .journal
                    .retained_command(owned_handle)
                    .ok()
                    .filter(|owner| owner.identity.deployment_id == command.identity.deployment_id)
                    .and_then(|_| self.journal.execution_result(owned_handle, now).ok());
                let mut result = owned.unwrap_or_default();
                result.state = String::new();
                result.model_usable = false;
                result.owned_handle = owned_handle.clone();
                result.residency = Some(pb::ResidencyEvidence {
                    state: "unchanged".into(),
                    mem_available_before_bytes: -1,
                    mem_available_after_bytes: -1,
                    milestones: Vec::new(),
                });
                result
            }
            _ => return Err(SessionError),
        };
        result.identity = command.to_wire().identity;
        result.state = "completed".into();
        result.observed_at_unix_ms = now;
        result.checkpoint = None;
        result.refused = reason;
        capyctl_protocol::execution::validate_result(command, &result).map_err(|_| SessionError)?;
        Ok(result)
    }

    /// The closed category for a refusal the journal already made. The journal
    /// refused on this same policy, so a verdict that now admits the command
    /// (the host changed between the two reads) is reported as `unauthorized`.
    fn refusal_reason(&self, command: &MemberCommand) -> Option<&'static str> {
        match &command.action {
            // SPEC §§3.1, 7.3: the journal refused alone or beside the
            // launches it still claims; the reason is recomputed the same way.
            MemberAction::LaunchSingle(plan) | MemberAction::Launch { member: plan, .. } => {
                Some(match self.admit_launch_here(command, plan) {
                    Err(LaunchVerdict::Refused(reason)) => reason,
                    _ => "unauthorized",
                })
            }
            MemberAction::Park { owned_handle } | MemberAction::Restore { owned_handle, .. } => {
                // ADR 0012: only a parking tier (`deep`, or `host_backed`
                // where it resolved, discrete GPU design §5) parks. A
                // `restart_only` one (any engine, SPEC §6.2) is refused by its
                // declared tier. ADR 0028 §2, §12 (OD3): so is a TensorFold
                // group's head, whatever it declared: TensorFold groups are
                // restart-only (they park by a group stop).
                let owner = self
                    .journal
                    .retained_command(owned_handle)
                    .ok()
                    .filter(crate::journal::parks_in_place);
                let tensorfold_group = owner.as_ref().is_some_and(|owner| {
                    owner
                        .action
                        .group_launch()
                        .is_some_and(|plan| plan.engine() == GroupEngine::Tensorfold)
                });
                let tier = tensorfold_group
                    || owner
                        .as_ref()
                        .and_then(|owner| self.resolve_retained(owner).ok())
                        .is_some_and(|effective| !effective.residency.parks());
                // SPEC §§3.1, 7.3, 9.1: a wake that does not fit beside the
                // other claims is refused by the rule it breaks.
                let wake = || match &command.action {
                    MemberAction::Restore { .. } => {
                        let owner = self.journal.retained_command(owned_handle).ok()?;
                        let others = self.journal.claimed_launches(owned_handle).ok()?;
                        match self.admit_wake_beside_claims(&owner, &others) {
                            Err(LaunchVerdict::Refused(reason)) => Some(reason),
                            _ => None,
                        }
                    }
                    _ => None,
                };
                // ADR 0008: a Park the installation's probe cannot support.
                let capability = || {
                    let owner = owner.as_ref()?;
                    let plan = owner.action.launch_plan()?;
                    let effective = self.resolve_retained(owner).ok()?;
                    self.park_capability(&effective, &plan.profile_name)
                };
                Some(if tier {
                    "residency_tier"
                } else if let Some(reason) = capability() {
                    reason
                } else {
                    wake().unwrap_or("unauthorized")
                })
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{gpu_memory::GpuSample, memory::HostMemorySample};
    use capyctl_config::effective::{Allocation, DomainPolicy, EffectiveDeployment};

    const GIB: i64 = 1 << 30;

    fn base() -> EffectiveDeployment {
        let source: serde_json::Value = serde_json::from_str(include_str!(
            "../../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
        ))
        .unwrap();
        capyctl_config::effective::resolve_effective(
            &source["input"]["deployment"],
            &source["input"]["host"],
        )
        .unwrap()
    }

    /// An SGLang deployment declaring a modelopt quantization, with or without
    /// speculative decoding (the config crate's own fixture).
    fn sglang_modelopt(speculative: bool) -> EffectiveDeployment {
        let all: serde_json::Value = serde_json::from_str(include_str!(
            "../../../capyctl-config/tests/fixtures/f2-deployment.json"
        ))
        .unwrap();
        let (mut deployment, mut host) = (all["deployment"].clone(), all["host"].clone());
        let object = deployment.as_object_mut().unwrap();
        object.remove("resources");
        object.remove("residency");
        let profile = &mut host["runtime_profiles"]["local"];
        profile["engine"] = "sglang".into();
        profile["args"] = serde_json::json!([]);
        profile["security"]["admin_credential_ref"] = "secret://engine-admin".into();
        profile["security"]["approved_options"] =
            serde_json::json!(["--speculative-draft-model-path"]);
        profile["security"]["approved_paths"] = serde_json::json!(["/srv/drafters"]);
        deployment["engine_config"]["memory"] =
            serde_json::json!({"request": "40GiB", "kv_cache": "8GiB"});
        deployment["engine_config"]["quantization"] = "modelopt_fp4".into();
        if speculative {
            deployment["engine_config"]["accept_extra_args"] = true.into();
            deployment["engine_config"]["extra_args"] = serde_json::json!([
                "--speculative-draft-model-path",
                "/srv/drafters/d",
                "--speculative-algorithm",
                "DFLASH"
            ]);
        }
        capyctl_config::effective::resolve_effective_with_checkpoint(
            &deployment,
            &host,
            capyctl_config::effective::CheckpointFacts {
                weights_bytes: Some(GIB),
                ..Default::default()
            },
        )
        .unwrap()
    }

    /// ADR 0014 amendment A17: a park that keeps the weights resident reloads
    /// nothing from disk, so the modelopt reload rule does not refuse it.
    // T21
    #[test]
    fn a_resident_weights_park_is_not_refused_for_a_disk_reload() {
        let deep = sglang_modelopt(false);
        assert_eq!(deep.residency, capyctl_config::effective::Residency::Deep);
        assert_eq!(
            deep.deep_wake_refusal(),
            Some("capability_missing:deep_park")
        );
        let resident = sglang_modelopt(true);
        assert_eq!(
            resident.residency,
            capyctl_config::effective::Residency::Deep
        );
        assert_eq!(resident.deep_wake_refusal(), None);
    }

    fn domain(
        managed: i64,
        reserve: i64,
        memory: DomainMemory,
        device: Option<&str>,
    ) -> DomainPolicy {
        DomainPolicy {
            managed_limit: managed,
            free_reserve: reserve,
            host_kv_limit: None,
            parked_limit: None,
            memory,
            device: device.map(str::to_owned),
        }
    }

    fn allocation(domain: &str, bytes: i64) -> Allocation {
        Allocation {
            domain: domain.into(),
            bytes,
            host_kv_bytes: 0,
        }
    }

    /// A 16 GiB card (`gpu0`: managed 15 GiB, reserve 1.25 GiB) beside a
    /// system domain (managed 30 GiB, reserve 12 GiB), cold on both.
    fn discrete_effective(device: i64, system: i64) -> EffectiveDeployment {
        let mut effective = base();
        effective.host.domains = [
            (
                "system".to_string(),
                domain(30 * GIB, 12 * GIB, DomainMemory::Distinct, None),
            ),
            (
                "gpu0".to_string(),
                domain(15 * GIB, 1280 << 20, DomainMemory::Device, Some("gpu0")),
            ),
        ]
        .into();
        effective.resources.cold.allocations =
            vec![allocation("gpu0", device), allocation("system", system)];
        effective
    }

    /// One `unified` allocation, managed 96 GiB, reserve 12 GiB.
    fn unified_effective(bytes: i64) -> EffectiveDeployment {
        let mut effective = base();
        effective.host.domains = [(
            "unified".to_string(),
            domain(96 * GIB, 12 * GIB, DomainMemory::Unified, None),
        )]
        .into();
        effective.resources.cold.allocations = vec![allocation("unified", bytes)];
        effective
    }

    fn ram(capacity: i64, available: i64) -> HostMemorySample {
        crate::memory::parse_meminfo(
            &format!(
                "MemTotal: {} kB\nMemAvailable: {} kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n",
                capacity >> 10,
                available >> 10
            ),
            1,
        )
        .unwrap()
    }

    fn gpu(total_mib: i64, used_mib: i64) -> GpuSample {
        crate::gpu_memory::parse_query_gpu(
            &format!(
                "0, GPU-11111111-2222-3333-4444-555555555555, 00000000:01:00.0, RTX, {total_mib}, {used_mib}, {}\n",
                total_mib - used_mib
            ),
            1,
        )
        .unwrap()
    }

    // T26: each cold allocation is checked against its own domain.
    #[test]
    fn the_launch_check_reads_the_device() {
        let effective = discrete_effective(10 * GIB, 4 * GIB);
        let host = ram(61 * GIB, 50 * GIB);
        assert!(admit_memory_with(&effective, &host, Some(&gpu(16376, 2000))).is_ok());
        // 10 GiB + 1.25 GiB reserve > 5 GiB free on the card
        assert_eq!(
            admit_memory_with(&effective, &host, Some(&gpu(16376, 11_256))),
            Err(LaunchVerdict::Refused("insufficient_device_memory"))
        );
        assert_eq!(
            admit_memory_with(&effective, &host, None),
            Err(LaunchVerdict::Uncertain)
        );
        assert_eq!(
            admit_memory_with(&effective, &ram(61 * GIB, 10 * GIB), Some(&gpu(16376, 0))),
            Err(LaunchVerdict::Refused("insufficient_memory"))
        );
    }

    // T26 (ADR 0019 §2, found live on a 16 GB laptop GPU, 2026-10-03): the
    // device reserve absorbs what the card holds outside CapyCTL (here 348 MiB
    // the driver keeps and 112 MiB a display server uses), so a cold charge up
    // to the managed limit passes on an idle card. Beyond the reserve the card
    // must still hold the charge.
    #[test]
    fn the_device_reserve_absorbs_memory_held_outside_capyctl() {
        const MIB: i64 = 1 << 20;
        let sample = |used: i64, free: i64| {
            crate::gpu_memory::parse_query_gpu(
                &format!(
                    "0, GPU-11111111-2222-3333-4444-555555555555, 00000000:01:00.0, RTX, 16376, {used}, {free}\n"
                ),
                1,
            )
            .unwrap()
        };
        let mut effective = discrete_effective(14828 * MIB, 4 * GIB);
        effective.host.domains.insert(
            "gpu0".into(),
            domain(15066 * MIB, 1310 * MIB, DomainMemory::Device, Some("gpu0")),
        );
        let host = ram(61 * GIB, 50 * GIB);
        assert!(admit_memory_with(&effective, &host, Some(&sample(112, 15916))).is_ok());
        // 2 GiB more in use elsewhere: 13868 MiB free cannot hold 14828 MiB.
        assert_eq!(
            admit_memory_with(&effective, &host, Some(&sample(2160, 13868))),
            Err(LaunchVerdict::Refused("insufficient_device_memory"))
        );
    }

    // T37 (SPEC §3.1, found live 2026-10-03): a leased engine port another
    // program listens on refuses the launch `port_conflict` before anything
    // starts; a free one passes.
    #[test]
    fn a_leased_engine_port_in_use_is_a_port_conflict() {
        // A released port can be taken by anything running in parallel, so
        // the free case is checked first, on a port this test then holds
        // itself to make the conflict.
        let (port, held) = (0..100)
            .find_map(|_| {
                let port = std::net::TcpListener::bind("127.0.0.1:0")
                    .ok()?
                    .local_addr()
                    .ok()?
                    .port();
                engine_port_free(port).ok()?;
                let held = std::net::TcpListener::bind(("127.0.0.1", port)).ok()?;
                Some((port, held))
            })
            .expect("a loopback port that is free, then held");
        assert_eq!(
            engine_port_free(port),
            Err(LaunchVerdict::Refused("port_conflict"))
        );
        drop(held);
    }

    // T26: the unified single-pool behaviour is unchanged.
    #[test]
    fn the_unified_launch_check_is_unchanged() {
        let effective = unified_effective(48 * GIB);
        assert!(admit_memory_with(&effective, &ram(128 * GIB, 70 * GIB), None).is_ok());
        assert_eq!(
            admit_memory_with(&effective, &ram(128 * GIB, 50 * GIB), None),
            Err(LaunchVerdict::Refused("insufficient_memory"))
        );
    }

    // T26 T21: an allocation in an undeclared domain, over its managed limit,
    // or a limit above what the domain observably holds, is not a memory
    // shortfall: it is refused `unauthorized`. A device domain naming no
    // `gpuN` device has no source and is refused the same way.
    #[test]
    fn a_launch_outside_its_declared_domains_is_unauthorized() {
        let host = ram(61 * GIB, 50 * GIB);
        let card = gpu(16376, 0);
        let mut undeclared = discrete_effective(GIB, GIB);
        undeclared
            .resources
            .cold
            .allocations
            .push(allocation("gpu1", GIB));
        assert_eq!(
            admit_memory_with(&undeclared, &host, Some(&card)),
            Err(LaunchVerdict::Refused("unauthorized"))
        );
        assert_eq!(
            admit_memory_with(&discrete_effective(16 * GIB, GIB), &host, Some(&card)),
            Err(LaunchVerdict::Refused("unauthorized"))
        );
        let mut oversized = discrete_effective(GIB, GIB);
        oversized
            .host
            .domains
            .get_mut("gpu0")
            .unwrap()
            .managed_limit = 17 * GIB;
        assert_eq!(
            admit_memory_with(&oversized, &host, Some(&card)),
            Err(LaunchVerdict::Refused("unauthorized"))
        );
        let mut unnamed = discrete_effective(GIB, GIB);
        unnamed.host.domains.get_mut("gpu0").unwrap().device = Some("left".into());
        assert_eq!(
            admit_memory_with(&unnamed, &host, Some(&card)),
            Err(LaunchVerdict::Refused("unauthorized"))
        );
        let mut empty = discrete_effective(GIB, GIB);
        empty.resources.cold.allocations.clear();
        assert_eq!(
            admit_memory_with(&empty, &host, Some(&card)),
            Err(LaunchVerdict::Refused("unauthorized"))
        );
        // A sample that does not report the domain's device is unobserved.
        let other = crate::gpu_memory::parse_query_gpu(
            "1, GPU-11111111-2222-3333-4444-555555555555, 00000000:02:00.0, RTX, 16376, 0, 16376\n",
            1,
        )
        .unwrap();
        assert_eq!(
            admit_memory_with(&discrete_effective(GIB, GIB), &host, Some(&other)),
            Err(LaunchVerdict::Uncertain)
        );
    }
}
