//! SPEC §13.2 (W13, G08): a host agent observes the exit of an owned engine
//! process of a Ready launch and reports it as a `MemberExit`, with the exit
//! status its launcher reaped. The claim stays until a Terminate settles it.
//!
//! The "engine" here is a shell with one worker child. CPU tests are not
//! qualification of a native engine recipe (SPEC §18).
use capyctl_agent::journal::{Acceptance, HostJournal, JournalError, LocalExecutionPolicy};
use capyctl_domain::completion::{EffectObservation, Milestone, ProcessIdentity, TransitionToken};
use capyctl_domain::group::{CommandIdentity, MemberKey};
use capyctl_protocol::execution::{MemberAction, MemberCommand, SingleLaunchPlan};
use capyctl_protocol::reports::{ExitStatus, MemberExit};
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

struct Workers;
impl LocalExecutionPolicy for Workers {
    fn authorize(&self, _: &MemberCommand) -> Result<(), JournalError> {
        Ok(())
    }
    fn render_launch(
        &self,
        _: &MemberCommand,
    ) -> Result<capyctl_agent::journal::ApprovedLaunch, JournalError> {
        Ok(launch_of("sleep 60 & wait; exec sleep 61"))
    }
}

/// ADR 0027: an engine whose worker started a helper of its own, as a
/// scheduler starts a compile worker pool. The worker outlives its helper.
struct WithHelper;
impl LocalExecutionPolicy for WithHelper {
    fn authorize(&self, _: &MemberCommand) -> Result<(), JournalError> {
        Ok(())
    }
    fn render_launch(
        &self,
        _: &MemberCommand,
    ) -> Result<capyctl_agent::journal::ApprovedLaunch, JournalError> {
        Ok(launch_of(
            "sh -c 'sleep 60 & wait; exec sleep 61' & wait; exec sleep 61",
        ))
    }
}

fn launch_of(script: &str) -> capyctl_agent::journal::ApprovedLaunch {
    capyctl_agent::journal::ApprovedLaunch {
        command: capyctl_adapters::traits::RenderedCommand {
            argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
            env: Default::default(),
        },
        descriptors: None,
    }
}

fn directory() -> tempfile::TempDir {
    let d = tempfile::Builder::new()
        .prefix("capyctl-exit-")
        .tempdir_in(std::env::var("HOME").unwrap())
        .unwrap();
    std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    d
}

fn command(id: &str) -> MemberCommand {
    MemberCommand {
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
            deadline_ms: 30_000,
            payload_digest: [0; 32],
            expected_state: "reserved".into(),
            profile_fingerprint: "approved".into(),
            instance_index: 0,
        },
        action: MemberAction::Inspect,
    }
}

fn sign(mut command: MemberCommand) -> MemberCommand {
    command.identity.payload_digest = command.canonical_digest();
    command
}

/// Kills whatever of the group is left if the test fails part-way.
struct Group(Vec<ProcessIdentity>);
impl Drop for Group {
    fn drop(&mut self) {
        for process in &self.0 {
            if capyctl_launchers::process_absence::presence(process)
                == capyctl_domain::completion::Presence::Alive
            {
                unsafe {
                    libc::kill(process.pid as i32, libc::SIGKILL);
                }
            }
        }
    }
}

