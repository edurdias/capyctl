use capyctl_agent::journal::{Acceptance, HostJournal, JournalError, LocalExecutionPolicy};
use capyctl_domain::group::{CommandIdentity, MemberKey};
use capyctl_protocol::execution::{MemberAction, MemberCommand};
use std::os::unix::fs::PermissionsExt;

struct Policy;
impl LocalExecutionPolicy for Policy {
    fn authorize(&self, _: &MemberCommand) -> Result<(), JournalError> {
        Ok(())
    }
    fn render_launch(
        &self,
        _: &MemberCommand,
    ) -> Result<capyctl_agent::journal::ApprovedLaunch, JournalError> {
        Err(JournalError::Unauthorized)
    }
}
fn directory() -> tempfile::TempDir {
    let d = tempfile::Builder::new()
        .prefix("capyctl-journal-")
        .tempdir_in(std::env::var("HOME").unwrap())
        .unwrap();
    std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    d
}
struct ChildCleanup(capyctl_domain::completion::ProcessIdentity);
impl Drop for ChildCleanup {
    fn drop(&mut self) {
        use capyctl_adapters::traits::OwnedProcessLaunch;
        struct NoSpawn;
        impl capyctl_launchers::LaunchAssociation for NoSpawn {
            fn persist_api_identity(
                &self,
                _: &capyctl_domain::completion::ProcessIdentity,
            ) -> Result<(), capyctl_launchers::AssociationError> {
                panic!("cleanup cannot spawn")
            }
        }
        let tools = capyctl_launchers::DurableProcessLaunch::new(std::sync::Arc::new(NoSpawn));
        let _ = tools.terminate_owned(
            std::slice::from_ref(&self.0),
            std::time::Duration::from_millis(100),
        );
    }
}

fn command(id: &str) -> MemberCommand {
    let mut c = MemberCommand {
        identity: CommandIdentity {
            controller_id: "controller".into(),
            member: MemberKey {
                host_id: "host".into(),
                member_id: "head".into(),
            },
            deployment_id: "deployment".into(),
            operation_id: "operation".into(),
            command_id: id.into(),
            step_id: id.into(),
            generation: 1,
            revision: 1,
            deadline_ms: 1000,
            payload_digest: [0; 32],
            expected_state: "launching".into(),
            profile_fingerprint: "approved".into(),
            instance_index: 0,
        },
        action: MemberAction::Inspect,
    };
    c.identity.payload_digest = c.canonical_digest();
    c
}
// T09 T13 T33: restart and compaction cannot turn accepted delivery into fresh work.
#[test]
fn acceptance_is_durable_and_replay_never_mints_an_execution_ticket() {
    let d = directory();
    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    let s = j.connect().unwrap();
    let c = command("inspect");
    assert!(matches!(
        j.accept(s, &c, 10, &Policy).unwrap(),
        Acceptance::Fresh(_)
    ));
    assert!(matches!(
        j.accept(s, &c, 10, &Policy).unwrap(),
        Acceptance::Replay(_)
    ));
    drop(j);
    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    assert!(j.accept(s, &c, 10, &Policy).is_err());
    let s = j.connect().unwrap();
    assert!(matches!(
        j.accept(s, &c, 10, &Policy).unwrap(),
        Acceptance::Replay(_)
    ));
    assert_eq!(j.history(0, 100).unwrap().len(), 1);
}
// T13 T34: controller, session, deadline and immutable payload fences.
#[test]
fn rejects_changed_payload_expired_stale_and_disconnected_commands() {
    let d = directory();
    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    let s = j.connect().unwrap();
    let c = command("one");
    j.accept(s, &c, 10, &Policy).unwrap();
    let mut changed = c.clone();
    changed.identity.revision = 2;
    changed.identity.payload_digest = changed.canonical_digest();
    assert!(matches!(
        j.accept(s, &changed, 10, &Policy),
        Err(JournalError::Conflict)
    ));
    assert!(j.accept(s, &command("expired"), 1000, &Policy).is_err());
    let newer = j.connect().unwrap();
    assert!(j.accept(s, &command("stale"), 10, &Policy).is_err());
    j.disconnect(newer).unwrap();
    assert!(j.accept(newer, &command("lost"), 10, &Policy).is_err());
}
// T33: exactly one local resource registry and persistent enrolled authority.
#[test]
fn exclusive_lock_and_persistent_identity_binding() {
    let d = directory();
    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    assert!(HostJournal::open(d.path(), "controller", "host").is_err());
    drop(j);
    assert!(HostJournal::open(d.path(), "other-controller", "host").is_err());
    assert!(HostJournal::open(d.path(), "controller", "other-host").is_err());
}

