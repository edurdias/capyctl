//! ADR 0028: a fake multi-host engine group, the engine side of the
//! coordinator's group tests.
//!
//! One [`FakeGroup`] is the engine every member host of a group runs a rank
//! of: rank `r` runs on the `r`-th host it was built with, rank 0 is the head.
//! It holds what a real group's ranks share and a test needs to steer: which
//! ranks launched and which have ended, each rank's resident bytes, the
//! sleep/wake state the head's calls move, the tokens a completion returns,
//! and which hosts are reachable. Scripted hosts answer their agents'
//! commands from it; tests inject faults into it directly.
//!
//! The handle is cheap to clone and every clone shares one state, so the
//! hosts and the test see the same group. Every change is announced on
//! [`FakeGroup::subscribe`], so a host waiting on the group (a head waiting for
//! readiness) wakes without polling.
//!
//! **Not qualification.** Nothing here says a real engine group forms,
//! serves, sleeps or wakes; the live rows MN1–MN9 do (SPEC §18).
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};

/// The bytes one serving rank holds resident.
pub const RANK_RESIDENT_BYTES: u64 = 8 << 30;
/// The bytes one rank keeps after a clean sleep.
pub const RANK_PARKED_BYTES: u64 = 1 << 30;

/// The journal an agent comes back with when its host reconnects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JournalState {
    /// The agent kept its journal: it still knows every launch it recorded.
    Kept,
    /// The agent restarted with an empty journal: it knows no launch, but the
    /// processes it had started may still run (Review Focus 3).
    Empty,
}

/// How a rank's process ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RankEnd {
    /// The engine rank exited on its own; its agent observes the exit.
    Exited,
    /// The process was killed from outside CapyCTL (an operator, the kernel);
    /// nothing reports it, only a later observation finds it gone.
    Killed,
    /// Its host terminated it on a Terminate.
    Terminated,
}

#[derive(Debug)]
struct Rank {
    host: String,
    launched: bool,
    ended: Option<RankEnd>,
    resident: u64,
    /// ADR 0028 §9, Review Focus 4: a sleep leaves this rank resident.
    stays_resident: bool,
}

impl Rank {
    fn alive(&self) -> bool {
        self.launched && self.ended.is_none()
    }
}

#[derive(Debug)]
struct State {
    ranks: Vec<Rank>,
    launches: u32,
    launch_completed: bool,
    disconnected: BTreeSet<String>,
    journals: BTreeMap<String, JournalState>,
    /// Hosts whose agent came back with an empty journal and has not yet
    /// forgotten its launches.
    journal_losses: BTreeSet<String>,
    asleep: bool,
    sleep_calls: u32,
    /// What completions return after the next wake, when set.
    pending_wake_output: Option<Vec<u32>>,
    wake_output: Option<Vec<u32>>,
}

/// A clonable handle on one fake engine group.
#[derive(Clone, Debug)]
pub struct FakeGroup {
    state: Arc<Mutex<State>>,
    changes: Arc<tokio::sync::watch::Sender<u64>>,
}

impl FakeGroup {
    /// A group with one rank per host, in rank order: `hosts[0]` heads it.
    pub fn new(hosts: &[&str]) -> Self {
        assert!(!hosts.is_empty(), "a group has at least one rank");
        let ranks = hosts
            .iter()
            .map(|host| Rank {
                host: (*host).to_owned(),
                launched: false,
                ended: None,
                resident: 0,
                stays_resident: false,
            })
            .collect();
        Self {
            state: Arc::new(Mutex::new(State {
                ranks,
                launches: 0,
                launch_completed: false,
                disconnected: BTreeSet::new(),
                journals: BTreeMap::new(),
                journal_losses: BTreeSet::new(),
                asleep: false,
                sleep_calls: 0,
                pending_wake_output: None,
                wake_output: None,
            })),
            changes: Arc::new(tokio::sync::watch::channel(0).0),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("fake group state")
    }

    /// Apply `change` and announce it.
    fn change<T>(&self, change: impl FnOnce(&mut State) -> T) -> T {
        let out = change(&mut self.state());
        self.changes.send_modify(|version| *version += 1);
        out
    }

    fn rank_mut(state: &mut State, rank: u32) -> &mut Rank {
        let size = state.ranks.len();
        state
            .ranks
            .get_mut(rank as usize)
            .unwrap_or_else(|| panic!("rank {rank} outside a group of {size}"))
    }

    fn rank<T>(&self, rank: u32, read: impl FnOnce(&Rank) -> T) -> T {
        let state = self.state();
        let size = state.ranks.len();
        read(
            state
                .ranks
                .get(rank as usize)
                .unwrap_or_else(|| panic!("rank {rank} outside a group of {size}")),
        )
    }

    /// A receiver that sees every change to the group.
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }

