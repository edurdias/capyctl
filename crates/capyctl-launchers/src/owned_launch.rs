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

use crate::durable::{
    DurableSpawn, DurableSpawnError, DurableSpawnOutcome, LaunchAssociation, SpawnRole,
};
use crate::group_observation::{observe_process_group_or_empty, GroupProcessFact};
use crate::process_absence::{presence, verify_gone, GoneProof};
use crate::spawn_policy::SpawnPolicy;

/// How long to wait after SIGKILL before the outcome is reported as uncertain.
const KILL_PROOF_WINDOW: Duration = Duration::from_secs(5);
/// A `/proc` scan races every process on the host that happens to exit while it
/// runs, and fails closed when it does. That transient is retried a bounded
/// number of times so a busy host is not reported as uncertain ownership; a
/// restricted or hidden `/proc` still fails after the last attempt.
const OBSERVATION_ATTEMPTS: u32 = 8;

/// The host's POSIX shared-memory directory.
const DEV_SHM: &str = "/dev/shm";

/// The association is shared across threads because the builder's process tools
/// are, so it carries the `Send + Sync` bounds the trait itself does not impose on
/// its other, single-threaded callers.
pub struct DurableProcessLaunch {
    spawn: DurableSpawn,
    association: Arc<dyn LaunchAssociation + Send + Sync>,
    /// ADR 0033 §10: how every process of this launch is started.
    policy: SpawnPolicy,
    /// ADR 0033 (LC1): where the CUDA driver leaves a server's
    /// `cuda.shm.<uid>.<pid>.1` (`/dev/shm` on a host).
    shm_root: std::path::PathBuf,
}

impl DurableProcessLaunch {
    pub fn new(association: Arc<dyn LaunchAssociation + Send + Sync>) -> Self {
        Self {
            spawn: DurableSpawn::new(),
            association,
            policy: SpawnPolicy::default(),
            shm_root: std::path::PathBuf::from(DEV_SHM),
        }
    }

    /// ADR 0033 §10: start every process of this launch (engine, LMCache
    /// server, a wake's engine) under `policy`.
    pub fn with_policy(mut self, policy: SpawnPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Use `relay` as the log writer instead of the process's installed one.
    pub fn with_log_relay(mut self, relay: crate::engine_log_relay::LogRelay) -> Self {
        self.spawn = self.spawn.with_log_relay(relay);
        self
    }

    /// Where a stopped LMCache server's CUDA driver file is looked for
    /// (`/dev/shm`; tests name their own).
    pub fn with_shm_root(mut self, root: std::path::PathBuf) -> Self {
        self.shm_root = root;
        self
    }

    /// ADR 0033 (owner requirement from LC1): an LMCache server that had an
    /// engine connected leaves the CUDA driver's
    /// `<shm>/cuda.shm.<uid hex>.<pid hex>.1` behind after a clean exit. Once
    /// a stop has verified every recorded identity gone, exactly that file is
    /// removed for each identity of the server's group: named from this
    /// process's uid and the exited pid, never a pattern, and only a regular
    /// file (not followed through a symlink) this uid owns. An absent file is
    /// the usual case; any other failure is logged and does not fail the stop.
    fn remove_server_shm(&self, gone: &[ProcessIdentity]) {
        use std::os::unix::fs::MetadataExt;
        let uid = nix::unistd::geteuid().as_raw();
        for identity in gone.iter().filter(|identity| identity.is_lmcache()) {
            let path = self
                .shm_root
                .join(format!("cuda.shm.{uid:x}.{:x}.1", identity.pid));
            let removed = match std::fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_file() && metadata.uid() == uid => {
                    std::fs::remove_file(&path)
                }
                // Not a regular file of this uid: never this server's to remove.
                Ok(_) => Ok(()),
                Err(error) => Err(error),
            };
            match removed {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => capyctl_domain::role_log::notice(
                    capyctl_domain::role_log::Level::Warning,
                    &format!(
                        "the CUDA shared-memory file {} of the stopped LMCache server was not removed: {error}",
                        path.display()
                    ),
                ),
            }
        }
    }

