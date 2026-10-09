//! ADR 0013 §4, §5 (owner decision P1; host journal v5): a host fences each
//! instance of a deployment by that instance's own last assignment.
//!
//! A deployment's instances draw generations from one counter, so fencing by
//! the deployment's highest generation refused an older instance's commands
//! once a newer instance of the same deployment had reached the host. These
//! tests drive the journal directly; no engine runs. CPU tests are not
//! qualification (SPEC §18).

use capyctl_agent::journal::{
    Acceptance, ClaimedLaunch, ExecutionTicket, HostJournal, JournalError, LocalExecutionPolicy,
};
use capyctl_domain::group::{CommandIdentity, MemberKey};
use capyctl_protocol::execution::{MemberAction, MemberCommand, SingleLaunchPlan};
use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex;

fn directory() -> tempfile::TempDir {
    let d = tempfile::Builder::new()
        .prefix("capyctl-fencing-")
        .tempdir_in(std::env::var("HOME").unwrap())
        .unwrap();
    std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    d
}

fn sign(mut command: MemberCommand) -> MemberCommand {
    command.identity.payload_digest = command.canonical_digest();
    command
}

fn fresh(acceptance: Acceptance) -> ExecutionTicket {
    match acceptance {
        Acceptance::Fresh(ticket) => ticket,
        other => panic!("expected fresh acceptance: {other:?}"),
    }
}

fn identity(id: &str, deployment: &str, instance: u32, generation: i64) -> CommandIdentity {
    CommandIdentity {
        controller_id: "controller".into(),
        member: MemberKey {
            host_id: "host".into(),
            member_id: "head".into(),
        },
        deployment_id: deployment.into(),
        operation_id: format!("operation-{id}"),
        command_id: id.into(),
        step_id: id.into(),
        generation,
        revision: 1,
        deadline_ms: 60_000,
        payload_digest: [0; 32],
        expected_state: "reserved".into(),
        profile_fingerprint: "approved".into(),
        instance_index: instance,
    }
}

/// The reserved launch of one instance incarnation, as a per-instance
/// controller sends it. `slot` keeps binding, incarnation and port distinct.
fn launch(id: &str, deployment: &str, instance: u32, generation: i64, slot: u8) -> MemberCommand {
    let golden: serde_json::Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    sign(MemberCommand {
        identity: identity(id, deployment, instance, generation),
        action: MemberAction::LaunchSingle(SingleLaunchPlan {
            deployment_config: golden["input"]["deployment"].to_string(),
            profile_name: "local".into(),
            checkpoint_fingerprint: "sha256:model".into(),
            host_policy_fingerprint: "a".repeat(64),
            binding_id: format!("01K000000000000000000001{slot:02}"),
            incarnation: format!("01K000000000000000000002{slot:02}"),
            grant_id: format!("01K000000000000000000003{slot:02}"),
            service_port: 30_000 + u16::from(slot),
            issued_at_ms: 1,
            coordinator_session_id: "01K00000000000000000000400".into(),
            checkpoint_digest: String::new(),
            checkpoint_weights_bytes: None,
            checkpoint_state_slot_bytes: None,
            checkpoint_layout: None,
            checkpoint_tables: None,
            checkpoint_gguf: None,
            startup_bytes: None,
        }),
    })
}

/// A command naming `owner`'s launch; `instance` is what it declares.
fn about(
    owner: &MemberCommand,
    id: &str,
    generation: i64,
    instance: u32,
    action: MemberAction,
) -> MemberCommand {
    let mut identity = identity(id, &owner.identity.deployment_id, instance, generation);
    identity.expected_state = match action {
        MemberAction::Terminate { .. } => "retained",
        MemberAction::Restore { .. } => "parked",
        _ => "ready",
    }
    .into();
    sign(MemberCommand { identity, action })
}

fn terminate(owner: &MemberCommand, id: &str, generation: i64, instance: u32) -> MemberCommand {
    about(
        owner,
        id,
        generation,
        instance,
        MemberAction::Terminate {
            owned_handle: owner.identity.command_id.clone(),
            recorded: Vec::new(),
        },
    )
}

fn restore(owner: &MemberCommand, id: &str) -> MemberCommand {
    about(
        owner,
        id,
        owner.identity.generation,
        owner.identity.instance_index,
        MemberAction::Restore {
            owned_handle: owner.identity.command_id.clone(),
            checkpoint_digest: String::new(),
        },
    )
}

