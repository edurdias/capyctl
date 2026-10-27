//! ADR 0035 §9: the per-GPU evidence of a vLLM park or wake on several GPUs
//! of one host.
//!
//! On one GPU a vLLM park settles on the engine's own reply, and the residue
//! its processes hold is measured afterwards (ADR 0014 amendment A13). On N
//! GPUs the reply is one collective answer for N ranks, so it is not evidence
//! that every GPU released (or got back) its memory. After the reply, each
//! chosen GPU's bytes held by the launch's own processes (ADR 0035 §8, one
//! reading per process and GPU) must be at or below that GPU's parked budget
//! for a park, and above it for a wake. A GPU with no fresh reading, or a
//! reading without per-GPU figures, proves nothing: the step is not settled
//! and every charge stays as it was (AGENTS.md: uncertainty retains
//! accounting). A GPU's parked budget is what its domain's parked phase
//! charges the launch: its measured residue, never below the placeholder and
//! never above the Ready charge (`parked_charge::apply`), the rule a group
//! member's park is judged by (ADR 0028 §12).
//!
//! SGLang proves the same per GPU through each rank's saver map (the host's
//! per-rank read), so only vLLM is judged here. A launch on one GPU is
//! unchanged.
use super::*;
use capyctl_config::engine_policy::Engine;
use capyctl_domain::resources::{MemoryObservation, ProcessResident};

/// One chosen GPU of a launch on several GPUs, with the parked budget its
/// park and wake are judged against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GpuBudget {
    /// The GPU's memory domain.
    pub domain: String,
    /// The host-local device id (`gpuN`) the host's readings name it by.
    pub device: String,
    /// The bytes the GPU's parked charge allows the launch to keep there.
    pub parked_bytes: i64,
}

/// ADR 0035 §9: what an armed park or wake of a vLLM launch on several GPUs
/// settles on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceEvidence {
    pub kind: ResidencyKind,
    /// Every chosen GPU, in the order of the parked phase's allocations.
    pub gpus: Vec<GpuBudget>,
    /// The launch's recorded processes; only their bytes count.
    pub identities: Vec<ProcessIdentity>,
}

impl DeviceEvidence {
    /// Whether one host sample proves the step on every GPU: each GPU's
    /// domain read within `[not_before, now]`, and the bytes the launch's
    /// processes hold there at or below its parked budget for a park, above
    /// it for a wake. `Ok` carries when the evidence was observed (its
    /// oldest GPU reading); `Err` names the first GPU that does not prove it.
    ///
    /// A process the sample does not list holds nothing on any GPU (the host
    /// lists every process holding GPU memory). Each GPU must still list one
    /// of the launch's processes (each rank keeps its CUDA context on its GPU
    /// while parked), so a sample that saw none of them on a GPU is no reading
    /// of that GPU. A listed process without per-GPU figures (an older host)
    /// leaves every GPU unread.
    pub fn judge(
        &self,
        observations: &[MemoryObservation],
        residents: &[ProcessResident],
        not_before: i64,
        now: i64,
    ) -> Result<i64, String> {
        let verb = match self.kind {
            ResidencyKind::Park => "park",
            ResidencyKind::Restore => "wake",
        };
        if self.gpus.len() < 2 {
            return Err(format!("the {verb} names no GPUs to judge"));
        }
        let processes: Vec<&ProcessResident> = residents
            .iter()
            .filter(|r| {
                self.identities.iter().any(|i| {
                    i.pid == r.pid && i.boot_id == r.boot_id && i.start_ticks == r.start_ticks
                })
            })
            .collect();
        if processes
            .iter()
            .any(|r| r.per_device.is_empty() && r.device_bytes != 0)
        {
            return Err(format!(
                "the host's reading after the {verb} has no per-GPU figures"
            ));
        }
        let mut observed_at = now;
        for gpu in &self.gpus {
            let Some(read_at) = observations
                .iter()
                .filter(|o| o.domain == gpu.domain && (not_before..=now).contains(&o.sampled_at_ms))
                .map(|o| o.sampled_at_ms)
                .max()
            else {
                return Err(format!(
                    "{} has no fresh reading after the {verb}",
                    gpu.device
                ));
            };
            observed_at = observed_at.min(read_at);
            let listed = processes
                .iter()
                .any(|r| r.per_device.iter().any(|d| d.device == gpu.device));
            if !listed {
                return Err(format!(
                    "{} shows none of the launch's processes after the {verb}",
                    gpu.device
                ));
            }
            let held = processes.iter().fold(0_u64, |sum, r| {
                sum.saturating_add(r.bytes_on(&gpu.device).unwrap_or(0))
            });
            let held = i64::try_from(held).unwrap_or(i64::MAX);
            let proven = match self.kind {
                ResidencyKind::Park => held <= gpu.parked_bytes,
                ResidencyKind::Restore => held > gpu.parked_bytes,
            };
            if !proven {
                return Err(format!(
                    "{} holds {held} bytes of the launch after the {verb}, {} its parked budget of {} bytes",
                    gpu.device,
                    match self.kind {
                        ResidencyKind::Park => "above",
                        ResidencyKind::Restore => "not above",
                    },
                    gpu.parked_bytes
                ));
            }
        }
        Ok(observed_at)
    }
}