fn fresh(acceptance: Acceptance) -> capyctl_agent::journal::ExecutionTicket {
    match acceptance {
        Acceptance::Fresh(ticket) => ticket,
        other => panic!("expected fresh acceptance: {other:?}"),
    }
}
fn sign(mut command: MemberCommand) -> MemberCommand {
    command.identity.payload_digest = command.canonical_digest();
    command
}
fn launch(id: &str) -> MemberCommand {
    use capyctl_domain::group::{
        member_id, GroupEngine, GroupPlan, GroupTopology, MemberPlan, MemberRole,
    };
    // ADR 0028 §6: a group plan records the checkpoint digest.
    const DIGEST: &str = "sha256:abababababababababababababababababababababababababababababababab";
    let golden: serde_json::Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    let mut c = command(id);
    c.action = MemberAction::Launch {
        // ADR 0028 §8 (ruling R29): this host's own member launch.
        member: capyctl_protocol::execution::SingleLaunchPlan {
            deployment_config: golden["input"]["deployment"].to_string(),
            profile_name: "controlled-child".into(),
            checkpoint_fingerprint: "sha256:model".into(),
            host_policy_fingerprint: "a".repeat(64),
            binding_id: "01K00000000000000000000001".into(),
            incarnation: "01K00000000000000000000002".into(),
            grant_id: "01K00000000000000000000003".into(),
            service_port: 31000,
            issued_at_ms: 1,
            coordinator_session_id: "01K00000000000000000000004".into(),
            checkpoint_digest: DIGEST.into(),
            checkpoint_weights_bytes: None,
            checkpoint_state_slot_bytes: None,
            startup_bytes: None,
        },
        plan: GroupPlan::new(
            GroupEngine::Vllm,
            (0..2)
                .map(|rank| MemberPlan {
                    member: MemberKey {
                        host_id: if rank == 0 { "host" } else { "peer" }.into(),
                        member_id: member_id(rank),
                    },
                    rank,
                    role: if rank == 0 {
                        MemberRole::Head
                    } else {
                        MemberRole::Worker
                    },
                    profile_name: "controlled-child".into(),
                    profile_fingerprint: "approved".into(),
                    checkpoint_fingerprint: DIGEST.into(),
                    model_path: "/models/m".into(),
                    devices: vec!["gpu:0".into()],
                    peer_address: format!("10.0.0.{}", rank + 1).parse().unwrap(),
                    service_port: (rank == 0).then_some(31000),
                    worker_port: None,
                })
                .collect(),
            GroupTopology {
                tensor_parallel: 2,
                pipeline_parallel: 1,
                local_ranks: 1,
            },
            32000,
            1,
        )
        .unwrap(),
    };
    sign(c)
}
struct ChildPolicy {
    marker: std::path::PathBuf,
}
impl LocalExecutionPolicy for ChildPolicy {
    fn authorize(&self, c: &MemberCommand) -> Result<(), JournalError> {
        if c.identity.profile_fingerprint != "approved" {
            return Err(JournalError::Unauthorized);
        }
        Ok(())
    }
    fn render_launch(
        &self,
        _: &MemberCommand,
    ) -> Result<capyctl_agent::journal::ApprovedLaunch, JournalError> {
        // Test-only approved renderer. Production authority must also verify a grant.
        Ok(capyctl_agent::journal::ApprovedLaunch {
            command: capyctl_adapters::traits::RenderedCommand {
                argv: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "printf x >> \"$1\"; exec sleep 60".into(),
                    "controlled-child".into(),
                    self.marker.to_str().unwrap().into(),
                ],
                env: Default::default(),
            },
            descriptors: None,
        })
    }
}
fn stop(
    j: &std::sync::Arc<HostJournal>,
    session: u64,
    handle: &str,
    policy: &dyn LocalExecutionPolicy,
) {
    let mut c = command(&format!("stop-{handle}"));
    c.action = MemberAction::Terminate {
        owned_handle: handle.into(),
        recorded: Vec::new(),
    };
    let c = sign(c);
    let ticket = fresh(j.accept(session, &c, 10, policy).unwrap());
    j.execute(ticket, 10, policy).unwrap();
}
// T09 T12 T33: effect succeeded but acknowledgement was lost; restart reconciles
// the durable gated child identity, never launches a replacement.
#[test]
fn lost_launch_ack_and_restart_keep_one_owned_process() {
    let d = directory();
    let policy = ChildPolicy {
        marker: d.path().join("launch-count"),
    };
    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    let s = j.connect().unwrap();
    let c = launch("launch");
    let ticket = fresh(j.accept(s, &c, 10, &policy).unwrap());
    j.execute(ticket, 10, &policy).unwrap();
    for _ in 0..100 {
        if policy.marker.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(std::fs::read(&policy.marker).unwrap(), b"x");
    j.disconnect(s).unwrap();
    let before = j.inspect_owned("launch").unwrap();
    let _cleanup = ChildCleanup(before[0].0.clone());
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].1, capyctl_domain::completion::Presence::Alive);
    let lifetime = std::sync::Arc::downgrade(&j);
    drop(j);
    assert!(
        lifetime.upgrade().is_none(),
        "journal and its exclusive lock must drop"
    );
    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    let s = j.connect().unwrap();
    let replay = j.accept(s, &c, 2000, &policy).unwrap();
    assert!(matches!(replay,Acceptance::Replay(ref r) if r.claim_retained && r.processes.len()==1));
    assert_eq!(j.inspect_owned("launch").unwrap()[0].0, before[0].0);
    assert!(matches!(
        j.accept(s, &launch("competing"), 10, &policy),
        Err(JournalError::Uncertain)
    ));
    assert_eq!(std::fs::read(&policy.marker).unwrap(), b"x");
    stop(&j, s, "launch", &policy);
    assert!(!j.history(0, 100).unwrap()[0].claim_retained);
    assert_eq!(
        j.inspect_owned("launch").unwrap()[0].1,
        capyctl_domain::completion::Presence::Gone
    );
}
// T13 T33: dropped execution capability is ambiguous, not permission to retry.
#[test]
fn accepted_unexecuted_launch_survives_restart_and_retains_claim() {
    let d = directory();
    let policy = ChildPolicy {
        marker: d.path().join("never"),
    };
    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    let s = j.connect().unwrap();
    let c = launch("launch");
    drop(fresh(j.accept(s, &c, 10, &policy).unwrap()));
    drop(j);
    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    let s = j.connect().unwrap();
    assert!(
        matches!(j.accept(s,&c,10,&policy).unwrap(),Acceptance::Replay(ref r) if r.claim_retained && r.processes.is_empty())
    );
    assert!(j.accept(s, &launch("next"), 10, &policy).is_err());
    assert!(!policy.marker.exists());
    assert_eq!(j.compact_completed(i64::MAX).unwrap(), 0);
}
// T13 T34: deadline/session/generation are checked again immediately before effects.
#[test]
fn queued_expired_or_fenced_ticket_never_starts() {
    for reason in ["deadline", "session", "generation", "disconnect"] {
        let d = directory();
        let policy = ChildPolicy {
            marker: d.path().join("never"),
        };
        let j = HostJournal::open(d.path(), "controller", "host").unwrap();
        let s = j.connect().unwrap();
        let ticket = fresh(j.accept(s, &launch("launch"), 10, &policy).unwrap());
        match reason {
            "session" => {
                j.connect().unwrap();
            }
            "disconnect" => j.disconnect(s).unwrap(),
            "generation" => {
                let mut c = command("new-generation");
                c.identity.generation = 2;
                j.accept(s, &sign(c), 10, &policy).unwrap();
            }
            _ => (),
        }
        assert!(j
            .execute(
                ticket,
                if reason == "deadline" { 1000 } else { 10 },
                &policy
            )
            .is_err());
        assert!(!policy.marker.exists());
        assert!(j.history(0, 100).unwrap()[0].claim_retained);
    }
}
// T09 T13: operation steps are independent and durable tombstones reject replay
// after completed payload compaction without exposing unbounded resume replies.
#[test]
fn independent_steps_replay_after_compaction_without_new_effects() {
    let d = directory();
    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    let s = j.connect().unwrap();
    for id in ["one", "two"] {
        let c = command(id);
        let t = fresh(j.accept(s, &c, 10, &Policy).unwrap());
        j.execute(t, 10, &Policy).unwrap();
    }
    assert_eq!(j.history(0, 1).unwrap().len(), 1);
    assert!(j.history(0, 257).is_err());
    assert_eq!(j.compact_completed(i64::MAX).unwrap(), 2);
    assert!(
        matches!(j.accept(s,&command("one"),2000,&Policy).unwrap(),Acceptance::Replay(ref r) if r.state==capyctl_agent::journal::CommandState::Tombstone)
    );
    let mut duplicate = command("new-id");
    duplicate.identity.step_id = "one".into();
    assert!(matches!(
        j.accept(s, &sign(duplicate), 10, &Policy),
        Err(JournalError::Conflict)
    ));
}
// T34: no bootstrap or generated wire object confers local grant authority.
#[test]
fn authority_and_schema_fail_closed() {
    let d = directory();
    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    let s = j.connect().unwrap();
    let policy = ChildPolicy {
        marker: d.path().join("never"),
    };
    let mut c = command("wrong-host");
    c.identity.member.host_id = "peer".into();
    assert!(j.accept(s, &sign(c), 10, &policy).is_err());
    let mut c = command("wrong-controller");
    c.identity.controller_id = "peer".into();
    assert!(j.accept(s, &sign(c), 10, &policy).is_err());
    let mut c = command("no-grant");
    c.identity.profile_fingerprint = "unapproved".into();
    assert!(j.accept(s, &sign(c), 10, &policy).is_err());
    let mut c = command("tamper");
    c.identity.revision = 2;
    assert!(j.accept(s, &c, 10, &policy).is_err());
    let mut c = command("unsupported");
    c.action = MemberAction::CloseIngress;
    assert!(j.accept(s, &sign(c), 10, &policy).is_err());
    assert!(j.history(0, 100).unwrap().is_empty());
}
// T34: SQLite must not open symlinked, shared, or public journal leaves.
#[test]
fn unsafe_database_and_sidecars_are_rejected() {
    for leaf in [
        "commands.sqlite",
        "commands.sqlite-journal",
        "commands.sqlite-wal",
        "commands.sqlite-shm",
    ] {
        let d = directory();
        std::fs::write(d.path().join(leaf), b"unsafe").unwrap();
        std::fs::set_permissions(d.path().join(leaf), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        assert!(HostJournal::open(d.path(), "controller", "host").is_err());
    }
    let d = directory();
    std::os::unix::fs::symlink("/dev/null", d.path().join("commands.sqlite")).unwrap();
    assert!(HostJournal::open(d.path(), "controller", "host").is_err());
}

// T13 T34: process-local tickets cannot cross journals even for equal wire IDs.
#[test]
fn ticket_is_bound_to_the_journal_that_accepted_it() {
    let a = directory();
    let b = directory();
    let first = HostJournal::open(a.path(), "controller", "host").unwrap();
    let second = HostJournal::open(b.path(), "controller", "host").unwrap();
    let sa = first.connect().unwrap();
    let sb = second.connect().unwrap();
    let c = command("same-id");
    let ticket = fresh(first.accept(sa, &c, 10, &Policy).unwrap());
    drop(fresh(second.accept(sb, &c, 10, &Policy).unwrap()));
    assert!(second.execute(ticket, 10, &Policy).is_err());
    assert_eq!(
        second.history(0, 100).unwrap()[0].state,
        capyctl_agent::journal::CommandState::Accepted
    );
}

// T33 T34: partial journals never silently reinitialize lost identity or claims.
#[test]
fn partial_journal_is_not_repaired_or_rebound() {
    for damage in [
        "DELETE FROM authority",
        "DROP TABLE commands",
        "PRAGMA user_version=0",
    ] {
        let d = directory();
        let j = HostJournal::open(d.path(), "controller", "host").unwrap();
        drop(j);
        let db = rusqlite::Connection::open(d.path().join("commands.sqlite")).unwrap();
        db.execute_batch(damage).unwrap();
        drop(db);
        assert!(
            HostJournal::open(d.path(), "controller", "host").is_err(),
            "{damage}"
        );
    }
}

// T12 T33: a recorded PID with changed start identity cannot be adopted or killed.
#[test]
fn reused_pid_never_becomes_owned_through_inspection() {
    let d = directory();
    let policy = ChildPolicy {
        marker: d.path().join("launched"),
    };
    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    let s = j.connect().unwrap();
    let ticket = fresh(j.accept(s, &launch("launch"), 10, &policy).unwrap());
    j.execute(ticket, 10, &policy).unwrap();
    let real = j.inspect_owned("launch").unwrap()[0].0.clone();
    let _cleanup = ChildCleanup(real.clone());
    drop(j);
    // Deliberately corrupt start identity to simulate persisted PID reuse.
    let db = rusqlite::Connection::open(d.path().join("commands.sqlite")).unwrap();
    db.execute("UPDATE processes SET ticks=ticks-1", [])
        .unwrap();
    drop(db);
    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    let s = j.connect().unwrap();
    assert_eq!(
        j.inspect_owned("launch").unwrap()[0].1,
        capyctl_domain::completion::Presence::Gone
    );
    let ticket = fresh(j.accept(s, &command("inspect"), 10, &policy).unwrap());
    assert!(j.execute(ticket, 10, &policy).is_err());
    assert_eq!(j.history(0, 100).unwrap()[0].processes.len(), 1);
    let mut c = command("reject-stop");
    c.action = MemberAction::Terminate {
        owned_handle: "launch".into(),
        recorded: Vec::new(),
    };
    let c = sign(c);
    let ticket = fresh(j.accept(s, &c, 10, &policy).unwrap());
    assert!(j.execute(ticket, 10, &policy).is_err());
    assert_eq!(
        capyctl_launchers::process_absence::presence(&real),
        capyctl_domain::completion::Presence::Alive
    );
    assert!(j.history(0, 100).unwrap()[0].claim_retained);
    drop(j);
    let db = rusqlite::Connection::open(d.path().join("commands.sqlite")).unwrap();
    db.execute("UPDATE processes SET ticks=ticks+1", [])
        .unwrap();
    drop(db);
    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    let s = j.connect().unwrap();
    stop(&j, s, "launch", &policy);
}

// T13 T33: a lost database cannot reset an established host's retained claims.
#[test]
fn missing_established_database_cannot_reinitialize() {
    let d = directory();
    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    let s = j.connect().unwrap();
    let policy = ChildPolicy {
        marker: d.path().join("never"),
    };
    j.accept(s, &launch("retained"), 10, &policy).unwrap();
    drop(j);
    std::fs::remove_file(d.path().join("commands.sqlite")).unwrap();
    assert!(HostJournal::open(d.path(), "controller", "host").is_err());
}

struct TestClock(std::sync::atomic::AtomicI64);
impl capyctl_agent::journal::ExecutionClock for TestClock {
    fn now_ms(&self) -> Result<i64, JournalError> {
        Ok(self.0.load(std::sync::atomic::Ordering::SeqCst))
    }
}
struct DelayedPolicy {
    child: ChildPolicy,
    clock: std::sync::Arc<TestClock>,
}
impl LocalExecutionPolicy for DelayedPolicy {
    fn authorize(&self, c: &MemberCommand) -> Result<(), JournalError> {
        self.child.authorize(c)
    }
    fn render_launch(
        &self,
        c: &MemberCommand,
    ) -> Result<capyctl_agent::journal::ApprovedLaunch, JournalError> {
        self.clock
            .0
            .store(1000, std::sync::atomic::Ordering::SeqCst);
        self.child.render_launch(c)
    }
}
// T13 T34: local preparation cannot turn an expired command into a late effect.
#[test]
fn deadline_is_rechecked_after_local_rendering() {
    let d = directory();
    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    let s = j.connect().unwrap();
    let clock = std::sync::Arc::new(TestClock(std::sync::atomic::AtomicI64::new(10)));
    let policy = DelayedPolicy {
        child: ChildPolicy {
            marker: d.path().join("never"),
        },
        clock: clock.clone(),
    };
    let ticket = fresh(j.accept(s, &launch("launch"), 10, &policy).unwrap());
    assert!(matches!(
        j.execute_with_clock(ticket, clock, &policy),
        Err(JournalError::Fenced)
    ));
    assert!(!policy.child.marker.exists());
    let record = &j.history(0, 100).unwrap()[0];
    assert!(record.claim_retained);
    assert!(record.processes.is_empty());
}

// T09 / T33: native adapters receive a one-shot journal-backed launch capability;
// neither duplicate delivery nor disconnect can authorize another child.
#[test]
fn adapter_launch_tools_preserve_gate_ownership_and_disconnect_fence() {
    let d = directory();
    let policy = std::sync::Arc::new(ChildPolicy {
        marker: d.path().join("adapter-launch-count"),
    });
    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    let s = j.connect().unwrap();
    let c = launch("adapter-launch");
    let ticket = fresh(j.accept(s, &c, 10, policy.as_ref()).unwrap());
    let tools = j.launch_tools(ticket, 10, policy.clone()).unwrap();
    let rendered = policy.render_launch(&c).unwrap();
    let api = tools
        .spawn_durable(
            &c.action.launch_plan().unwrap().incarnation,
            &rendered.command,
        )
        .unwrap();
    let _cleanup = ChildCleanup(api.clone());
    assert_eq!(j.inspect_owned("adapter-launch").unwrap()[0].0, api);
    assert!(tools
        .spawn_durable(
            &c.action.launch_plan().unwrap().incarnation,
            &rendered.command
        )
        .is_err());
    assert!(matches!(
        j.accept(s, &c, 10, policy.as_ref()).unwrap(),
        Acceptance::Replay(_)
    ));
    drop(tools);
    stop(&j, s, "adapter-launch", policy.as_ref());
    let c = launch("fenced-adapter");
    let ticket = fresh(j.accept(s, &c, 10, policy.as_ref()).unwrap());
    let tools = j.launch_tools(ticket, 10, policy.clone()).unwrap();
    j.disconnect(s).unwrap();
    assert!(tools
        .spawn_durable("fenced-adapter", &rendered.command)
        .is_err());
    assert!(j.inspect_owned("fenced-adapter").unwrap().is_empty());
    assert!(j
        .history(0, 100)
        .unwrap()
        .iter()
        .any(|r| r.command_id == "fenced-adapter" && r.claim_retained));
}

// T41 (ADR 0023 §3): the evidence a TensorFold build lock is cleared on is
// every process recorded for a launch this host still claims. A claimed
// launch's running child keeps the lock; once the launch is stopped and its
// claim released, a lock it left is cleared.
#[test]
fn a_claimed_launchs_running_child_keeps_a_build_lock() {
    use std::os::unix::fs::PermissionsExt;
    let d = directory();
    let policy = std::sync::Arc::new(ChildPolicy {
        marker: d.path().join("build-lock-launch"),
    });
    let cache = d.path().join("torch_extensions");
    std::fs::create_dir(&cache).unwrap();
    std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::create_dir(cache.join("tensorfold_qmm_v5")).unwrap();
    let lock = cache.join("tensorfold_qmm_v5/lock");
    std::fs::write(&lock, "").unwrap();
    let clear = |j: &HostJournal| {
        j.with_claimed_processes(|recorded| {
            capyctl_agent::engine_cache::clear_stale_locks(&cache, recorded)
        })
        .unwrap()
    };
    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    let s = j.connect().unwrap();
    let c = launch("building");
    let ticket = fresh(j.accept(s, &c, 10, policy.as_ref()).unwrap());
    let tools = j.launch_tools(ticket, 10, policy.clone()).unwrap();
    let rendered = policy.render_launch(&c).unwrap();
    let incarnation = &c.action.launch_plan().unwrap().incarnation;
    let api = tools.spawn_durable(incarnation, &rendered.command).unwrap();
    let _cleanup = ChildCleanup(api.clone());
    assert_eq!(j.with_claimed_processes(|p| p.to_vec()).unwrap(), vec![api]);
    assert_eq!(clear(&j), 0);
    assert!(lock.exists(), "a launch that may be building holds it");
    drop(tools);
    stop(&j, s, "building", policy.as_ref());
    assert_eq!(clear(&j), 1);
    assert!(!lock.exists());
}

// T16 / T33: durable model evidence remains tied to the exact owned launch;
// reading current process presence cannot refresh an old native readiness probe.
#[tokio::test]
async fn native_result_replay_retains_probe_timestamp_and_rejects_forged_identity() {
    use capyctl_domain::completion::{EffectObservation, Milestone, TransitionToken};
    use capyctl_protocol::execution::SingleLaunchPlan;
    struct Workers;
    impl LocalExecutionPolicy for Workers {
        fn authorize(&self, _: &MemberCommand) -> Result<(), JournalError> {
            Ok(())
        }
        fn render_launch(
            &self,
            _: &MemberCommand,
        ) -> Result<capyctl_agent::journal::ApprovedLaunch, JournalError> {
            Ok(capyctl_agent::journal::ApprovedLaunch {
                command: capyctl_adapters::traits::RenderedCommand {
                    argv: vec!["/bin/sh".into(), "-c".into(), "sleep 60 & wait".into()],
                    env: Default::default(),
                },
                descriptors: None,
            })
        }
    }
    let d = directory();
    let journal = HostJournal::open(d.path(), "controller", "host").unwrap();
    let session = journal.connect().unwrap();
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    let mut c = command("native-once");
    c.identity.deadline_ms = 30_000;
    let binding = "01K00000000000000000000001".to_owned();
    let incarnation = "01K00000000000000000000002".to_owned();
    c.action = MemberAction::LaunchSingle(SingleLaunchPlan {
        deployment_config: fixture["input"]["deployment"].to_string(),
        profile_name: "profile".into(),
        checkpoint_fingerprint: "sha256:model".into(),
        host_policy_fingerprint: "a".repeat(64),
        binding_id: binding.clone(),
        incarnation: incarnation.clone(),
        grant_id: "01K00000000000000000000003".into(),
        service_port: 30000,
        issued_at_ms: 1,
        coordinator_session_id: "01K00000000000000000000004".into(),
        checkpoint_digest: String::new(),
        checkpoint_weights_bytes: None,
        checkpoint_state_slot_bytes: None,
        startup_bytes: None,
    });
    let c = sign(c);
    let policy = std::sync::Arc::new(Workers);
    let ticket = fresh(journal.accept(session, &c, 10, policy.as_ref()).unwrap());
    let tools = journal.launch_tools(ticket, 10, policy.clone()).unwrap();
    let api = tools
        .spawn_durable(&incarnation, &policy.render_launch(&c).unwrap().command)
        .unwrap();
    let _cleanup = ChildCleanup(api.clone());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let processes = loop {
        let observed = tools.observe_group(&api).unwrap();
        if observed.len() >= 2 {
            break observed;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    };
    let mut observation = EffectObservation {
        token: TransitionToken {
            deployment_id: c.identity.deployment_id.clone(),
            operation_id: c.identity.operation_id.clone(),
            step_id: c.identity.step_id.clone(),
            revision: 1,
            generation: 1,
        },
        binding_id: binding,
        incarnation,
        identities: processes,
        observed_at_ms: 20,
        receipt: "controlled adapter probe".into(),
        facts: vec![
            Milestone::AllocationsRestored,
            Milestone::WeightsUsable,
            Milestone::CacheValid,
            Milestone::ModelUsable,
        ],
        // ADR 0014 amendment A12: the builds the adapter saw travel with it.
        kernel_builds: vec![capyctl_domain::completion::KernelBuild {
            from_ms: 12,
            until_ms: 18,
        }],
    };
    observation.identities[0].start_ticks += 1;
    assert!(journal
        .record_launch_ready(session, "native-once", &observation)
        .is_err());
    observation.identities[0].start_ticks -= 1;
    // SPEC §13.2 / T13: readiness evidence is written under the transition
    // lock for the session that is still connected; a stale one records none.
    assert!(matches!(
        journal.record_launch_ready(session + 1, "native-once", &observation),
        Err(JournalError::Fenced)
    ));
    assert!(
        !journal
            .execution_result("native-once", 30)
            .unwrap()
            .model_usable
    );
    journal
        .record_launch_ready(session, "native-once", &observation)
        .unwrap();
    let replay = journal.execution_result("native-once", 500).unwrap();
    assert!(replay.model_usable && replay.claim_retained);
    assert_eq!(replay.observed_at_unix_ms, 20);
    assert_eq!(
        replay
            .kernel_builds
            .iter()
            .map(|b| (b.from_unix_ms, b.until_unix_ms))
            .collect::<Vec<_>>(),
        [(12, 18)]
    );
    // The product executor must expose an expired probe as retained uncertainty,
    // even while the exact recorded processes are still alive after reconnect.
    let private = directory();
    let identities = capyctl_agent::ingress_identity::IngressIdentities::new(
        capyctl_agent::identity_storage::IdentityDirectory::open(private.path()).unwrap(),
    );
    let host_config = capyctl_config::remote_roles::HostConfig::parse(
        &capyctl_config::remote_roles::HostConfig::template(d.path()),
    )
    .unwrap();
    let executor = capyctl_agent::native_execution::NativeHostExecution::new(
        journal.clone(),
        capyctl_agent::ingress::Ingress::new().unwrap(),
        identities,
        host_config,
        "host".into(),
        "controller".into(),
        d.path().join("runtime"),
        d.path().join("logs"),
        Default::default(),
    );
    use capyctl_agent::session::SessionExecution;
    let expired = executor.execute(session, c.clone()).await.unwrap();
    assert_eq!(expired.state, "launched");
    assert!(expired.claim_retained && !expired.model_usable);
    assert!(expired.processes.iter().all(|p| p.presence == "alive"));
    assert!(!d.path().join("runtime").exists());
    // Reporting current ownership did not overwrite the historical model probe.
    assert_eq!(
        journal
            .execution_result("native-once", 600)
            .unwrap()
            .observed_at_unix_ms,
        20
    );
    drop(tools);
    stop(&journal, session, "native-once", policy.as_ref());
    assert!(
        !journal
            .execution_result("native-once", 600)
            .unwrap()
            .model_usable
    );
}

fn terminate(id: &str, handle: &str) -> MemberCommand {
    let mut c = command(id);
    c.identity.expected_state = "retained".into();
    c.action = MemberAction::Terminate {
        owned_handle: handle.into(),
        recorded: Vec::new(),
    };
    sign(c)
}

// G1 (U5 live) T13 T32 T33: an accepted launch whose gate never released a
// process can be settled by Terminate. The gate protocol is the evidence: no
// engine executes before its identity is journaled, so a claim with no journaled
// process and no running effect owns nothing. Settling it fences the launch for
// good: its dropped ticket, and a spawn racing behind it, can never start one.
#[test]
fn terminate_settles_a_launch_that_never_released_a_process() {
    let d = directory();
    let policy = std::sync::Arc::new(ChildPolicy {
        marker: d.path().join("never"),
    });
    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    let s = j.connect().unwrap();
    // Accepted, never attempted (the agent died before starting the effect).
    drop(fresh(
        j.accept(s, &launch("accepted"), 10, policy.as_ref())
            .unwrap(),
    ));
    let settle = terminate("settle-accepted", "accepted");
    let ticket = fresh(j.accept(s, &settle, 10, policy.as_ref()).unwrap());
    j.execute(ticket, 10, policy.as_ref()).unwrap();
    let result = j.execution_result("settle-accepted", 20).unwrap();
    assert_eq!(result.state, "completed");
    assert!(!result.claim_retained);
    assert!(result.processes.is_empty());
    assert_eq!(result.owned_handle, "accepted");
    capyctl_protocol::execution::validate_result(&settle, &result).unwrap();

    // Attempted, with launch tools in hand but no spawn yet.
    let c = launch("attempted");
    let ticket = fresh(j.accept(s, &c, 10, policy.as_ref()).unwrap());
    let tools = j.launch_tools(ticket, 10, policy.clone()).unwrap();
    let ticket = fresh(
        j.accept(
            s,
            &terminate("settle-attempted", "attempted"),
            10,
            policy.as_ref(),
        )
        .unwrap(),
    );
    j.execute(ticket, 10, policy.as_ref()).unwrap();
    let rendered = policy.render_launch(&c).unwrap();
    assert!(tools.spawn_durable("attempted", &rendered.command).is_err());
    assert!(!policy.marker.exists(), "a settled launch must never spawn");
    assert!(j.history(0, 100).unwrap().iter().all(|r| !r.claim_retained));
    // The settled claim no longer blocks the host's single launch slot.
    fresh(j.accept(s, &launch("next"), 10, policy.as_ref()).unwrap());
}

// G1 T09 T34: a Terminate for a launch this host never accepted is authenticated
// evidence that nothing of it runs here, and it fences that launch permanently,
// so a delivery still in flight can never start it afterwards.
#[test]
fn terminate_of_an_unknown_launch_fences_it_permanently() {
    let d = directory();
    let policy = ChildPolicy {
        marker: d.path().join("never"),
    };
    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    let s = j.connect().unwrap();
    for id in ["fence-unknown", "fence-unknown-again"] {
        let settle = terminate(id, "never-accepted");
        let ticket = fresh(j.accept(s, &settle, 10, &policy).unwrap());
        j.execute(ticket, 10, &policy).unwrap();
        let result = j.execution_result(id, 20).unwrap();
        assert_eq!(result.state, "completed");
        assert!(!result.claim_retained && result.processes.is_empty());
        capyctl_protocol::execution::validate_result(&settle, &result).unwrap();
    }
    assert!(matches!(
        j.accept(s, &launch("never-accepted"), 10, &policy),
        Err(JournalError::Conflict)
    ));
    assert!(!policy.marker.exists());
}

// W4 T33: a version-2 journal gains the residency table in place, keeping every
// command and claim; a version-3 journal missing it is corrupt, never repaired.
#[test]
fn residency_schema_migrates_forward_and_is_never_recreated() {
    let d = directory();
    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    let s = j.connect().unwrap();
    drop(fresh(j.accept(s, &command("kept"), 10, &Policy).unwrap()));
    drop(j);
    let db = rusqlite::Connection::open(d.path().join("commands.sqlite")).unwrap();
    db.execute_batch(&format!(
        "{V4_SHAPE} DROP TABLE residency; PRAGMA user_version=2;"
    ))
    .unwrap();
    drop(db);
    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    assert_eq!(j.history(0, 10).unwrap().len(), 1);
    assert_eq!(j.residency_of("kept").unwrap(), None);
    drop(j);
    let db = rusqlite::Connection::open(d.path().join("commands.sqlite")).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        capyctl_agent::journal::JOURNAL_SCHEMA_VERSION
    );
    db.execute_batch("DROP TABLE residency").unwrap();
    drop(db);
    assert!(HostJournal::open(d.path(), "controller", "host").is_err());
}

// W4 T21 T34: Park and Restore are refused before they are journaled unless the
// policy admits the named launch's residency; there is no default allow.
#[test]
fn residency_commands_need_an_owned_launch_and_explicit_policy() {
    let d = directory();
    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    let s = j.connect().unwrap();
    for action in [
        MemberAction::Park {
            owned_handle: "never-launched".into(),
        },
        MemberAction::Restore {
            owned_handle: "never-launched".into(),
            checkpoint_digest: String::new(),
        },
    ] {
        let mut c = command("residency");
        c.action = action;
        assert!(matches!(
            j.accept(s, &sign(c), 10, &Policy),
            Err(JournalError::Unauthorized)
        ));
    }
    assert!(j.history(0, 10).unwrap().is_empty());
}

/// Rewrites a current journal's fencing tables exactly as a version-4 agent
/// left them (one assignment per deployment, no per-command instance), so a
/// test can reopen it as that older version.
const V4_SHAPE: &str = "CREATE TABLE assignments_v4(deployment TEXT PRIMARY KEY,generation INTEGER NOT NULL,revision INTEGER NOT NULL);
    INSERT INTO assignments_v4 SELECT deployment,generation,revision FROM assignments WHERE instance=0;
    DROP TABLE assignments; ALTER TABLE assignments_v4 RENAME TO assignments;
    ALTER TABLE commands DROP COLUMN instance;";

/// A launch of `deployment` at `generation`, otherwise as `launch`.
fn launch_of(id: &str, deployment: &str, generation: i64) -> MemberCommand {
    let mut c = launch(id);
    c.identity.deployment_id = deployment.into();
    c.identity.generation = generation;
    sign(c)
}

/// A host policy that keeps one claim per launch and admits a new launch
/// beside at most `room` others (its own budget), refusing beyond that.
struct PerLaunch {
    child: ChildPolicy,
    room: usize,
    seen: std::sync::Mutex<Vec<Vec<(String, capyctl_agent::journal::ClaimPhase)>>>,
}
impl LocalExecutionPolicy for PerLaunch {
    fn authorize(&self, c: &MemberCommand) -> Result<(), JournalError> {
        self.child.authorize(c)
    }
    fn render_launch(
        &self,
        c: &MemberCommand,
    ) -> Result<capyctl_agent::journal::ApprovedLaunch, JournalError> {
        self.child.render_launch(c)
    }
    fn admit_beside(
        &self,
        command: &MemberCommand,
        claimed: &[capyctl_agent::journal::ClaimedLaunch],
    ) -> Result<(), JournalError> {
        assert!(claimed
            .iter()
            .all(|c| c.command.identity.command_id != command.identity.command_id));
        self.seen.lock().unwrap().push(
            claimed
                .iter()
                .map(|c| (c.command.identity.command_id.clone(), c.phase))
                .collect(),
        );
        if claimed.len() < self.room {
            Ok(())
        } else {
            Err(JournalError::Unauthorized)
        }
    }
}

// SPEC §§3.1, 7.3, 13.1 (per-launch claims) T24 T27 T33 T34: launches of
// different deployments each keep their own claim; the policy sees every
// other retained claim and may refuse (nothing is journaled then); a second
// launch of one instance incarnation stays uncertain; a Terminate settles only
// the launch it names; a restart adopts every retained claim.
#[test]
fn per_launch_claims_are_independent_and_survive_restart() {
    use capyctl_agent::journal::ClaimPhase;
    let d = directory();
    let policy = PerLaunch {
        child: ChildPolicy {
            marker: d.path().join("never"),
        },
        room: 2,
        seen: Default::default(),
    };
    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    let s = j.connect().unwrap();
    drop(fresh(
        j.accept(s, &launch_of("a", "deployment-a", 1), 10, &policy)
            .unwrap(),
    ));
    drop(fresh(
        j.accept(s, &launch_of("b", "deployment-b", 4), 10, &policy)
            .unwrap(),
    ));
    assert_eq!(
        *policy.seen.lock().unwrap(),
        vec![vec![], vec![("a".to_string(), ClaimPhase::Starting)]]
    );
    // The host's own budget is full: a typed refusal, nothing journaled.
    assert!(matches!(
        j.accept(s, &launch_of("c", "deployment-c", 1), 10, &policy),
        Err(JournalError::Unauthorized)
    ));
    // One claim per instance incarnation (deployment, generation).
    let loose = PerLaunch {
        room: usize::MAX,
        ..policy
    };
    assert!(matches!(
        j.accept(s, &launch_of("a-again", "deployment-a", 1), 10, &loose),
        Err(JournalError::Uncertain)
    ));
    let history = j.history(0, 100).unwrap();
    assert_eq!(
        history
            .iter()
            .map(|r| r.command_id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert!(history.iter().all(|r| r.claim_retained));
    drop(j);

    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    let s = j.connect().unwrap();
    let claimed = j.claimed_launches("").unwrap();
    assert_eq!(claimed.len(), 2, "a restart adopts both retained claims");
    let mut settle = terminate("settle-a", "a");
    settle.identity.deployment_id = "deployment-a".into();
    let settle = sign(settle);
    let ticket = fresh(j.accept(s, &settle, 10, &loose).unwrap());
    j.execute(ticket, 10, &loose).unwrap();
    let claimed = j.claimed_launches("").unwrap();
    assert_eq!(
        claimed
            .iter()
            .map(|c| c.command.identity.command_id.as_str())
            .collect::<Vec<_>>(),
        ["b"],
        "only the named launch was settled"
    );
    // Its instance may launch again under the same generation once settled.
    drop(fresh(
        j.accept(s, &launch_of("a-next", "deployment-a", 1), 10, &loose)
            .unwrap(),
    ));
    assert!(!loose.child.marker.exists());
}

// SPEC §§13.1, 13.3 (per-launch claims migration) T33 T34: a version-3
// journal, whose one host-wide claim index allowed a single launch, migrates
// in place to version 4; its retained launch is adopted as its own claim and
// keeps every fence. A version-4 journal missing the per-instance index, or
// still carrying the host-wide one, is corrupt and never repaired.
#[test]
fn a_single_claim_journal_migrates_and_adopts_its_retained_launch() {
    let d = directory();
    let policy = ChildPolicy {
        marker: d.path().join("never"),
    };
    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    let s = j.connect().unwrap();
    let retained = launch("retained");
    drop(fresh(j.accept(s, &retained, 10, &policy).unwrap()));
    drop(j);
    // Rewrite it exactly as a version-3 agent left it.
    let db = rusqlite::Connection::open(d.path().join("commands.sqlite")).unwrap();
    db.execute_batch(&format!(
        "{V4_SHAPE} DROP INDEX one_claim_per_instance;
         CREATE UNIQUE INDEX one_host_claim ON commands(claim) WHERE claim=1;
         PRAGMA user_version=3;"
    ))
    .unwrap();
    drop(db);

    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    let s = j.connect().unwrap();
    let claimed = j.claimed_launches("").unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].command.identity.command_id, "retained");
    assert!(matches!(
        j.accept(s, &retained, 10, &policy).unwrap(),
        Acceptance::Replay(ref r) if r.claim_retained
    ));
    // A single-claim policy (the default) still refuses a second launch.
    assert!(matches!(
        j.accept(s, &launch_of("second", "other", 1), 10, &policy),
        Err(JournalError::Uncertain)
    ));
    drop(j);
    let db = rusqlite::Connection::open(d.path().join("commands.sqlite")).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        capyctl_agent::journal::JOURNAL_SCHEMA_VERSION
    );
    let indexes: Vec<String> = db
        .prepare(
            "SELECT name FROM sqlite_schema WHERE type='index' AND name LIKE 'one_%' ORDER BY name",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(indexes, ["one_claim_per_instance"]);
    drop(db);

    for damage in [
        // Control: an undamaged copy opens.
        "",
        "DROP INDEX one_claim_per_instance",
        "CREATE UNIQUE INDEX one_host_claim ON commands(claim) WHERE claim=1",
    ] {
        let copy = directory();
        for leaf in ["commands.sqlite", "journal-established"] {
            let from = d.path().join(leaf);
            if from.exists() {
                std::fs::copy(&from, copy.path().join(leaf)).unwrap();
            }
        }
        let db = rusqlite::Connection::open(copy.path().join("commands.sqlite")).unwrap();
        db.execute_batch(damage).unwrap();
        drop(db);
        assert_eq!(
            HostJournal::open(copy.path(), "controller", "host").is_ok(),
            damage.is_empty(),
            "{damage}"
        );
    }
}

// T34 T33: SPEC §13.1. Compaction drops a settled launch's payload but not its
// owner: a Terminate from another deployment or member naming a compacted
// launch is refused, and learns nothing of its processes; its own deployment's
// Terminate is still answered from the journaled identities.
#[test]
fn terminate_of_a_compacted_launch_checks_its_owner() {
    let d = directory();
    let policy = ChildPolicy {
        marker: d.path().join("launched"),
    };
    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    let s = j.connect().unwrap();
    let ticket = fresh(j.accept(s, &launch("launch"), 10, &policy).unwrap());
    j.execute(ticket, 10, &policy).unwrap();
    stop(&j, s, "launch", &policy);
    assert!(j.compact_completed(i64::MAX).unwrap() >= 1);
    let mut foreign = command("foreign-stop");
    foreign.identity.deployment_id = "other".into();
    foreign.identity.expected_state = "retained".into();
    foreign.action = MemberAction::Terminate {
        owned_handle: "launch".into(),
        recorded: Vec::new(),
    };
    let foreign = sign(foreign);
    let refused = j
        .accept(s, &foreign, 10, &policy)
        .and_then(|acceptance| j.execute(fresh(acceptance), 10, &policy));
    assert!(
        matches!(refused, Err(JournalError::Unauthorized)),
        "{refused:?}"
    );
    let own = terminate("own-stop", "launch");
    let ticket = fresh(j.accept(s, &own, 10, &policy).unwrap());
    j.execute(ticket, 10, &policy).unwrap();
}

/// Renders an engine with one worker child, so inspection has a group to find.
struct WithWorker(ChildPolicy);
impl LocalExecutionPolicy for WithWorker {
    fn authorize(&self, c: &MemberCommand) -> Result<(), JournalError> {
        self.0.authorize(c)
    }
    fn render_launch(
        &self,
        _: &MemberCommand,
    ) -> Result<capyctl_agent::journal::ApprovedLaunch, JournalError> {
        Ok(capyctl_agent::journal::ApprovedLaunch {
            command: capyctl_adapters::traits::RenderedCommand {
                argv: vec!["/bin/sh".into(), "-c".into(), "sleep 60 & wait".into()],
                env: Default::default(),
            },
            descriptors: None,
        })
    }
    fn admit_beside(
        &self,
        _: &MemberCommand,
        _: &[capyctl_agent::journal::ClaimedLaunch],
    ) -> Result<(), JournalError> {
        Ok(())
    }
}

// SPEC §§3.1, 13.2 (per-launch claims) T33 T20: one launch whose leader is
// gone makes inspection uncertain for that launch only; every other claimed
// launch is still refreshed with the group it has now.
#[test]
fn inspection_uncertainty_is_per_launch() {
    let d = directory();
    let policy = WithWorker(ChildPolicy {
        marker: d.path().join("never"),
    });
    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    let s = j.connect().unwrap();
    let mut cleanups = Vec::new();
    for (id, deployment) in [("a", "deployment-a"), ("b", "deployment-b")] {
        let ticket = fresh(
            j.accept(s, &launch_of(id, deployment, 1), 10, &policy)
                .unwrap(),
        );
        j.execute(ticket, 10, &policy).unwrap();
        let api = j.inspect_owned(id).unwrap()[0].0.clone();
        cleanups.push(ChildCleanup(api));
    }
    // `a`'s leader dies.
    let a_api = j.inspect_owned("a").unwrap()[0].0.clone();
    unsafe { libc::kill(a_api.pid as i32, libc::SIGKILL) };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while capyctl_launchers::process_absence::presence(&a_api)
        == capyctl_domain::completion::Presence::Alive
    {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let inspect = sign(command("inspect"));
    let ticket = fresh(j.accept(s, &inspect, 10, &policy).unwrap());
    assert!(j.execute(ticket, 10, &policy).is_err());
    // `b` was still refreshed: its worker child is journaled now.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        if j.inspect_owned("b").unwrap().len() >= 2 {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "b was not refreshed");
        let again = sign(command(&format!(
            "inspect-{}",
            capyctl_protocol::now_unix_ms()
        )));
        if let Ok(Acceptance::Fresh(ticket)) = j.accept(s, &again, 10, &policy) {
            let _ = j.execute(ticket, 10, &policy);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

// T33 T13: SPEC §13.2. Initialization is atomic: the "established" marker is
// written only after the schema is durable, so a crash part-way through leaves
// either nothing or a pristine, never-used database, and the next open finishes
// the job. A journal that ever held a command, or whose database vanished under
// its marker, is still never reinitialized.
#[test]
fn an_interrupted_initialization_is_completed_not_bricked() {
    let marker = |d: &std::path::Path| d.join("journal-established");
    // Crash before the schema committed: an empty database, no marker.
    let d = directory();
    std::fs::File::create(d.path().join("commands.sqlite")).unwrap();
    std::fs::set_permissions(
        d.path().join("commands.sqlite"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    assert!(marker(d.path()).exists());
    drop(j);
    HostJournal::open(d.path(), "controller", "host").unwrap();
    // Crash after the schema committed, before the marker: completed.
    let d = directory();
    drop(HostJournal::open(d.path(), "controller", "host").unwrap());
    std::fs::remove_file(marker(d.path())).unwrap();
    HostJournal::open(d.path(), "controller", "host").unwrap();
    assert!(marker(d.path()).exists());
    // A used journal without its marker is not adopted.
    let d = directory();
    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    let s = j.connect().unwrap();
    j.accept(s, &sign(command("used")), 10, &Policy).unwrap();
    drop(j);
    std::fs::remove_file(marker(d.path())).unwrap();
    assert!(HostJournal::open(d.path(), "controller", "host").is_err());
}

// T33: a journal written by a newer capyctl is refused with a typed error naming
// both versions, and the refusal writes nothing: its version and retained
// history are exactly what the newer binary left.
#[test]
fn journal_from_a_newer_version_is_refused_and_left_unmodified() {
    let supported = capyctl_agent::journal::JOURNAL_SCHEMA_VERSION;
    let d = directory();
    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    let s = j.connect().unwrap();
    drop(fresh(j.accept(s, &command("kept"), 10, &Policy).unwrap()));
    drop(j);
    let db = rusqlite::Connection::open(d.path().join("commands.sqlite")).unwrap();
    db.execute_batch(&format!(
        "CREATE TABLE from_the_future(x INTEGER); PRAGMA user_version={};",
        supported + 1
    ))
    .unwrap();
    drop(db);
    match HostJournal::open(d.path(), "controller", "host") {
        Err(JournalError::FromNewerVersion {
            found,
            supported: known,
        }) => {
            assert_eq!((found, known), (supported + 1, supported));
        }
        Err(other) => panic!("expected FromNewerVersion, got {other}"),
        Ok(_) => panic!("an older binary opened a newer journal"),
    }
    let db = rusqlite::Connection::open(d.path().join("commands.sqlite")).unwrap();
    let version: i64 = db
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, supported + 1);
    let commands: i64 = db
        .query_row("SELECT count(*) FROM commands", [], |r| r.get(0))
        .unwrap();
    assert_eq!(commands, 1);
}

/// This test process's own identity: certainly alive while the test runs.
fn own_identity() -> capyctl_domain::completion::ProcessIdentity {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    let close = stat.rfind(')').unwrap();
    let start_ticks = stat[close + 2..]
        .split_whitespace()
        .nth(19)
        .unwrap()
        .parse()
        .unwrap();
    capyctl_domain::completion::ProcessIdentity {
        role: "api".into(),
        pid: std::process::id(),
        boot_id: std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .unwrap()
            .trim()
            .to_owned(),
        start_ticks,
    }
}

fn terminate_recorded(
    id: &str,
    handle: &str,
    recorded: Vec<capyctl_domain::completion::ProcessIdentity>,
) -> MemberCommand {
    let mut c = command(id);
    c.identity.expected_state = "retained".into();
    c.action = MemberAction::Terminate {
        owned_handle: handle.into(),
        recorded,
    };
    sign(c)
}

// T33 T34 (ADR 0016): a host that lost its journal (a recovered host with
// fresh state) has no record of a launch the server still accounts for. A
// Terminate carrying the server's recorded identities is then answered by
// observing exactly those identities: nothing is signalled or adopted, an
// alive one is reported alive (so the server cannot release the launch), and
// only when every one is gone is gone evidence reported, by identity. A host
// whose journal knows the launch ignores the recorded identities and answers
// from its own record.
#[test]
fn terminate_of_an_unrecorded_launch_reports_the_recorded_identities_without_signalling() {
    let d = directory();
    let policy = ChildPolicy {
        marker: d.path().join("never"),
    };
    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    let s = j.connect().unwrap();
    let alive = own_identity();
    let mut gone = alive.clone();
    gone.role = "worker-0".into();
    gone.start_ticks += 1;

    let first = terminate_recorded(
        "lost-first",
        "lost-launch",
        vec![alive.clone(), gone.clone()],
    );
    let ticket = fresh(j.accept(s, &first, 10, &policy).unwrap());
    j.execute(ticket, 10, &policy).unwrap();
    let result = j.execution_result("lost-first", 20).unwrap();
    capyctl_protocol::execution::validate_result(&first, &result).unwrap();
    assert_eq!(result.state, "completed");
    assert!(!result.claim_retained);
    let presence = |result: &capyctl_protocol::pb::MemberExecutionResult, pid: u32, ticks: u64| {
        result
            .processes
            .iter()
            .find(|p| p.pid == pid && p.start_ticks == ticks)
            .map(|p| p.presence.clone())
    };
    assert_eq!(
        presence(&result, alive.pid, alive.start_ticks).as_deref(),
        Some("alive")
    );
    assert_eq!(
        presence(&result, gone.pid, gone.start_ticks).as_deref(),
        Some("gone")
    );
    // Nothing was signalled: this very process is the "alive" engine.
    assert_eq!(
        capyctl_launchers::process_absence::presence(&alive),
        capyctl_domain::completion::Presence::Alive
    );
    // The handle is fenced: the launch can never start here afterwards.
    assert!(matches!(
        j.accept(s, &launch("lost-launch"), 10, &policy),
        Err(JournalError::Conflict)
    ));

    // A later Terminate, once the recorded processes are gone, reports them
    // gone by identity: the evidence the server settles the launch on.
    let second = terminate_recorded("lost-second", "lost-launch", vec![gone.clone()]);
    let ticket = fresh(j.accept(s, &second, 10, &policy).unwrap());
    j.execute(ticket, 10, &policy).unwrap();
    let result = j.execution_result("lost-second", 20).unwrap();
    capyctl_protocol::execution::validate_result(&second, &result).unwrap();
    assert_eq!(result.state, "completed");
    assert!(!result.claim_retained);
    assert_eq!(result.processes.len(), 1);
    assert_eq!(result.processes[0].presence, "gone");

    // A launch this journal does know is answered from its own record; the
    // recorded identities are ignored, never signalled.
    drop(fresh(j.accept(s, &launch("known"), 10, &policy).unwrap()));
    let known = terminate_recorded("known-stop", "known", vec![alive.clone()]);
    let ticket = fresh(j.accept(s, &known, 10, &policy).unwrap());
    j.execute(ticket, 10, &policy).unwrap();
    let result = j.execution_result("known-stop", 20).unwrap();
    assert!(result.processes.is_empty(), "{result:?}");
    assert_eq!(
        capyctl_launchers::process_absence::presence(&alive),
        capyctl_domain::completion::Presence::Alive
    );
    assert!(!policy.marker.exists());
}