    /// The group's hosts in rank order.
    pub fn hosts(&self) -> Vec<String> {
        self.state().ranks.iter().map(|r| r.host.clone()).collect()
    }

    /// The rank `host` runs, if it is a member.
    pub fn rank_of(&self, host: &str) -> Option<u32> {
        self.state()
            .ranks
            .iter()
            .position(|r| r.host == host)
            .map(|rank| rank as u32)
    }

    /// The number of ranks.
    pub fn world_size(&self) -> u32 {
        self.state().ranks.len() as u32
    }

    // ---- the engine's own lifecycle ---------------------------------------

    /// Rank `rank`'s process starts and joins the group, fully resident. A
    /// relaunch of an ended rank is a fresh process.
    pub fn launch(&self, rank: u32) {
        self.change(|state| {
            state.launches += 1;
            let r = Self::rank_mut(state, rank);
            r.launched = true;
            r.ended = None;
            r.resident = RANK_RESIDENT_BYTES;
        });
    }

    /// How many rank processes were started, over the group's whole life.
    pub fn launches(&self) -> u32 {
        self.state().launches
    }

    /// ADR 0028 §9: the head serves only once every rank has launched and
    /// none has ended (a rank that ends hangs the rest; it never shrinks
    /// the group).
    pub fn head_ready(&self) -> bool {
        let state = self.state();
        state.ranks.iter().all(Rank::alive)
    }

    /// The engine finishes initializing (weights loaded, collectives up). A
    /// scripted head answers its Launch as ready only after this and
    /// [`FakeGroup::head_ready`], so a test can hold the group between
    /// "every rank launched" and "the head serves".
    pub fn launch_completes(&self) {
        self.change(|state| state.launch_completed = true);
    }

    /// Whether [`FakeGroup::launch_completes`] has been called.
    pub fn launch_completed(&self) -> bool {
        self.state().launch_completed
    }

    /// Whether rank `rank`'s process runs.
    pub fn alive(&self, rank: u32) -> bool {
        self.rank(rank, Rank::alive)
    }

    /// How rank `rank`'s process ended, if it has.
    pub fn ended(&self, rank: u32) -> Option<RankEnd> {
        self.rank(rank, |r| r.ended)
    }

    /// The bytes rank `rank` holds now; nothing once its process ended.
    pub fn resident_bytes(&self, rank: u32) -> u64 {
        self.rank(rank, |r| r.resident)
    }

    /// The bytes one rank keeps after a clean sleep.
    pub fn parked_bytes(&self) -> u64 {
        RANK_PARKED_BYTES
    }

    /// ADR 0028 §11: rank `rank`'s host terminates its process, as on a
    /// Terminate. Returns whether the terminate had to escalate: a rank whose
    /// group already lost another rank sits in a hung collective and ignores
    /// SIGTERM (Review Focus 6), so its host kills it. A rank that is not
    /// running needs nothing and never escalates.
    pub fn terminate_rank(&self, rank: u32) -> bool {
        self.change(|state| {
            let hung = state
                .ranks
                .iter()
                .enumerate()
                .any(|(other, r)| other != rank as usize && r.ended.is_some());
            let r = Self::rank_mut(state, rank);
            if !r.alive() {
                return false;
            }
            r.ended = Some(RankEnd::Terminated);
            r.resident = 0;
            hung
        })
    }

    // ---- faults ----------------------------------------------------------

    /// Rank `rank`'s engine process exits on its own.
    pub fn exit_rank(&self, rank: u32) {
        self.change(|state| {
            let r = Self::rank_mut(state, rank);
            if r.alive() {
                r.ended = Some(RankEnd::Exited);
                r.resident = 0;
            }
        });
    }

    /// Rank `rank`'s process is killed from outside CapyCTL; nothing reports
    /// it.
    pub fn kill_rank_process(&self, rank: u32) {
        self.change(|state| {
            let r = Self::rank_mut(state, rank);
            if r.alive() {
                r.ended = Some(RankEnd::Killed);
                r.resident = 0;
            }
        });
    }

