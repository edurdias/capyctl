//! ADR 0028 §12 (R12): the saver-map facts of this host's SGLang group
//! members, reported by this host on its own session.
//!
//! A SGLang group parks and wakes through its head alone: the head's agent
//! invokes the collective once, and every rank releases (or resumes) its own
//! memory. Each member's own host then proves its rank's state from the saver
//! map its scheduler enrolled in this host's private observation directory at
//! launch, read with the member's own credential and bound to the member's
//! recorded processes. Never from process sampling.
//!
//! A read hashes the saver library and waits on the scheduler's socket (for
//! seconds at worst), so reads run off the session loop: an availability
//! refresh reports the latest sample and, once it is older than [`REFRESH`],
//! starts the next one in the background, as the process residency sampler
//! does. A member whose map is unreadable, partial or not the approved saver's
//! is not reported. A member whose own Park or Restore is running here (a
//! head's) is left to that step, which reads the same socket.
use super::{
    saver_source::saver_facts, worker_observes, NativeHostExecution, SaverResidency, SaverScope,
};
use capyctl_domain::group::GroupEngine;
use capyctl_protocol::{execution::MemberCommand, pb};
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

/// How often a new sample is started when a refresh asks for one.
const REFRESH: Duration = Duration::from_secs(2);
/// A sample older than this is not reported.
const MAX_AGE: Duration = Duration::from_secs(10);
/// At most this many members are reported per sample.
const MAX_MEMBERS: usize = 64;

/// The latest sample of every reported member's saver map.
#[derive(Default)]
pub(super) struct MemberSavers {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    last: Option<(Instant, Vec<pb::MemberSaver>)>,
    running: bool,
}

impl NativeHostExecution {
    /// The latest sample no older than [`MAX_AGE`]. Never blocks: when the
    /// sample is older than [`REFRESH`] a new one is started in the background
    /// and a later refresh reports it. A host with no saver observation source
    /// reports none.
    pub(super) fn reported_member_savers(&self) -> Vec<pb::MemberSaver> {
        if self.saver.is_none() {
            return Vec::new();
        }
        let Ok(mut state) = self.member_savers.state.lock() else {
            return Vec::new();
        };
        let stale = state
            .last
            .as_ref()
            .is_none_or(|(at, _)| at.elapsed() >= REFRESH);
        if stale && !state.running {
            state.running = true;
            let host = self.clone();
            let spawned = std::thread::Builder::new()
                .name("capyctl-member-savers".into())
                .spawn(move || {
                    let sample = host.sample_member_savers();
                    if let Ok(mut state) = host.member_savers.state.lock() {
                        state.last = Some((Instant::now(), sample));
                        state.running = false;
                    }
                });
            if spawned.is_err() {
                state.running = false;
            }
        }
        match &state.last {
            Some((at, sample)) if at.elapsed() < MAX_AGE => sample.clone(),
            _ => Vec::new(),
        }
    }

    /// One sample now, on this thread: every SGLang group member this host
    /// claims whose map reads whole.
    fn sample_member_savers(&self) -> Vec<pb::MemberSaver> {
        let Some(saver) = self.saver.clone() else {
            return Vec::new();
        };
        let Ok(claimed) = self.journal.claimed_launches("") else {
            return Vec::new();
        };
        claimed
            .iter()
            .filter_map(|claim| self.member_saver(saver.as_ref(), &claim.command))
            .take(MAX_MEMBERS)
            .collect()
    }

    /// The saver map of one claimed launch, when it is a SGLang group member
    /// (head or worker) that parks, and its map is whole and the approved
    /// saver's. Its observed time is when the read began, so a map read
    /// across a collective is never dated after it.
    fn member_saver(
        &self,
        saver: &dyn SaverResidency,
        launch: &MemberCommand,
    ) -> Option<pb::MemberSaver> {
        if launch.action.group_launch()?.engine() != GroupEngine::Sglang {
            return None;
        }
        let plan = launch.action.launch_plan()?;
        let handle = &launch.identity.command_id;
        if matches!(
            self.journal.residency_of(handle).ok()?.as_deref(),
            Some("parking" | "restoring")
        ) {
            return None;
        }
        // The member's own approved configuration, resolved from this host's
        // policy: it parks, and its launch enrolled a saver observation.
        let effective = self.resolve_retained(launch).ok()?;
        if !effective.residency.parks() || !worker_observes(&effective) {
            return None;
        }
        // ADR 0028 §12 (R12): the member's own credential (a worker's is its
        // own bundle's admin key, minted at its launch).
        let keys = self
            .identities
            .load(&self.scope(launch).ok()?, launch.identity.payload_digest)
            .ok()?;
        let served = effective.routes.first()?;
        let frozen = self.sglang_frozen(&effective, plan, served, None).ok()?;
        let members: Vec<_> = self
            .journal
            .inspect_owned(handle)
            .ok()?
            .into_iter()
            .map(|(identity, _)| identity)
            .collect();
        if members.is_empty() {
            return None;
        }
        let observed_at = capyctl_protocol::now_unix_ms();
        let mapped = saver.mapped(&SaverScope {
            binding_id: plan.binding_id.clone(),
            incarnation: plan.incarnation.clone(),
            members: Some(members),
            admin_key: hex::encode(keys.admin),
            executable: frozen.executable().into(),
        });
        // SPEC §9.2: only a whole map from the approved saver is evidence.
        let facts = saver_facts(&mapped, frozen.settings().weight_restore == "resident")?;
        if !facts.real_saver {
            return None;
        }
        Some(pb::MemberSaver {
            owned_handle: handle.clone(),
            mapped_bytes: i64::try_from(mapped.ok()?.mapped_bytes()).ok()?,
            observed_at_unix_ms: observed_at,
        })
    }
}
