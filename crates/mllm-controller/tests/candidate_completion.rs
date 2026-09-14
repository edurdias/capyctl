//! Local F2 collector contract: exactly two owned Fake members. No native discovery.
use mllm_config::effective::{
    candidate::validate_candidate_reviewed_snapshot_text, resolve_effective,
};
use mllm_controller::{RuntimeAction, RuntimeCommand};
use mllm_domain::completion::{
    CleanupEvidence, CompletionEvidence, Milestone, OwnedLaunchReceipt, ProcessIdentity,
};
use mllm_domain::resources::{MemoryLimit, MemoryObservation, ResourcePhase};
use mllm_scheduler::residency::AdmissionContext;
use mllm_store::{
    Store,
    candidate_creation::{
        cleanup::{CleanupExecutionContext, CleanupMode},
        initialize::ArmResult,
    },
};
use serde_json::{Value, json};

struct OwnedFake {
    members: Vec<(ProcessIdentity, bool)>,
    initialize_sends: usize,
    termination_sends: usize,
}
impl OwnedFake {
    fn new() -> Self {
        Self {
            members: vec![],
            initialize_sends: 0,
            termination_sends: 0,
        }
    }
    fn initialize(&mut self, command: &RuntimeCommand) -> OwnedLaunchReceipt {
        assert_eq!(command.action, RuntimeAction::Initialize);
        assert!(matches!(
            command.context.launch_settings,
            Some(mllm_domain::launch::ProfileLaunchSettings::Fake(_))
        ));
        assert!(self.members.is_empty());
        self.initialize_sends += 1;
        self.members = vec![
            (
                ProcessIdentity {
                    role: "api".into(),
                    pid: 71,
                    boot_id: "owned-fake-boot".into(),
                    start_ticks: 100,
                },
                true,
            ),
            (
                ProcessIdentity {
                    role: "worker-0".into(),
                    pid: 72,
                    boot_id: "owned-fake-boot".into(),
                    start_ticks: 101,
                },
                true,
            ),
        ];
        OwnedLaunchReceipt {
            binding_id: command.context.binding_id.clone(),
            incarnation: command.context.incarnation.clone(),
            identities: self.members.iter().map(|(i, _)| i.clone()).collect(),
            observed_at_ms: 1250,
            receipt: "fake collector checked both owned members".into(),
        }
    }
    fn ready(&self, command: &RuntimeCommand) -> CompletionEvidence {
        assert!(self.members.iter().all(|(_, alive)| *alive));
        CompletionEvidence {
            token: command.context.token.clone(),
            identities: self.members.iter().map(|(i, _)| i.clone()).collect(),
            observed_at_ms: 1300,
            control_receipt: Some("fake allocator/cache/model milestones".into()),
            milestones: vec![
                Milestone::AllocationsRestored,
                Milestone::WeightsUsable,
                Milestone::CacheValid,
                Milestone::ModelUsable,
            ],
        }
    }
    fn cleanup(
        &mut self,
        context: &CleanupExecutionContext,
        observed: i64,
    ) -> Option<CleanupEvidence> {
        assert_eq!(
            context.identities,
            self.members
                .iter()
                .map(|(i, _)| i.clone())
                .collect::<Vec<_>>()
        );
        if context.mode == CleanupMode::TerminateOwned {
            self.termination_sends += 1;
            for (_, alive) in &mut self.members {
                *alive = false;
            }
        }
        self.members
            .iter()
            .all(|(_, alive)| !*alive)
            .then(|| CleanupEvidence {
                binding_id: context.binding_id.clone(),
                incarnation: context.incarnation.clone(),
                identities: self.members.iter().map(|(i, _)| i.clone()).collect(),
                observed_at_ms: observed,
                receipt: "fake collector verified every owned member gone".into(),
            })
    }
}