    /// `host` stops answering: its agent session is gone. Its rank's process
    /// keeps running.
    pub fn disconnect_host(&self, host: &str) {
        self.change(|state| {
            state.disconnected.insert(host.to_owned());
        });
    }

    /// `host` answers again, its agent holding `journal`. With
    /// [`JournalState::Empty`] the agent restarted and forgot every launch;
    /// the processes it started are untouched.
    pub fn reconnect_host(&self, host: &str, journal: JournalState) {
        self.change(|state| {
            state.disconnected.remove(host);
            state.journals.insert(host.to_owned(), journal);
            if journal == JournalState::Empty {
                state.journal_losses.insert(host.to_owned());
            }
        });
    }

    /// Whether `host` answers.
    pub fn connected(&self, host: &str) -> bool {
        !self.state().disconnected.contains(host)
    }

    /// The journal `host`'s agent last came back with; [`JournalState::Kept`]
    /// for a host that never reconnected.
    pub fn journal(&self, host: &str) -> JournalState {
        self.state()
            .journals
            .get(host)
            .copied()
            .unwrap_or(JournalState::Kept)
    }

    /// Whether `host`'s agent came back with an empty journal since the last
    /// call: the scripted host's cue to forget what it recorded. True once
    /// per [`FakeGroup::reconnect_host`] with [`JournalState::Empty`].
    pub fn take_journal_loss(&self, host: &str) -> bool {
        self.state().journal_losses.remove(host)
    }

    /// The next sleep frees nothing on rank `rank`, while the head's call
    /// still reports success (the TP > 1 sleep bug class, Review Focus 4).
    pub fn sleep_leaves_rank_resident(&self, rank: u32) {
        self.change(|state| Self::rank_mut(state, rank).stays_resident = true);
    }

    /// After the next wake, completions return `tokens` (a wake that came
    /// back with different weights or state).
    pub fn wake_output(&self, tokens: Vec<u32>) {
        self.change(|state| state.pending_wake_output = Some(tokens));
    }

    // ---- the head's calls ------------------------------------------------

    /// How many times the head was asked to sleep.
    pub fn sleep_calls(&self) -> u32 {
        self.state().sleep_calls
    }

    /// ADR 0028 §9: the head's sleep call, which reaches every rank. Every
    /// rank drops to [`RANK_PARKED_BYTES`] except one faulted with
    /// [`FakeGroup::sleep_leaves_rank_resident`], which keeps everything;
    /// the call reports success either way. A group that is not serving
    /// refuses.
    pub fn sleep(&self) -> Result<(), String> {
        self.change(|state| {
            state.sleep_calls += 1;
            if !state.ranks.iter().all(Rank::alive) {
                return Err("the group is not serving: a rank is not running".into());
            }
            if state.asleep {
                return Err("the group is already asleep".into());
            }
            for rank in &mut state.ranks {
                if !rank.stays_resident {
                    rank.resident = RANK_PARKED_BYTES;
                }
            }
            state.asleep = true;
            Ok(())
        })
    }

    /// The head's wake call: every rank is fully resident again. A group
    /// that is not asleep, or lost a rank, refuses.
    pub fn wake(&self) -> Result<(), String> {
        self.change(|state| {
            if !state.ranks.iter().all(Rank::alive) {
                return Err("the group is not serving: a rank is not running".into());
            }
            if !state.asleep {
                return Err("the group is not asleep".into());
            }
            for rank in &mut state.ranks {
                rank.resident = RANK_RESIDENT_BYTES;
            }
            state.asleep = false;
            if let Some(tokens) = state.pending_wake_output.take() {
                state.wake_output = Some(tokens);
            }
            Ok(())
        })
    }