/// A per-launch host policy: every launch fits beside the others, and a wake
/// fits beside at most `wake_room` other claims. Launches are never rendered.
struct PerInstance {
    wake_room: usize,
    woken_beside: Mutex<Vec<Vec<String>>>,
}
impl PerInstance {
    fn new(wake_room: usize) -> Self {
        Self {
            wake_room,
            woken_beside: Mutex::default(),
        }
    }
}
impl LocalExecutionPolicy for PerInstance {
    fn authorize(&self, _: &MemberCommand) -> Result<(), JournalError> {
        Ok(())
    }
    fn render_launch(
        &self,
        _: &MemberCommand,
    ) -> Result<capyctl_agent::journal::ApprovedLaunch, JournalError> {
        Err(JournalError::Unauthorized)
    }
    fn authorize_residency(
        &self,
        _: &MemberCommand,
        _: &MemberCommand,
    ) -> Result<(), JournalError> {
        Ok(())
    }
    fn admit_beside(&self, _: &MemberCommand, _: &[ClaimedLaunch]) -> Result<(), JournalError> {
        Ok(())
    }
    fn admit_wake(
        &self,
        _: &MemberCommand,
        owner: &MemberCommand,
        claimed: &[ClaimedLaunch],
    ) -> Result<(), JournalError> {
        let others: Vec<String> = claimed
            .iter()
            .map(|c| c.command.identity.command_id.clone())
            .collect();
        assert!(!others.contains(&owner.identity.command_id));
        self.woken_beside.lock().unwrap().push(others);
        if claimed.len() <= self.wake_room {
            Ok(())
        } else {
            Err(JournalError::Unauthorized)
        }
    }
}

/// Settle `owner`'s launch, which never released a process, on this host.
fn settle(j: &std::sync::Arc<HostJournal>, s: u64, command: &MemberCommand, p: &PerInstance) {
    let ticket = fresh(j.accept(s, command, 10, p).unwrap());
    j.execute(ticket, 10, p).unwrap();
    let result = j
        .execution_result(&command.identity.command_id, 20)
        .unwrap();
    assert!(
        result.state == "completed" && !result.claim_retained,
        "{result:?}"
    );
}

fn claimed(j: &HostJournal) -> Vec<String> {
    j.claimed_launches("")
        .unwrap()
        .into_iter()
        .map(|c| c.command.identity.command_id)
        .collect()
}

/// P1: two instances of one deployment on one host each keep their own
/// claim and fence. Instance 0 keeps generation 5 after instance 1 reached the
/// host at generation 6: its commands and its own durable attempt are still
/// admitted, a stale generation of instance 0 is refused without touching
/// instance 1, a command naming a launch cannot claim another instance, and
/// each stops on its own.
// T24 T26 T34 T10 T13
#[test]
fn two_instances_of_one_deployment_are_fenced_independently() {
    let d = directory();
    let policy = PerInstance::new(usize::MAX);
    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    let s = j.connect().unwrap();
    let zero = launch("zero", "deployment", 0, 5, 0);
    let one = launch("one", "deployment", 1, 6, 1);
    let ticket_zero = fresh(j.accept(s, &zero, 10, &policy).unwrap());
    drop(fresh(j.accept(s, &one, 10, &policy).unwrap()));
    assert_eq!(claimed(&j), ["zero", "one"]);

    // Instance 0's durable attempt is checked against its own assignment
    // (generation 5), not the deployment's newest (6).
    let tools = j
        .launch_tools(ticket_zero, 10, std::sync::Arc::new(PerInstance::new(0)))
        .expect("instance 0 is not fenced by instance 1's generation");
    drop(tools);

    // A stale command for instance 0 is refused; instance 1 is unaffected.
    assert!(matches!(
        j.accept(s, &launch("zero-stale", "deployment", 0, 4, 2), 10, &policy),
        Err(JournalError::Fenced)
    ));
    assert!(matches!(
        j.accept(s, &terminate(&zero, "zero-stale-stop", 4, 0), 10, &policy),
        Err(JournalError::Fenced)
    ));
    // A command naming instance 0's launch while declaring instance 1 is a
    // conflict, never a way to borrow instance 1's newer fence.
    assert!(matches!(
        j.accept(s, &terminate(&zero, "zero-as-one", 6, 1), 10, &policy),
        Err(JournalError::Conflict)
    ));
    assert_eq!(claimed(&j), ["zero", "one"]);

    // Instance 0 stops on its own at its own generation, from a controller
    // that declares its instance or one that does not (instance 0 decodes the
    // same either way).
    settle(&j, s, &terminate(&zero, "stop-zero", 5, 0), &policy);
    assert_eq!(claimed(&j), ["one"], "only instance 0's claim released");
    // Instance 1, older-numbered commands and all, is still its own: a
    // command naming its launch without declaring an instance is fenced by
    // instance 1's assignment.
    assert!(matches!(
        j.accept(s, &terminate(&one, "one-stale", 5, 0), 10, &policy),
        Err(JournalError::Fenced)
    ));
    settle(&j, s, &terminate(&one, "stop-one", 6, 0), &policy);
    assert!(claimed(&j).is_empty());

    // The fences survive a restart.
    drop(j);
    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    let s = j.connect().unwrap();
    assert!(matches!(
        j.accept(s, &launch("zero-late", "deployment", 0, 4, 3), 10, &policy),
        Err(JournalError::Fenced)
    ));
    drop(fresh(
        j.accept(s, &launch("zero-again", "deployment", 0, 5, 4), 10, &policy)
            .unwrap(),
    ));
}

