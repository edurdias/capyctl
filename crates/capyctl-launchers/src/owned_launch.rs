//! The one implementation of the builder's process tools.
//!
//! A native engine is launched behind the identity gate, so nothing runs before
//! the controller can name it again after a restart; it is enumerated from the
//! kernel rather than from what the launcher remembers; and it is terminated only
//! when the process about to be signalled is provably the recorded one.
//! SPEC §13.2: never kill by name, never adopt whatever occupies a port.
use std::sync::Arc;
use std::time::{Duration, Instant};

use capyctl_adapters::protected::ProtectedLaunchDescriptors;
use capyctl_adapters::traits::{OwnedProcessLaunch, RenderedCommand, RuntimeError};
use capyctl_domain::completion::{Presence, ProcessIdentity};

use crate::durable::{DurableSpawn, DurableSpawnError, DurableSpawnOutcome, LaunchAssociation};
use crate::group_observation::observe_process_group_or_empty;
use crate::process_absence::{presence, verify_gone, GoneProof};

/// How long to wait after SIGKILL before the outcome is reported as uncertain.
const KILL_PROOF_WINDOW: Duration = Duration::from_secs(5);
/// A `/proc` scan races every process on the host that happens to exit while it
/// runs, and fails closed when it does. That transient is retried a bounded
/// number of times so a busy host is not reported as uncertain ownership; a
/// restricted or hidden `/proc` still fails after the last attempt.
const OBSERVATION_ATTEMPTS: u32 = 8;

/// The association is shared across threads because the builder's process tools
/// are, so it carries the `Send + Sync` bounds the trait itself does not impose on
/// its other, single-threaded callers.
pub struct DurableProcessLaunch {
    spawn: DurableSpawn,
    association: Arc<dyn LaunchAssociation + Send + Sync>,
}

impl DurableProcessLaunch {
    pub fn new(association: Arc<dyn LaunchAssociation + Send + Sync>) -> Self {
        Self {
            spawn: DurableSpawn::new(),
            association,
        }
    }
}

fn uncertain(reason: impl Into<String>) -> RuntimeError {
    RuntimeError::Uncertain(reason.into())
}

/// The launcher's outcome is release evidence, and it maps the same way whether
/// descriptors were inherited or not.
fn released(
    outcome: Result<DurableSpawnOutcome, DurableSpawnError>,
) -> Result<ProcessIdentity, RuntimeError> {
    match outcome {
        Ok(DurableSpawnOutcome::Uncertain {
            api_identity: Some(identity),
            initialization_acknowledged: true,
            ..
        }) => Ok(identity),
        // The child was disposed of by the launcher before this returned, so
        // the failure leaves nothing running and nothing recorded.
        Ok(DurableSpawnOutcome::Uncertain { reason, .. }) => {
            Err(uncertain(format!("launch not released: {reason}")))
        }
        Err(error) => Err(uncertain(format!("spawn failed: {error}"))),
    }
}

impl OwnedProcessLaunch for DurableProcessLaunch {
    fn spawn_durable(
        &self,
        incarnation: &str,
        cmd: &RenderedCommand,
    ) -> Result<ProcessIdentity, RuntimeError> {
        released(
            self.spawn
                .spawn_persisted(incarnation, cmd, self.association.as_ref()),
        )
    }

    fn spawn_durable_protected(
        &self,
        incarnation: &str,
        cmd: &RenderedCommand,
        descriptors: &ProtectedLaunchDescriptors,
    ) -> Result<ProcessIdentity, RuntimeError> {
        released(self.spawn.spawn_persisted_with_descriptors(
            incarnation,
            cmd,
            Some(descriptors),
            self.association.as_ref(),
        ))
    }

    fn present(&self, identity: &ProcessIdentity) -> Presence {
        presence(identity)
    }

    fn building(&self, api: &ProcessIdentity) -> bool {
        crate::group_observation::group_building(api)
    }