    /// One completion through the head. A serving group answers with tokens
    /// that depend on `prompt` alone (the same prompt, the same tokens), or
    /// with the output a [`FakeGroup::wake_output`] fault installed at the
    /// last wake. A group that is not serving (a rank down, or asleep)
    /// answers nothing.
    pub fn complete(&self, prompt: &str) -> Vec<u32> {
        let state = self.state();
        if state.asleep || !state.ranks.iter().all(Rank::alive) {
            return Vec::new();
        }
        match &state.wake_output {
            Some(tokens) => tokens.clone(),
            None => prompt.bytes().map(u32::from).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // T30: the head is not ready until every rank has launched.
    #[test]
    fn head_waits_for_every_rank() {
        let g = FakeGroup::new(&["a", "b", "c", "d"]);
        for r in 0..3 {
            g.launch(r);
        }
        assert!(!g.head_ready());
        g.launch(3);
        assert!(g.head_ready());
    }

    // T31: a rank exit leaves the others alive but not serving.
    #[test]
    fn rank_exit_hangs_the_rest() {
        let g = FakeGroup::new(&["a", "b"]);
        g.launch(0);
        g.launch(1);
        g.exit_rank(1);
        assert!(g.alive(0));
        assert!(!g.head_ready());
    }

    // T20: a faulty sleep leaves one rank resident while the head reports success.
    #[test]
    fn faulty_sleep_leaves_a_rank_resident() {
        let g = FakeGroup::new(&["a", "b"]);
        g.launch(0);
        g.launch(1);
        g.sleep_leaves_rank_resident(1);
        assert!(g.sleep().is_ok());
        assert_eq!(g.sleep_calls(), 1);
        assert!(g.resident_bytes(1) > g.parked_bytes());
        assert_eq!(g.resident_bytes(0), g.parked_bytes());
    }

    // T20: a clean sleep and wake round trip keeps the completion; a
    // wake_output fault changes what completes after the next wake only.
    #[test]
    fn wake_output_applies_after_the_next_wake() {
        let g = FakeGroup::new(&["a", "b"]);
        g.launch(0);
        g.launch(1);
        let reference = g.complete("canary");
        assert!(!reference.is_empty());
        g.sleep().unwrap();
        assert!(
            g.complete("canary").is_empty(),
            "an asleep group serves nothing"
        );
        g.wake().unwrap();
        assert_eq!(g.complete("canary"), reference);
        g.wake_output(vec![9, 9, 9]);
        assert_eq!(g.complete("canary"), reference);
        g.sleep().unwrap();
        g.wake().unwrap();
        assert_eq!(g.complete("canary"), vec![9, 9, 9]);
        assert_eq!(g.resident_bytes(1), RANK_RESIDENT_BYTES);
    }

    // T31, Review Focus 6: an exit is the rank's own, a kill is silent, and a
    // terminate of a rank left hanging by another's exit escalates.
    #[test]
    fn exits_kills_and_terminates_are_told_apart() {
        let g = FakeGroup::new(&["a", "b", "c"]);
        for r in 0..3 {
            g.launch(r);
        }
        assert!(
            !g.terminate_rank(2),
            "a healthy group's rank stops on SIGTERM"
        );
        g.exit_rank(0);
        assert_eq!(g.ended(0), Some(RankEnd::Exited));
        assert!(g.terminate_rank(1), "a hung rank needs SIGKILL");
        assert_eq!(g.ended(1), Some(RankEnd::Terminated));
        assert!(!g.terminate_rank(1), "an ended rank needs nothing");
        g.launch(2);
        g.kill_rank_process(2);
        assert_eq!(g.ended(2), Some(RankEnd::Killed));
        assert_eq!(g.resident_bytes(2), 0);
        assert_eq!(g.launches(), 4);
    }

    // T32, Review Focus 3: a disconnected host keeps its rank running; an
    // empty-journal reconnect is reported once.
    #[test]
    fn hosts_disconnect_and_come_back_with_their_journal() {
        let g = FakeGroup::new(&["a", "b"]);
        g.launch(1);
        g.disconnect_host("b");
        assert!(!g.connected("b") && g.alive(1));
        g.reconnect_host("b", JournalState::Empty);
        assert!(g.connected("b"));
        assert_eq!(g.journal("b"), JournalState::Empty);
        assert!(g.take_journal_loss("b"));
        assert!(!g.take_journal_loss("b"));
        assert!(g.alive(1));
        g.reconnect_host("b", JournalState::Kept);
        assert!(!g.take_journal_loss("b"));
        assert_eq!(g.journal("a"), JournalState::Kept);
    }

    #[tokio::test]
    async fn every_change_is_announced() {
        let g = FakeGroup::new(&["a", "b"]);
        let mut changes = g.subscribe();
        g.launch_completes();
        changes.changed().await.unwrap();
        assert!(g.launch_completed());
        assert_eq!(g.rank_of("b"), Some(1));
        assert_eq!(g.hosts(), vec!["a".to_owned(), "b".to_owned()]);
        assert_eq!(g.world_size(), 2);
    }
}