#[test]
fn local_fake_candidate_composes_full_store_protocol_and_restart_inspection() {
    let store = Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let ordinary: Value = serde_json::from_str(include_str!(
        "../../mllm-config/tests/fixtures/effective-fake-golden.json"
    ))
    .unwrap();
    let mut host = ordinary["input"]["host"].clone();
    let mut manifest: Value = serde_json::from_str(include_str!(
        "../../mllm-config/tests/fixtures/candidate-fake.json"
    ))
    .unwrap();
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = socket.local_addr().unwrap().port();
    drop(socket);
    host["resource_policy"]["endpoint_port_range"] = json!({"start":port,"end":port});
    manifest["limits"]["max_run_duration_ms"] = json!(600000);
    manifest["limits"]["max_cleanup_duration_ms"] = json!(60000);
    let reviewed = validate_candidate_reviewed_snapshot_text(&manifest.to_string()).unwrap();
    host["qualification_policy"] = json!({"revision":1,"allow_qualification_runs":true,"allow_experimental_controls":true,"allowed_manifest_digests":[reviewed.manifest_digest()],"max_run_duration":"600s","max_cleanup_duration":"60s","max_cases":128,"max_requests":4096,"max_request_body_bytes":"1MiB","max_input_tokens_per_request":131072,"max_output_tokens_per_request":16384});
    let policy = resolve_effective(&ordinary["input"]["deployment"], &host)
        .unwrap()
        .host;
    let observations: Vec<_> = policy
        .domains
        .keys()
        .map(|domain| MemoryObservation {
            domain: domain.clone(),
            capacity_bytes: 1_i64 << 50,
            available_bytes: 1_i64 << 50,
            sampled_at_ms: 1000,
        })
        .collect();
    store
        .import_resource_policy(&session, &policy, &observations, 1000)
        .unwrap();
    store
        .import_qualification_policy(&session, &policy)
        .unwrap();
    let body=json!({"host_id":"lab","expected_host_revision":1,"recipe_digest":reviewed.manifest_digest(),"manifest":manifest,"deadline_ms":500000,"allow_owned_abort_cleanup":true}).to_string();
    let created = store
        .create_candidate_run(&session, "owner", "create", &body, &host, 1000)
        .unwrap();
    let init = store
        .accept_candidate_initialize(
            &session,
            "owner",
            created.run_id(),
            "init",
            r#"{"expected_revision":1,"action":"initialize","deadline_ms":400000}"#,
            1100,
        )
        .unwrap();
    let resource = store.resource_policy("lab").unwrap().unwrap();
    let limits: Vec<_> = resource
        .controls
        .domains
        .iter()
        .map(|(domain, p)| MemoryLimit {
            domain: domain.clone(),
            managed_bytes: p.managed_limit,
            free_reserve_bytes: p.free_reserve,
            host_kv_bytes: p.host_kv_limit,
            parked_bytes: p.parked_limit,
        })
        .collect();
    let mut fake = OwnedFake::new();
    let armed = store
        .arm_step(
            &session,
            init.step_id(),
            AdmissionContext::new(
                &observations,
                &limits,
                1200,
                resource.controls.observation_ttl_ms,
                resource.controls.max_parked as usize,
            ),
        )
        .unwrap();
    assert!(matches!(armed, ArmResult::New { .. }));
    let command = RuntimeCommand {
        action: RuntimeAction::Initialize,
        context: store
            .candidate_initialize_execution(&session, init.step_id())
            .unwrap(),
    };
    let association = fake.initialize(&command);
    store
        .record_owned_launch(&session, init.step_id(), &association, 1250)
        .unwrap();
    let ready = fake.ready(&command);
    store
        .complete_step(
            &session,
            init.step_id(),
            &ready,
            1300,
            resource.controls.observation_ttl_ms,
        )
        .unwrap();
    assert_eq!(
        store.resource_snapshot().unwrap().owners[created.deployment_id()].phase,
        ResourcePhase::Cold
    );
    let body = r#"{"expected_revision":1,"action":"cleanup","deadline_ms":12000}"#;
    assert!(matches!(
        store.finish_candidate_run(
            &session,
            "owner",
            created.run_id(),
            "finish",
            r#"{"expected_revision":1,"action":"finish"}"#,
            1400
        ),
        Err(mllm_store::lifecycle::LifecycleError::Unsupported)
    ));
    let cleanup = store
        .accept_candidate_cleanup(&session, "owner", created.run_id(), "cleanup", body, 2000)
        .unwrap();
    assert!(matches!(
        store
            .arm_candidate_cleanup(&session, cleanup.step_id(), 2100)
            .unwrap(),
        ArmResult::New { .. }
    ));
    let execution = store
        .candidate_cleanup_execution(&session, cleanup.step_id())
        .unwrap();
    fake.cleanup(&execution, 2200).unwrap();
    // Simulate lost acknowledgment after termination. Recovery inspects; it never resends termination.
    let next_session = store.begin_coordinator_session().unwrap();
    let recovery = store
        .accept_candidate_cleanup(
            &next_session,
            "owner",
            created.run_id(),
            "recover",
            body,
            2300,
        )
        .unwrap();
    // A second crash occurs before the inspection action arms. Its successor
    // must retain inspection-only authority from the earlier termination.
    let unarmed_inspection = recovery;
    let next_session = store.begin_coordinator_session().unwrap();
    let recovery = store
        .accept_candidate_cleanup(
            &next_session,
            "owner",
            created.run_id(),
            "recover-again",
            body,
            2350,
        )
        .unwrap();
    assert!(
        store
            .candidate_cleanup_execution(&next_session, unarmed_inspection.step_id())
            .is_err()
    );
    assert!(matches!(
        store
            .arm_candidate_cleanup(&next_session, recovery.step_id(), 2400)
            .unwrap(),
        ArmResult::New { .. }
    ));
    let inspection = store
        .candidate_cleanup_execution(&next_session, recovery.step_id())
        .unwrap();
    assert_eq!(inspection.mode, CleanupMode::InspectOwnedGone);
    fake.members[1].1 = true;
    assert!(fake.cleanup(&inspection, 2500).is_none());
    assert_eq!(store.resource_snapshot().unwrap().owners.len(), 1);
    fake.members[1].1 = false;
    let gone = fake.cleanup(&inspection, 2500).unwrap();
    store
        .complete_cleanup(
            &next_session,
            recovery.step_id(),
            &gone,
            2500,
            resource.controls.observation_ttl_ms,
        )
        .unwrap();
    assert!(store.resource_snapshot().unwrap().owners.is_empty());
    assert_eq!(fake.initialize_sends, 1);
    assert_eq!(fake.termination_sends, 1);
    store
        .complete_step(&next_session, init.step_id(), &ready, 999999, 1)
        .unwrap();
    store
        .complete_cleanup(&next_session, recovery.step_id(), &gone, 999999, 1)
        .unwrap();
}