/// Whether a launch resolved as `e` parks and wakes on per-GPU evidence: a
/// vLLM launch on several GPUs of its host that parks.
pub(super) fn judged_per_gpu(e: &EffectiveDeployment) -> bool {
    e.local_tensor_parallel() > 1 && e.profile.engine == Engine::Vllm && parks(e)
}

/// The per-GPU evidence a residency step of `kind` on the launch `source`
/// (resolved as `e`, its processes `identities`) settles on; `None` when the
/// launch is not judged per GPU.
pub(super) fn evidence(
    tx: &rusqlite::Connection,
    source: &Plan,
    e: &EffectiveDeployment,
    identities: &[ProcessIdentity],
    kind: ResidencyKind,
) -> Result<Option<DeviceEvidence>, LifecycleError> {
    if !judged_per_gpu(e) {
        return Ok(None);
    }
    Ok(Some(DeviceEvidence {
        kind,
        gpus: budgets(e, &charged_footprints(tx, source, e)?.parked)?,
        identities: identities.to_vec(),
    }))
}

/// Each GPU of `e`'s launch with what its domain's allocation in `parked`
/// (the launch's charged parked phase) allows. A launch whose parked phase
/// does not name exactly its N GPUs, each with a host-local device id, is
/// corrupt.
pub(super) fn budgets(
    e: &EffectiveDeployment,
    parked: &PhaseFootprint,
) -> Result<Vec<GpuBudget>, LifecycleError> {
    let devices = crate::resident_floors::domain_devices_of(
        &capyctl_config::resource_controls::ResourceContext::from_host(&e.host),
    );
    let gpus: Vec<GpuBudget> = parked
        .allocations
        .iter()
        .filter(|a| {
            e.host
                .domains
                .get(&a.domain)
                .is_some_and(|d| d.memory == capyctl_config::effective::DomainMemory::Device)
        })
        .map(|a| {
            Ok(GpuBudget {
                domain: a.domain.clone(),
                device: devices
                    .get(&a.domain)
                    .cloned()
                    .ok_or(LifecycleError::CorruptStoredData)?,
                parked_bytes: a.bytes,
            })
        })
        .collect::<Result<_, LifecycleError>>()?;
    if gpus.len() != e.local_tensor_parallel() as usize {
        return Err(LifecycleError::CorruptStoredData);
    }
    Ok(gpus)
}

/// ADR 0035 §9: one host sample offered as a step's per-GPU evidence, taken
/// at or after `since_ms` (after the engine's reply, or a wake's canary).
pub struct GpuReadings<'a> {
    pub observations: &'a [MemoryObservation],
    pub residents: &'a [ProcessResident],
    pub since_ms: i64,
}

impl crate::Store {
    /// ADR 0035 §9: the per-GPU evidence the armed step `id` settles on, or
    /// `None` when its launch settles on the engine's reply alone (one GPU,
    /// or an engine that proves each GPU otherwise).
    pub fn residency_device_evidence(
        &self,
        s: &CoordinatorSession,
        id: &str,
    ) -> Result<Option<DeviceEvidence>, LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        check_session(&tx, s)?;
        let (p, state) = read(&tx, id)?;
        if state != "armed" || group::is_group(&tx, &p)? {
            return Err(LifecycleError::Conflict);
        }
        let (source, e, identities) = current_residency(&tx, s, &p)?;
        evidence(&tx, &source, &e, &identities, p.kind)
    }

    /// As [`Self::complete_residency`], for a step that settles on per-GPU
    /// evidence (ADR 0035 §9): `readings` must prove it on every GPU of the
    /// launch, read within the host's observation TTL and after the step was
    /// armed, or nothing changes.
    pub fn complete_residency_on_gpus(
        &self,
        s: &CoordinatorSession,
        id: &str,
        observation: &EffectObservation,
        readings: &GpuReadings<'_>,
        now: i64,
    ) -> Result<(), LifecycleError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, s)?;
        complete(&tx, s, id, observation, Some(readings), now)?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "park_devices_tests.rs"]
mod tests;