    fn spawn_as(
        &self,
        incarnation: &str,
        role: SpawnRole<'_>,
        cmd: &RenderedCommand,
        descriptors: Option<&ProtectedLaunchDescriptors>,
    ) -> Result<ProcessIdentity, RuntimeError> {
        released(self.spawn.spawn_role(
            incarnation,
            role,
            cmd,
            descriptors,
            self.association.as_ref(),
            self.policy,
        ))
    }

    /// The live processes of the group `leader` leads, read from the kernel,
    /// retried over the transient failures a busy host produces.
    fn group_facts(&self, leader: &ProcessIdentity) -> Result<Vec<GroupProcessFact>, RuntimeError> {
        let mut observed = observe_process_group_or_empty(leader);
        for _ in 1..OBSERVATION_ATTEMPTS {
            use crate::group_observation::GroupObservationError::{Changed, Visibility};
            if !matches!(observed, Err(Visibility | Changed)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
            observed = observe_process_group_or_empty(leader);
        }
        let facts = observed.map_err(|error| uncertain(error.to_string()))?;
        // SPEC §13.2: a reused leader is neither ours nor proof of an empty
        // group. Dropping it from the split would manufacture cleanup.
        if facts
            .iter()
            .any(|fact| fact.pid == leader.pid && fact.start_ticks != leader.start_ticks)
        {
            return Err(uncertain("process group leader identity changed"));
        }
        Ok(facts)
    }
}

/// The recorded leader first while it lives, then every other live member in
/// start order (pids wrap, start ticks within one boot do not).
fn split_leader<'a>(
    leader: &ProcessIdentity,
    facts: &'a [GroupProcessFact],
) -> (Option<ProcessIdentity>, Vec<&'a GroupProcessFact>) {
    let mut others: Vec<_> = facts.iter().filter(|fact| fact.pid != leader.pid).collect();
    others.sort_by_key(|fact| (fact.start_ticks, fact.pid));
    let alive = facts
        .iter()
        .any(|fact| fact.pid == leader.pid && fact.start_ticks == leader.start_ticks);
    (alive.then(|| leader.clone()), others)
}