fn scan_until(
    journal: &std::sync::Arc<HostJournal>,
    within: Duration,
    done: impl Fn(&[capyctl_agent::exits::ExitedLaunch]) -> bool,
) -> Vec<MemberExit> {
    let started = Instant::now();
    loop {
        let exited = capyctl_agent::exits::scan(journal, "host", capyctl_protocol::now_unix_ms());
        if done(&exited) {
            return exited.into_iter().map(|launch| launch.exit).collect();
        }
        assert!(
            started.elapsed() < within,
            "no exit observed within {within:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// T20 T30 T32 T33 (W13): a Ready launch is watched by its recorded group. A
/// worker that dies alone (a partial group) is reported at once with an
/// unobserved status, since this host did not reap it; the API process this
/// host's launcher reaped is reported with its signal, well inside a second.
/// The first observation is journaled, the claim is kept throughout, and only
/// a Terminate ends the watch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_exited_member_of_a_ready_launch_is_reported_with_its_status() {
    let d = directory();
    let journal = HostJournal::open(d.path(), "controller", "host").unwrap();
    let session = journal.connect().unwrap();
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    let mut launch = command("exit-launch");
    let binding = "01K00000000000000000000001".to_owned();
    let incarnation = "01K00000000000000000000002".to_owned();
    launch.action = MemberAction::LaunchSingle(SingleLaunchPlan {
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
        checkpoint_layout: None,
        checkpoint_tables: None,
        checkpoint_gguf: None,
        startup_bytes: None,
    });
    let launch = sign(launch);
    let policy = std::sync::Arc::new(Workers);
    let Acceptance::Fresh(ticket) = journal
        .accept(session, &launch, 10, policy.as_ref())
        .unwrap()
    else {
        panic!("fresh acceptance");
    };
    let tools = journal.launch_tools(ticket, 10, policy.clone()).unwrap();
    let api = tools
        .spawn_durable(
            &incarnation,
            &policy.render_launch(&launch).unwrap().command,
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let group = loop {
        let observed = tools.observe_group(&api).unwrap();
        if observed.len() >= 2 {
            break observed;
        }
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    };
    let _cleanup = Group(group.clone());
    // Not Ready yet: nothing is watched.
    assert!(capyctl_agent::exits::scan(&journal, "host", 20).is_empty());
    journal
        .record_launch_ready(
            session,
            "exit-launch",
            &EffectObservation {
                token: TransitionToken {
                    deployment_id: "deployment".into(),
                    operation_id: "operation".into(),
                    step_id: "exit-launch".into(),
                    revision: 1,
                    generation: 1,
                },
                binding_id: binding,
                incarnation,
                identities: group.clone(),
                observed_at_ms: 20,
                receipt: "controlled adapter probe".into(),
                facts: vec![
                    Milestone::AllocationsRestored,
                    Milestone::WeightsUsable,
                    Milestone::CacheValid,
                    Milestone::ModelUsable,
                ],
                kernel_builds: Vec::new(),
            },
        )
        .unwrap();
    assert!(
        capyctl_agent::exits::scan(&journal, "host", 30).is_empty(),
        "a live group has no exit"
    );

    // A partial group: the worker dies, the API process lives on.
    let worker = group.iter().find(|p| p.role != "api").unwrap().clone();
    unsafe {
        libc::kill(worker.pid as i32, libc::SIGKILL);
    }
    let reported = scan_until(&journal, Duration::from_secs(5), |exited| {
        !exited.is_empty()
    });
    assert_eq!(reported.len(), 1);
    assert_eq!(reported[0].process, worker);
    assert_eq!(reported[0].status, ExitStatus::Unobserved);
    assert_eq!(reported[0].owned_handle, "exit-launch");
    assert_eq!(reported[0].generation, 1);
    // The wire form is the W3 report the controller validates.
    assert_eq!(
        MemberExit::try_from(reported[0].to_wire()).unwrap(),
        reported[0]
    );

    // The API process this host reaped: named first, with its signal.
    let killed = Instant::now();
    unsafe {
        libc::kill(api.pid as i32, libc::SIGKILL);
    }
    // The scan sees the exit as soon as the process is reaped; the bound
    // leaves room for a slow runner to reap it.
    let reported = scan_until(&journal, Duration::from_secs(5), |exited| {
        exited
            .first()
            .is_some_and(|launch| launch.exit.status == ExitStatus::Signal(9))
    });
    assert!(
        killed.elapsed() < Duration::from_secs(5),
        "reported once reaped"
    );
    assert_eq!(reported[0].process, api);
    // The claim is kept: an exit report is never release evidence.
    assert!(journal
        .history(0, 100)
        .unwrap()
        .iter()
        .any(|r| r.command_id == "exit-launch" && r.claim_retained));
    // The first observation is journaled: a later scan reports the same time.
    let again =
        capyctl_agent::exits::scan(&journal, "host", capyctl_protocol::now_unix_ms() + 5_000);
    assert_eq!(again[0].exit.observed_at_ms, reported[0].observed_at_ms);

    // A Terminate settles the launch; nothing is watched after it.
    drop(tools);
    let mut stop = command("stop-exit-launch");
    stop.identity.expected_state = "retained".into();
    stop.action = MemberAction::Terminate {
        owned_handle: "exit-launch".into(),
        recorded: Vec::new(),
    };
    let stop = sign(stop);
    let Acceptance::Fresh(ticket) = journal.accept(session, &stop, 10, policy.as_ref()).unwrap()
    else {
        panic!("fresh acceptance");
    };
    journal.execute(ticket, 10, policy.as_ref()).unwrap();
    assert!(
        capyctl_agent::exits::scan(&journal, "host", capyctl_protocol::now_unix_ms()).is_empty()
    );
}

/// ADR 0027: a helper of a Ready launch (started by one of the engine's own
/// processes) that exits is not reported: the engine is still the one recorded.
/// The engine's own worker exiting afterwards still is, and a Terminate still
/// settles the whole launch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_helper_exit_of_a_ready_launch_is_not_reported() {
    let d = directory();
    let journal = HostJournal::open(d.path(), "controller", "host").unwrap();
    let session = journal.connect().unwrap();
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    let mut launch = command("helper-launch");
    let binding = "01K00000000000000000000001".to_owned();
    let incarnation = "01K00000000000000000000002".to_owned();
    launch.action = MemberAction::LaunchSingle(SingleLaunchPlan {
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
        checkpoint_layout: None,
        checkpoint_tables: None,
        checkpoint_gguf: None,
        startup_bytes: None,
    });
    let launch = sign(launch);
    let policy = std::sync::Arc::new(WithHelper);
    let Acceptance::Fresh(ticket) = journal
        .accept(session, &launch, 10, policy.as_ref())
        .unwrap()
    else {
        panic!("fresh acceptance");
    };
    let tools = journal.launch_tools(ticket, 10, policy.clone()).unwrap();
    let api = tools
        .spawn_durable(
            &incarnation,
            &policy.render_launch(&launch).unwrap().command,
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    let group = loop {
        let observed = tools.observe_group(&api).unwrap();
        if observed.len() >= 3 {
            break observed;
        }
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    };
    let _cleanup = Group(group.clone());
    let roles: Vec<_> = group.iter().map(|p| p.role.as_str()).collect();
    assert_eq!(roles, ["api", "worker-0", "helper-0"], "{group:?}");
    journal
        .record_launch_ready(
            session,
            "helper-launch",
            &EffectObservation {
                token: TransitionToken {
                    deployment_id: "deployment".into(),
                    operation_id: "operation".into(),
                    step_id: "helper-launch".into(),
                    revision: 1,
                    generation: 1,
                },
                binding_id: binding,
                incarnation,
                identities: group.clone(),
                observed_at_ms: 20,
                receipt: "controlled adapter probe".into(),
                facts: vec![
                    Milestone::AllocationsRestored,
                    Milestone::WeightsUsable,
                    Milestone::CacheValid,
                    Milestone::ModelUsable,
                ],
                kernel_builds: Vec::new(),
            },
        )
        .unwrap();

    // The helper exits, as an idle compile worker does.
    let helper = group[2].clone();
    unsafe {
        libc::kill(helper.pid as i32, libc::SIGKILL);
    }
    let gone_by = Instant::now() + Duration::from_secs(5);
    while capyctl_launchers::process_absence::presence(&helper)
        != capyctl_domain::completion::Presence::Gone
    {
        assert!(Instant::now() < gone_by, "the helper exits");
        std::thread::sleep(Duration::from_millis(20));
    }
    for _ in 0..10 {
        assert!(
            capyctl_agent::exits::scan(&journal, "host", capyctl_protocol::now_unix_ms())
                .is_empty(),
            "a helper's exit is not the engine's"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // The engine's own worker exiting is.
    let worker = group[1].clone();
    unsafe {
        libc::kill(worker.pid as i32, libc::SIGKILL);
    }
    let reported = scan_until(&journal, Duration::from_secs(5), |exited| {
        !exited.is_empty()
    });
    assert_eq!(reported[0].process, worker);

    drop(tools);
    let mut stop = command("stop-helper-launch");
    stop.identity.expected_state = "retained".into();
    stop.action = MemberAction::Terminate {
        owned_handle: "helper-launch".into(),
        recorded: Vec::new(),
    };
    let stop = sign(stop);
    let Acceptance::Fresh(ticket) = journal.accept(session, &stop, 10, policy.as_ref()).unwrap()
    else {
        panic!("fresh acceptance");
    };
    journal.execute(ticket, 10, policy.as_ref()).unwrap();
    for process in &group {
        assert_ne!(
            capyctl_launchers::process_absence::presence(process),
            capyctl_domain::completion::Presence::Alive,
            "{process:?} outlived the Terminate"
        );
    }
}