/// Rewrites a current journal's fencing tables exactly as a version-4 agent
/// left them: one assignment per deployment and no per-command instance.
const V4_SHAPE: &str = "CREATE TABLE assignments_v4(deployment TEXT PRIMARY KEY,generation INTEGER NOT NULL,revision INTEGER NOT NULL);
    INSERT INTO assignments_v4 SELECT deployment,generation,revision FROM assignments WHERE instance=0;
    DROP TABLE assignments; ALTER TABLE assignments_v4 RENAME TO assignments;
    ALTER TABLE commands DROP COLUMN instance; PRAGMA user_version=4;";

/// Journal v5 migration: a version-4 journal's assignments become instance
/// 0's, keeping every fence and retained claim; afterwards another instance
/// of the same deployment is fenced on its own. A version-5 journal missing
/// either instance column is corrupt and never repaired.
// T33 T34 T13
#[test]
fn a_v4_journal_migrates_its_assignments_to_instance_zero() {
    let d = directory();
    let policy = PerInstance::new(usize::MAX);
    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    let s = j.connect().unwrap();
    let retained = launch("retained", "deployment", 0, 7, 0);
    drop(fresh(j.accept(s, &retained, 10, &policy).unwrap()));
    drop(j);
    let db = rusqlite::Connection::open(d.path().join("commands.sqlite")).unwrap();
    db.execute_batch(V4_SHAPE).unwrap();
    drop(db);

    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    let s = j.connect().unwrap();
    assert_eq!(claimed(&j), ["retained"], "the retained claim is adopted");
    assert!(matches!(
        j.accept(s, &retained, 10, &policy).unwrap(),
        Acceptance::Replay(ref r) if r.claim_retained
    ));
    // The migrated fence is instance 0's.
    assert!(matches!(
        j.accept(s, &launch("older", "deployment", 0, 6, 1), 10, &policy),
        Err(JournalError::Fenced)
    ));
    // Instance 1 has no assignment yet: an older generation than instance
    // 0's is its own, admitted beside it.
    drop(fresh(
        j.accept(s, &launch("other", "deployment", 1, 3, 2), 10, &policy)
            .unwrap(),
    ));
    drop(j);
    let db = rusqlite::Connection::open(d.path().join("commands.sqlite")).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        capyctl_agent::journal::JOURNAL_SCHEMA_VERSION
    );
    let assignments: Vec<(String, i64, i64)> = db
        .prepare("SELECT deployment,instance,generation FROM assignments ORDER BY instance")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        assignments,
        [("deployment".into(), 0, 7), ("deployment".into(), 1, 3)]
    );
    drop(db);

    for damage in [
        // Control: an undamaged copy opens.
        "",
        "ALTER TABLE commands DROP COLUMN instance",
        "CREATE TABLE a(deployment TEXT PRIMARY KEY,generation INTEGER NOT NULL,revision INTEGER NOT NULL);
         DROP TABLE assignments; ALTER TABLE a RENAME TO assignments;",
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

/// SPEC §§3.1, 7.3, 9.1: a wake is charged beside every other launch this
/// host claims. Over the host's own budget it is refused before anything is
/// journaled, and the launch stays parked; once room frees it is admitted.
/// The woken launch itself is never counted as its own neighbour.
// T24 T26 T20 T23
#[test]
fn a_wake_over_the_host_budget_is_refused_and_stays_parked() {
    let d = directory();
    let policy = PerInstance::new(0);
    let j = HostJournal::open(d.path(), "controller", "host").unwrap();
    let s = j.connect().unwrap();
    let parked = launch("parked", "deployment", 0, 5, 0);
    let beside = launch("beside", "deployment", 1, 6, 1);
    drop(fresh(j.accept(s, &parked, 10, &policy).unwrap()));
    drop(fresh(j.accept(s, &beside, 10, &policy).unwrap()));
    // The launch parked in place (its durable W4 outcome).
    let db = rusqlite::Connection::open(d.path().join("commands.sqlite")).unwrap();
    db.execute(
        "INSERT INTO residency(owner,state,command_id) VALUES('parked','parked','park-parked')",
        [],
    )
    .unwrap();
    drop(db);

    let wake = restore(&parked, "wake");
    assert!(matches!(
        j.accept(s, &wake, 10, &policy),
        Err(JournalError::Unauthorized)
    ));
    assert_eq!(j.residency_of("parked").unwrap().as_deref(), Some("parked"));
    assert!(j
        .history(0, 100)
        .unwrap()
        .iter()
        .all(|r| r.command_id != "wake"));
    assert_eq!(
        *policy.woken_beside.lock().unwrap(),
        [vec!["beside".to_string()]]
    );

    // The other instance stops; the same wake now fits and is accepted.
    settle(&j, s, &terminate(&beside, "stop-beside", 6, 1), &policy);
    drop(fresh(j.accept(s, &wake, 10, &policy).unwrap()));
    assert_eq!(
        policy.woken_beside.lock().unwrap().last().unwrap(),
        &Vec::<String>::new()
    );
}