fn named(prefix: &str, list: Vec<&GroupProcessFact>) -> Vec<ProcessIdentity> {
    list.into_iter()
        .enumerate()
        .map(|(index, fact)| ProcessIdentity {
            role: format!("{prefix}{index}"),
            pid: fact.pid,
            boot_id: fact.boot_id.clone(),
            start_ticks: fact.start_ticks,
        })
        .collect()
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
        self.spawn_as(incarnation, SpawnRole::Engine, cmd, None)
    }

    fn spawn_durable_protected(
        &self,
        incarnation: &str,
        cmd: &RenderedCommand,
        descriptors: &ProtectedLaunchDescriptors,
    ) -> Result<ProcessIdentity, RuntimeError> {
        self.spawn_as(incarnation, SpawnRole::Engine, cmd, Some(descriptors))
    }

    fn spawn_lmcache(
        &self,
        incarnation: &str,
        cmd: &RenderedCommand,
    ) -> Result<ProcessIdentity, RuntimeError> {
        self.spawn_as(incarnation, SpawnRole::Lmcache, cmd, None)
    }

    fn spawn_durable_wake(
        &self,
        incarnation: &str,
        wake: &str,
        cmd: &RenderedCommand,
        descriptors: Option<&ProtectedLaunchDescriptors>,
    ) -> Result<ProcessIdentity, RuntimeError> {
        self.spawn_as(incarnation, SpawnRole::Wake(wake), cmd, descriptors)
    }

    fn present(&self, identity: &ProcessIdentity) -> Presence {
        presence(identity)
    }

    fn building(&self, api: &ProcessIdentity) -> bool {
        crate::group_observation::group_building(api)
    }

    fn observe_group(&self, api: &ProcessIdentity) -> Result<Vec<ProcessIdentity>, RuntimeError> {
        let facts = self.group_facts(api)?;
        let (leader, others) = split_leader(api, &facts);
        let mut members: Vec<ProcessIdentity> = leader.into_iter().collect();
        // ADR 0027: the processes the API process started itself are the
        // engine's workers. Anything else in the group was started by one of
        // them (a compile worker pool) or outlived its parent: a helper,
        // recorded for cleanup, whose exit is not the engine's. Each kind is
        // numbered on its own, so a helper exiting renames no worker.
        let (workers, helpers): (Vec<_>, Vec<_>) = others
            .into_iter()
            .partition(|fact| fact.parent_pid == api.pid);
        members.extend(named("worker-", workers));
        members.extend(named(
            capyctl_domain::completion::HELPER_ROLE_PREFIX,
            helpers,
        ));
        Ok(members)
    }

    fn observe_lmcache_group(
        &self,
        leader: &ProcessIdentity,
    ) -> Result<Vec<ProcessIdentity>, RuntimeError> {
        if leader.role != capyctl_domain::completion::LMCACHE_ROLE {
            return Err(uncertain("the LMCache group's leader is not `lmcache`"));
        }
        let facts = self.group_facts(leader)?;
        // ADR 0033 §3: every other process of the server's group, whoever
        // started it, is `lmcache-N` by start order.
        let (alive, others) = split_leader(leader, &facts);
        let mut members: Vec<ProcessIdentity> = alive.into_iter().collect();
        members.extend(named(
            capyctl_domain::completion::LMCACHE_MEMBER_PREFIX,
            others,
        ));
        Ok(members)
    }

    fn stop_group(
        &self,
        identities: &[ProcessIdentity],
        grace: Duration,
    ) -> Result<(), RuntimeError> {
        // ADR 0033 §§3, 7: exactly one owned group: the server's, or the
        // engine's with its helpers. The launch's Terminate stops both.
        let servers = identities.iter().filter(|id| id.is_lmcache()).count();
        if identities.is_empty() || (servers != 0 && servers != identities.len()) {
            return Err(uncertain(
                "a group stop names exactly one owned group of the launch",
            ));
        }
        self.terminate_escalating(identities, grace).map(drop)
    }

    fn terminate_owned(
        &self,
        identities: &[ProcessIdentity],
        grace: Duration,
    ) -> Result<(), RuntimeError> {
        self.terminate_escalating(identities, grace).map(drop)
    }
}

/// One owned process group of a launch, as recorded: the engine's (`api`, its
/// workers and helpers) or the LMCache server's (`lmcache`, `lmcache-N`).
struct RecordedGroup {
    leader: ProcessIdentity,
    members: Vec<ProcessIdentity>,
    /// SPEC §13.2: the group id is provably ours, so the group may be signalled.
    provable: bool,
}

impl DurableProcessLaunch {
    /// SPEC §13.2: terminate the recorded group with SIGTERM, a bounded wait
    /// of `grace`, then SIGKILL, and prove it gone. ADR 0028 §11: the answer
    /// says whether SIGKILL was needed (`true`), which a group member's gone
    /// report carries as `escalated`; a SGLang worker ignores SIGTERM, so its
    /// stop normally escalates. Never waits past grace plus the proof window.
    ///
    /// ADR 0033 §3: a launch with an LMCache server owns two groups. The
    /// recorded set is split by group (the engine's, led by `api`; the
    /// server's, led by `lmcache`), each is signalled by the rule below, both
    /// share one grace window, and the stop is settled only when every
    /// recorded identity is gone and every group is empty. The same function
    /// stops an engine-only or a server-only subset.
    pub fn terminate_escalating(
        &self,
        identities: &[ProcessIdentity],
        grace: Duration,
    ) -> Result<bool, RuntimeError> {
        let escalated = self.terminate_groups(identities, grace)?;
        // Every recorded identity is proven gone here, and only here.
        self.remove_server_shm(identities);
        Ok(escalated)
    }