    fn observe_group(&self, api: &ProcessIdentity) -> Result<Vec<ProcessIdentity>, RuntimeError> {
        let mut observed = observe_process_group_or_empty(api);
        for _ in 1..OBSERVATION_ATTEMPTS {
            use crate::group_observation::GroupObservationError::{Changed, Visibility};
            if !matches!(observed, Err(Visibility | Changed)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
            observed = observe_process_group_or_empty(api);
        }
        let facts = observed.map_err(|error| uncertain(error.to_string()))?;
        // SPEC §13.2: a reused leader is neither ours nor proof of an empty
        // group. Dropping it from the API/worker split would manufacture cleanup.
        if facts
            .iter()
            .any(|fact| fact.pid == api.pid && fact.start_ticks != api.start_ticks)
        {
            return Err(uncertain("process group leader identity changed"));
        }
        let mut members: Vec<ProcessIdentity> = Vec::new();
        let mut others: Vec<_> = facts.iter().filter(|fact| fact.pid != api.pid).collect();
        // Start order, not pid order: pids wrap, start ticks within one boot do not.
        others.sort_by_key(|fact| (fact.start_ticks, fact.pid));
        if facts
            .iter()
            .any(|fact| fact.pid == api.pid && fact.start_ticks == api.start_ticks)
        {
            members.push(api.clone());
        }
        // ADR 0027: the processes the API process started itself are the
        // engine's workers. Anything else in the group was started by one of
        // them (a compile worker pool) or outlived its parent: a helper,
        // recorded for cleanup, whose exit is not the engine's. Each kind is
        // numbered on its own, so a helper exiting renames no worker.
        let (workers, helpers): (Vec<_>, Vec<_>) = others
            .into_iter()
            .partition(|fact| fact.parent_pid == api.pid);
        let named = |prefix: &str, list: Vec<&crate::group_observation::GroupProcessFact>| {
            list.into_iter()
                .enumerate()
                .map(|(index, fact)| ProcessIdentity {
                    role: format!("{prefix}{index}"),
                    pid: fact.pid,
                    boot_id: fact.boot_id.clone(),
                    start_ticks: fact.start_ticks,
                })
                .collect::<Vec<_>>()
        };
        members.extend(named("worker-", workers));
        members.extend(named(
            capyctl_domain::completion::HELPER_ROLE_PREFIX,
            helpers,
        ));
        Ok(members)
    }

    fn terminate_owned(
        &self,
        identities: &[ProcessIdentity],
        grace: Duration,
    ) -> Result<(), RuntimeError> {
        // Presence that cannot be established is retention, never absence, so it is
        // settled before anything is signalled or released.
        for identity in identities {
            if presence(identity) == Presence::Unknown {
                return Err(uncertain(format!(
                    "presence of pid {} could not be established",
                    identity.pid
                )));
            }
        }
        let Some(api) = identities.iter().find(|identity| identity.role == "api") else {
            // With no leader recorded there is no group to signal; only proof that
            // the recorded set is already gone can end this without one.
            return match verify_gone(identities) {
                GoneProof::AllGone => Ok(()),
                _ => Err(uncertain(
                    "no API identity recorded and the set is not proven gone",
                )),
            };
        };
        // SPEC §13.2: signal only a group whose leader is provably ours. `presence`
        // compares boot id and start ticks, so a reused pid reads as Gone and is
        // never signalled.
        if presence(api) == Presence::Alive {
            signal_group(api.pid, nix::sys::signal::Signal::SIGTERM)?;
            if self.settled(identities, api, Instant::now() + grace, 200)? {
                return Ok(());
            }
            if presence(api) == Presence::Alive {
                signal_group(api.pid, nix::sys::signal::Signal::SIGKILL)?;
            }
            // Spec §5 escalates per identity, not only per group. A leader that
            // died during grace can no longer be signalled through, so a worker
            // still holding device memory would otherwise never receive the
            // group's SIGKILL. Everything recorded has had SIGTERM and the whole
            // grace window by now, so the second signal follows immediately
            // rather than opening another grace window: a stop stays within
            // grace plus the proof window, which is what the coordinator
            // validates its protocol timeout against.
            signal_recorded(identities, nix::sys::signal::Signal::SIGTERM)?;
            signal_recorded(identities, nix::sys::signal::Signal::SIGKILL)?;
            if self.settled(identities, api, Instant::now() + KILL_PROOF_WINDOW, 100)? {
                return Ok(());
            }
        } else {
            // The head crashed and left its workers behind. There is no leader to
            // signal the group through, but each recorded identity carries a boot
            // id and start ticks that `presence` has just matched, so naming them
            // one by one kills exactly the launch's own processes and nothing
            // else. Refusing here instead would pause an operator over a state
            // capyctl can prove and end.
            signal_recorded(identities, nix::sys::signal::Signal::SIGTERM)?;
            if self.settled(identities, api, Instant::now() + grace, 200)? {
                return Ok(());
            }
            signal_recorded(identities, nix::sys::signal::Signal::SIGKILL)?;
            if self.settled(identities, api, Instant::now() + KILL_PROOF_WINDOW, 100)? {
                return Ok(());
            }
        }
        match (verify_gone(identities), self.observe_group(api)?.is_empty()) {
            (GoneProof::AllGone, true) => Ok(()),
            (GoneProof::AllGone, false) => Err(uncertain(
                "recorded processes gone but the group still has members",
            )),
            (GoneProof::SomeAlive, _) => {
                Err(uncertain("a recorded process is still alive after SIGKILL"))
            }
            (GoneProof::Indeterminate, _) => {
                Err(uncertain("process absence could not be established"))
            }
        }
    }
}

impl DurableProcessLaunch {
    /// Poll until every recorded process is proven gone and the group itself is
    /// empty, or the deadline passes. Both halves are required: a recorded process
    /// may exit while a worker it started keeps running in the same group.
    /// An observation that fails here is "not settled yet", not a verdict. A
    /// hidden `/proc` entry on a busy host, or unrelated process churn, would
    /// otherwise end the stop early and report uncertainty for a state the next
    /// poll would have proven. Only the final match after the proof window turns
    /// an observation failure into Uncertain.
    fn settled(
        &self,
        identities: &[ProcessIdentity],
        api: &ProcessIdentity,
        deadline: Instant,
        poll_millis: u64,
    ) -> Result<bool, RuntimeError> {
        while Instant::now() < deadline {
            let gone = verify_gone(identities) == GoneProof::AllGone;
            if gone && self.observe_group(api).is_ok_and(|group| group.is_empty()) {
                return Ok(true);
            }
            std::thread::sleep(Duration::from_millis(poll_millis));
        }
        Ok(false)
    }
}

/// Signal each recorded process that is alive at this moment, by pid.
///
/// SPEC §13.2: `presence` has just matched boot id and start ticks, so a reused
/// pid reads as Gone and is never signalled. A process that exits between the
/// check and the signal is not an error; the proof that the launch is over is the
/// absence check afterwards, never the signal's return value.
fn signal_recorded(
    identities: &[ProcessIdentity],
    signal: nix::sys::signal::Signal,
) -> Result<(), RuntimeError> {
    for identity in identities {
        if presence(identity) != Presence::Alive {
            continue;
        }
        match nix::sys::signal::kill(nix::unistd::Pid::from_raw(identity.pid as i32), signal) {
            Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
            Err(error) => {
                return Err(uncertain(format!(
                    "signal to pid {} failed: {error}",
                    identity.pid
                )));
            }
        }
    }
    Ok(())
}

/// Signal the whole group the leader started, the way `ExecLauncher` does: the
/// engine's own children belong to the launch and must not outlive it. A group
/// that has already gone is not an error.
fn signal_group(leader_pid: u32, signal: nix::sys::signal::Signal) -> Result<(), RuntimeError> {
    match nix::sys::signal::killpg(nix::unistd::Pid::from_raw(leader_pid as i32), signal) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
        Err(error) => Err(uncertain(format!("signal failed: {error}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Accept;
    impl LaunchAssociation for Accept {
        fn persist_api_identity(
            &self,
            _: &ProcessIdentity,
        ) -> Result<(), crate::durable::AssociationError> {
            Ok(())
        }
    }

    fn sleeper(seconds: u32) -> RenderedCommand {
        RenderedCommand {
            // A leader that spawns one child in the same group, like an engine with a worker.
            argv: vec![
                "sh".into(),
                "-c".into(),
                format!("sleep {seconds} & sleep {seconds}"),
            ],
            env: Default::default(),
        }
    }

    /// The group once the leader has started its worker, which a slow runner
    /// does later.
    fn until_group(tool: &DurableProcessLaunch, api: &ProcessIdentity) -> Vec<ProcessIdentity> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let members = tool.observe_group(api).unwrap();
            if members.len() >= 2 || Instant::now() >= deadline {
                return members;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The group is enumerated with the API process first and workers in start
    /// order, and terminating it leaves nothing behind.
    // T12: owned descendants are handled, not just the process that was launched.
    #[test]
    fn a_group_is_observed_and_terminated_completely() {
        let tool = DurableProcessLaunch::new(Arc::new(Accept));
        let api = tool.spawn_durable("grp", &sleeper(60)).unwrap();
        let members = until_group(&tool, &api);
        assert_eq!(members[0].role, "api");
        assert!(members.len() >= 2, "{members:?}");
        assert_eq!(members[1].role, "worker-0");
        tool.terminate_owned(&members, Duration::from_secs(2))
            .unwrap();
        assert_eq!(tool.present(&api), Presence::Gone);
        assert!(tool.observe_group(&api).unwrap().is_empty());
    }

    /// Spec §5: escalation is per recorded identity, not only per group. The head
    /// crashing with a worker still holding device memory used to end in an
    /// operator pause with the device held, because a group can only be signalled
    /// through a leader that is still alive. Each survivor carries a verified boot
    /// id and start ticks, so it is nameable and killable on its own.
    // T31
    #[test]
    fn a_worker_that_outlives_its_leader_is_still_terminated() {
        let tool = DurableProcessLaunch::new(Arc::new(Accept));
        let api = tool.spawn_durable("outlives", &sleeper(60)).unwrap();
        let members = until_group(&tool, &api);
        assert!(members.len() >= 2, "{members:?}");
        // End the leader alone. Its workers keep running in the group it started.
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(api.pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while tool.present(&api) != Presence::Gone && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(tool.present(&api), Presence::Gone, "the leader ended");
        assert!(
            !tool.observe_group(&api).unwrap().is_empty(),
            "a worker outlived the leader"
        );

        tool.terminate_owned(&members, Duration::from_millis(200))
            .unwrap();
        assert!(tool.observe_group(&api).unwrap().is_empty());
    }

    /// A leader with one child, which starts a grandchild of its own: an engine
    /// whose scheduler started a compile worker pool.
    fn with_helper(seconds: u32) -> RenderedCommand {
        RenderedCommand {
            argv: vec![
                "sh".into(),
                "-c".into(),
                format!("sh -c 'sleep {seconds} & wait; sleep {seconds}' & sleep {seconds}; wait"),
            ],
            env: Default::default(),
        }
    }

    /// The group once it holds at least `count` processes.
    fn until_members(
        tool: &DurableProcessLaunch,
        api: &ProcessIdentity,
        count: usize,
    ) -> Vec<ProcessIdentity> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let members = tool.observe_group(api).unwrap();
            if members.len() >= count || Instant::now() >= deadline {
                return members;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// ADR 0027: the processes the leader started itself are the engine's
    /// workers; a process one of them started is a helper. A helper that exits
    /// leaves the workers' names unchanged, and terminating the recorded group
    /// still ends every helper.
    #[test]
    fn a_grandchild_is_recorded_as_a_helper_and_still_terminated() {
        let tool = DurableProcessLaunch::new(Arc::new(Accept));
        let api = tool.spawn_durable("helper", &with_helper(60)).unwrap();
        let members = until_members(&tool, &api, 4);
        let roles: Vec<_> = members.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles[0], "api", "{members:?}");
        let workers = members
            .iter()
            .filter(|m| m.role.starts_with("worker-"))
            .count();
        let helpers: Vec<_> = members.iter().filter(|m| m.is_helper()).collect();
        assert_eq!(
            workers, 2,
            "the inner shell and the leader's sleep: {members:?}"
        );
        assert_eq!(helpers.len(), 1, "the inner shell's sleep: {members:?}");
        assert_eq!(helpers[0].role, "helper-0");

        let helper = helpers[0].clone();
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(helper.pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while tool.present(&helper) != Presence::Gone && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(tool.present(&helper), Presence::Gone);
        let after = tool.observe_group(&api).unwrap();
        for member in members.iter().filter(|m| !m.is_helper()) {
            assert!(after.contains(member), "{member:?} renamed: {after:?}");
        }

        tool.terminate_owned(&members, Duration::from_secs(2))
            .unwrap();
        assert!(tool.observe_group(&api).unwrap().is_empty());
    }

    /// A pid that now names a different process is never signalled.
    // T33: reconciliation from durable records never adopts or kills an arbitrary
    // process that happens to hold a recorded pid.
    #[test]
    fn a_reused_pid_is_refused() {
        let tool = DurableProcessLaunch::new(Arc::new(Accept));
        let api = tool.spawn_durable("reuse", &sleeper(60)).unwrap();
        let forged = ProcessIdentity {
            start_ticks: api.start_ticks - 1,
            ..api.clone()
        };
        assert_eq!(tool.present(&forged), Presence::Gone);
        // Terminating the forged identity signals nothing: the pid is live, but its
        // start identity is not the recorded one, so the real process survives.
        assert!(tool
            .terminate_owned(&[forged], Duration::from_millis(200))
            .is_err());
        assert_eq!(tool.present(&api), Presence::Alive);
        // Cleanup of the real one so the test leaves nothing behind.
        tool.terminate_owned(&[api], Duration::from_secs(1))
            .unwrap();
    }
}