    fn terminate_groups(
        &self,
        identities: &[ProcessIdentity],
        grace: Duration,
    ) -> Result<bool, RuntimeError> {
        // Presence that cannot be established is retention, never absence, so it is
        // settled before anything is signalled or released.
        for identity in identities {
            // SPEC §13.2 / T12: an exited process its parent has not reaped yet
            // is not unreadable. It holds nothing but its pid, and stopping the
            // group can end that parent, which hands it to this role to reap;
            // the proof below still waits for it.
            if presence(identity) == Presence::Unknown
                && !crate::process_absence::exited_unreaped(identity)
            {
                return Err(uncertain(format!(
                    "presence of pid {} could not be established",
                    identity.pid
                )));
            }
        }
        let (server, engine): (Vec<_>, Vec<_>) = identities
            .iter()
            .cloned()
            .partition(ProcessIdentity::is_lmcache);
        let mut groups = Vec::new();
        for (members, leader_role) in [
            (engine, "api"),
            (server, capyctl_domain::completion::LMCACHE_ROLE),
        ] {
            if members.is_empty() {
                continue;
            }
            let Some(leader) = members.iter().find(|id| id.role == leader_role).cloned() else {
                // With no leader recorded there is no group to signal; only proof
                // that the recorded set is already gone can end this without one.
                if verify_gone(&members) == GoneProof::AllGone {
                    continue;
                }
                return Err(uncertain(format!(
                    "no {leader_role} identity recorded and the set is not proven gone"
                )));
            };
            // SPEC §13.2: signal only a group whose id is provably ours: its
            // leader is alive (`presence` compares boot id and start ticks, so
            // a reused pid reads as Gone and is never signalled), or this
            // launcher still holds it unreaped, which pins the group id (ADR
            // 0028 §11, `pinned_leader`).
            let provable =
                presence(&leader) == Presence::Alive || crate::pinned_leader::is_held(&leader);
            groups.push(RecordedGroup {
                leader,
                members,
                provable,
            });
        }
        if groups.is_empty() {
            return Ok(false);
        }
        for group in &groups {
            if group.provable {
                signal_launch_group(&group.leader, nix::sys::signal::Signal::SIGTERM)?;
            } else {
                // The head crashed and left its workers behind, and this process
                // does not hold the leader (it was launched before this role
                // restarted). There is no leader to signal the group through, but
                // each recorded identity carries a boot id and start ticks that
                // `presence` has just matched, so naming them one by one kills
                // exactly the launch's own processes and nothing else. Refusing
                // here instead would pause an operator over a state capyctl can
                // prove and end.
                //
                // ADR 0028 §8, §11: only recorded identities are signalled here.
                // A leaderless group scan can name an unrelated group once the
                // leader's pid is reused, so it never authorizes a signal: an
                // unrecorded process still in the group is settlement evidence
                // only, and the stop below reports it uncertain (claim retained).
                signal_recorded(&group.members, nix::sys::signal::Signal::SIGTERM)?;
            }
        }
        if self.settled(identities, &groups, Instant::now() + grace, 200)? {
            return Ok(false);
        }
        for group in &groups {
            if group.provable {
                // ADR 0028 §11 (MN5, found live): a leader that died, during grace or
                // before this stop, is still held, so the processes of its group that
                // were never recorded (a vLLM worker the engine forked after the
                // tree was recorded) get the group's SIGKILL too.
                signal_launch_group(&group.leader, nix::sys::signal::Signal::SIGKILL)?;
                // Spec §5 escalates per identity, not only per group. A leader that
                // died during grace and was not held can no longer be signalled
                // through, so a worker still holding device memory would otherwise
                // never receive the group's SIGKILL. Everything recorded has had
                // SIGTERM and the whole grace window by now, so the second signal
                // follows immediately rather than opening another grace window: a
                // stop stays within grace plus the proof window, which is what the
                // coordinator validates its protocol timeout against.
                signal_recorded(&group.members, nix::sys::signal::Signal::SIGTERM)?;
            }
            signal_recorded(&group.members, nix::sys::signal::Signal::SIGKILL)?;
        }
        if self.settled(identities, &groups, Instant::now() + KILL_PROOF_WINDOW, 100)? {
            return Ok(true);
        }
        let mut empty = true;
        for group in &groups {
            empty &= self.group_facts(&group.leader)?.is_empty();
        }
        match (verify_gone(identities), empty) {
            (GoneProof::AllGone, true) => Ok(true),
            (GoneProof::AllGone, false) => Err(uncertain(
                "recorded processes gone but the group still has members",
            )),
            (GoneProof::SomeAlive, _) => {
                Err(uncertain("a recorded process is still alive after SIGKILL"))
            }
            (GoneProof::Indeterminate, _) => {
                Err(uncertain("process absence could not be established"))
            }
            (GoneProof::Unreaped, _) => Err(uncertain(
                "a recorded process exited but its parent has not reaped it (a zombie)",
            )),
        }
    }
}

/// An association for a tool that only stops: it never spawns.
struct StopOnly;

impl LaunchAssociation for StopOnly {
    fn persist_api_identity(
        &self,
        _identity: &ProcessIdentity,
    ) -> Result<(), crate::durable::AssociationError> {
        Err(crate::durable::AssociationError::Uncertain(
            "a stop-only tool never launches".into(),
        ))
    }
}

/// ADR 0031 §7: stop a launch's recorded processes from outside its builder,
/// as [`DurableProcessLaunch::terminate_escalating`] does (SIGTERM, `grace`,
/// SIGKILL, then proof that every identity and the group are gone). The
/// owner of the recorded identities calls it when a launch whose processes
/// it holds must not keep running (an engine listening off loopback); the
/// claim and accounting stay until the settlement records the gone evidence.
pub fn terminate_recorded(
    identities: &[ProcessIdentity],
    grace: Duration,
) -> Result<bool, RuntimeError> {
    DurableProcessLaunch::new(Arc::new(StopOnly)).terminate_escalating(identities, grace)
}

impl DurableProcessLaunch {
    /// Poll until every recorded process is proven gone and every group itself
    /// is empty, or the deadline passes. Both halves are required: a recorded
    /// process may exit while a worker it started keeps running in the same group.
    /// An observation that fails here is "not settled yet", not a verdict. A
    /// hidden `/proc` entry on a busy host, or unrelated process churn, would
    /// otherwise end the stop early and report uncertainty for a state the next
    /// poll would have proven. Only the final match after the proof window turns
    /// an observation failure into Uncertain.
    fn settled(
        &self,
        identities: &[ProcessIdentity],
        groups: &[RecordedGroup],
        deadline: Instant,
        poll_millis: u64,
    ) -> Result<bool, RuntimeError> {
        while Instant::now() < deadline {
            let gone = verify_gone(identities) == GoneProof::AllGone;
            if gone
                && groups.iter().all(|group| {
                    self.group_facts(&group.leader)
                        .is_ok_and(|facts| facts.is_empty())
                })
            {
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
///
/// SPEC §13.2, ADR 0028 §11: through the hold when this launcher holds the
/// leader, alive or exited (the id cannot be reused while it is held);
/// otherwise only through a leader `presence` reads alive now. Anything else
/// is not signalled at all.
fn signal_launch_group(
    api: &ProcessIdentity,
    signal: nix::sys::signal::Signal,
) -> Result<(), RuntimeError> {
    let sent = match crate::pinned_leader::signal_group(api, signal) {
        Some(sent) => sent,
        None if presence(api) == Presence::Alive => {
            nix::sys::signal::killpg(nix::unistd::Pid::from_raw(api.pid as i32), signal)
        }
        None => return Ok(()),
    };
    match sent {
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
        assert_group_empties(&tool, &api);
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
        assert_group_empties(&tool, &api);
    }

    /// Whether `pid` runs: an exited process (a zombie) does not.
    fn runs(pid: u32) -> bool {
        crate::subreaper::proc_stat(pid).is_some_and(|stat| !stat.exited())
    }

    /// After a Terminate the group's processes have exited, but a member that
    /// was reparented to init (its leader died first) stays a zombie in the
    /// group until init reaps it, which on a loaded machine is not immediate.
    /// The group must empty within a bound, not at the very next scan.
    fn assert_group_empties(tool: &DurableProcessLaunch, api: &ProcessIdentity) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let left = tool.observe_group(api).unwrap();
            if left.is_empty() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the group never emptied: {left:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// ADR 0028 §11 (MN5 vLLM, found live): the leader of a group worker died
    /// and left a worker it forked after the tree was recorded, so only the
    /// leader is recorded. This launcher still holds the dead leader unreaped,
    /// so the group id is still the launch's: Terminate signals the whole
    /// group, the unrecorded worker ends, and the recorded set is proven gone
    /// with the group empty. Before the hold, the leader was reaped at once and
    /// the stop answered "recorded processes gone but the group still has
    /// members" for as long as the worker ran.
    // T31, T33
    #[test]
    fn an_unrecorded_worker_of_a_dead_held_leader_is_terminated() {
        let tool = DurableProcessLaunch::new(Arc::new(Accept));
        let api = tool.spawn_durable("orphan", &sleeper(60)).unwrap();
        let members = until_group(&tool, &api);
        assert!(members.len() >= 2, "{members:?}");
        let worker = members[1].clone();
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
        assert!(runs(worker.pid), "the worker outlived its leader");
        // The leader's exit is known, as before: its launcher observed it.
        assert_eq!(
            crate::reaped::status_of(&api),
            Some(crate::reaped::ReapedStatus::Signal(9))
        );

        // Only the leader is recorded, as the live worker's tree was.
        let escalated = tool
            .terminate_escalating(std::slice::from_ref(&api), Duration::from_secs(2))
            .unwrap();
        assert!(!escalated, "a sleep ends on SIGTERM");
        assert!(!runs(worker.pid), "the unrecorded worker was terminated");
        assert_group_empties(&tool, &api);
        assert_eq!(verify_gone(&[api.clone(), worker]), GoneProof::AllGone);
        // Nothing of the group runs any more, so the leader is reaped.
        let deadline = Instant::now() + Duration::from_secs(5);
        while std::path::Path::new(&format!("/proc/{}", api.pid)).exists() {
            assert!(Instant::now() < deadline, "the leader was never reaped");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(tool.present(&api), Presence::Gone);
    }

    /// SPEC §13.2, ADR 0028 §8, §11: a leaderless group this launcher does not
    /// hold (its leader was spawned and reaped elsewhere, as after a restart of
    /// the role) is never signalled by its id: its unrecorded survivor keeps
    /// running and the stop is uncertain.
    // T33
    #[test]
    fn an_unheld_leaderless_group_is_never_signalled() {
        use std::os::unix::process::CommandExt;
        let mut command = std::process::Command::new("sh");
        command
            .args(["-c", "sleep 60 & echo $!; sleep 60"])
            .stdout(std::process::Stdio::piped())
            .process_group(0);
        let mut leader = crate::subreaper::spawn_direct(&mut command).unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(leader.stdout.take().unwrap()),
            &mut line,
        )
        .unwrap();
        let survivor: u32 = line.trim().parse().unwrap();
        let api = crate::exec::process_identity(leader.id(), "api").unwrap();
        leader.kill().unwrap();
        leader.wait().unwrap();
        assert_eq!(presence(&api), Presence::Gone);

        let tool = DurableProcessLaunch::new(Arc::new(Accept));
        assert!(tool
            .terminate_escalating(std::slice::from_ref(&api), Duration::from_millis(200))
            .is_err());
        assert!(runs(survivor), "an unheld group was signalled");
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(survivor as i32),
            nix::sys::signal::Signal::SIGKILL,
        )
        .unwrap();
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
        assert_group_empties(&tool, &api);
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
