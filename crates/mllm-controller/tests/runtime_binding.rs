use std::sync::Arc;

use mllm_adapters::fake::FakeEngine;
use mllm_adapters::traits::RenderedCommand;
use mllm_controller::{
    DurableRuntimeSupervisor, RuntimeAction, RuntimeBinding, RuntimeBindings, RuntimeError,
    RuntimeOwnership,
};
use mllm_domain::resources::{Allocation, PhaseFootprint, RecipeFootprints, ResourcePhase};
use mllm_domain::{DeploymentId, LifecycleState, OperationId};
use mllm_launchers::DurableSpawnOutcome;
use mllm_store::lifecycle::{DeploymentFence, ReserveBinding};
use mllm_store::{AcceptDeployment, Store};

struct NativeFixture {
    store: Store,
    session: mllm_store::dispatch::CoordinatorSession,
    deployment: String,
    step: String,
    observations: Vec<mllm_domain::resources::MemoryObservation>,
    limits: Vec<mllm_domain::resources::MemoryLimit>,
    ttl: i64,
    max_parked: usize,
    _directory: tempfile::TempDir,
}

impl NativeFixture {
    fn new() -> Self {
        use mllm_config::effective::{
            candidate::validate_candidate_reviewed_snapshot_text, resolve_effective,
        };
        use mllm_domain::resources::{MemoryLimit, MemoryObservation};
        use serde_json::{Value, json};
        let ordinary: Value = serde_json::from_str(include_str!(
            "../../mllm-config/tests/fixtures/effective-sglang-golden.json"
        ))
        .unwrap();
        let mut host = ordinary["input"]["host"].clone();
        let mut manifest: Value = serde_json::from_str(include_str!(
            "../../mllm-config/tests/fixtures/candidate-sglang.json"
        ))
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        manifest["effective_recipe"]["model"]["path"] = json!(root.to_str().unwrap());
        manifest["effective_recipe"]["model"]["revision"] =
            json!("cdbee75f17c01a7cc42f958dc650907174af0554");
        manifest["effective_recipe"]["resolved_profile"]["build_fingerprint"] =
            json!("fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1");
        host["runtime_profiles"]["local"]["build_fingerprint"] =
            json!("fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1");
        manifest["limits"]["max_run_duration_ms"] = json!(600000);
        manifest["limits"]["max_cleanup_duration_ms"] = json!(60000);
        let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = socket.local_addr().unwrap().port();
        drop(socket);
        host["resource_policy"]["endpoint_port_range"] = json!({"start":port,"end":port});
        let reviewed = validate_candidate_reviewed_snapshot_text(&manifest.to_string()).unwrap();
        host["qualification_policy"] = json!({"revision":1,"allow_qualification_runs":true,"allow_experimental_controls":true,"allowed_manifest_digests":[reviewed.manifest_digest()],"max_run_duration":"600s","max_cleanup_duration":"60s","max_cases":128,"max_requests":4096,"max_request_body_bytes":"1MiB","max_input_tokens_per_request":131072,"max_output_tokens_per_request":16384});
        let policy = resolve_effective(&ordinary["input"]["deployment"], &host)
            .unwrap()
            .host;
        let store = Store::open(&directory.path().join("native.db")).unwrap();
        let session = store.begin_coordinator_session().unwrap();
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
        let command = json!({"host_id":"lab","expected_host_revision":1,"recipe_digest":reviewed.manifest_digest(),"manifest":manifest,"deadline_ms":500000,"allow_owned_abort_cleanup":true}).to_string();
        let created = store
            .create_candidate_run(&session, "owner", "create", &command, &host, 1000)
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
        let limits = resource
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
        Self {
            store,
            session,
            deployment: created.deployment_id().into(),
            step: init.step_id().into(),
            observations,
            limits,
            ttl: resource.controls.observation_ttl_ms,
            max_parked: resource.controls.max_parked as usize,
            _directory: directory,
        }
    }

    fn context(&self) -> mllm_scheduler::residency::AdmissionContext<'_> {
        mllm_scheduler::residency::AdmissionContext::new(
            &self.observations,
            &self.limits,
            1200,
            self.ttl,
            self.max_parked,
        )
    }

    fn revoke(&self) {
        let connection =
            rusqlite::Connection::open(self._directory.path().join("native.db")).unwrap();
        connection.execute("UPDATE host_qualification_policies SET policy_json=json_set(policy_json,'$.state.policy.allow_qualification_runs',json('false'))", []).unwrap();
    }

    fn assert_retained(&self) {
        let connection =
            rusqlite::Connection::open(self._directory.path().join("native.db")).unwrap();
        let retained: (String, String, i64, i64, i64, i64) = connection.query_row("SELECT b.state,b.identities_json,d.admission_enabled,d.dispatch_enabled,(SELECT COUNT(*) FROM endpoint_leases),(SELECT COUNT(*) FROM resource_grants) FROM runtime_bindings b JOIN deployments d ON d.id=b.deployment_id", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).unwrap();
        assert_eq!(retained, ("uncertain".into(), "[]".into(), 0, 0, 1, 1));
    }
}

fn native_service() -> mllm_controller::runtime::NativeCandidateService<'static> {
    static PATH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    mllm_controller::runtime::NativeCandidateService {
        wrapper: PATH.get_or_init(|| {
            std::path::Path::new("/usr/bin/true")
                .canonicalize()
                .unwrap()
        }),
        now_ms: &|| Ok(1200),
    }
}

fn resolve_native_credential(reference: &str) -> Result<Vec<u8>, RuntimeError> {
    match reference {
        "secret://engine-key" => Ok(b"private-inference-token".to_vec()),
        "secret://admin-key" => Ok(b"private-admin-token".to_vec()),
        _ => panic!("unexpected reference"),
    }
}

#[test]
fn native_candidate_policy_revoked_during_preflight_rejects_handoff() {
    let fixture = NativeFixture::new();
    let result = mllm_controller::runtime::NativeCandidateHandoff::arm(
        &fixture.store,
        &fixture.session,
        &fixture.step,
        fixture.context(),
        &resolve_native_credential,
        &|_| {
            fixture.revoke();
            Ok(())
        },
        native_service(),
    );
    assert!(
        result.is_err(),
        "revoked policy still produced a launch handoff"
    );
    assert_eq!(
        fixture.store.resource_snapshot().unwrap().owners[&fixture.deployment].phase,
        ResourcePhase::Cold
    );
    fixture.assert_retained();
}

#[test]
fn native_candidate_deadline_advanced_during_callbacks_rejects_handoff() {
    for during_preflight in [true, false] {
        let fixture = NativeFixture::new();
        let now = std::cell::Cell::new(1200);
        let clock = || Ok(now.get());
        let result = mllm_controller::runtime::NativeCandidateHandoff::arm(
            &fixture.store,
            &fixture.session,
            &fixture.step,
            fixture.context(),
            &|reference| {
                if !during_preflight {
                    now.set(400000);
                }
                resolve_native_credential(reference)
            },
            &|_| {
                if during_preflight {
                    now.set(400000);
                }
                Ok(())
            },
            mllm_controller::runtime::NativeCandidateService {
                now_ms: &clock,
                ..native_service()
            },
        );
        assert!(result.is_err(), "expired callbacks produced handoff");
        fixture.assert_retained();
    }
}

#[test]
fn native_candidate_revocation_and_deadline_are_checked_before_spawn_and_acknowledgement() {
    use mllm_launchers::LaunchAssociation;
    for revoke in [true, false] {
        let fixture = NativeFixture::new();
        let now = std::cell::Cell::new(1200);
        let clock = || Ok(now.get());
        let handoff = mllm_controller::runtime::NativeCandidateHandoff::arm(
            &fixture.store,
            &fixture.session,
            &fixture.step,
            fixture.context(),
            &resolve_native_credential,
            &|_| Ok(()),
            mllm_controller::runtime::NativeCandidateService {
                now_ms: &clock,
                ..native_service()
            },
        )
        .unwrap()
        .unwrap();
        if revoke {
            fixture.revoke();
        } else {
            now.set(400000);
        }
        let error = handoff
            .persist_api_identity(&mllm_domain::completion::ProcessIdentity {
                role: "api".into(),
                pid: 42,
                boot_id: "test-boot".into(),
                start_ticks: 100,
            })
            .unwrap_err();
        assert!(format!("{error:?}").contains("candidate handoff is stale"));
        let error = handoff
            .spawn(&mllm_launchers::DurableSpawn::new())
            .err()
            .unwrap();
        assert_eq!(
            error,
            RuntimeError::Uncertain("candidate handoff is stale".into())
        );
        fixture.assert_retained();
    }
}

#[test]
fn native_candidate_clock_failure_is_redacted_and_retains_reservations() {
    let fixture = NativeFixture::new();
    let calls = std::cell::Cell::new(0);
    let clock = || {
        calls.set(calls.get() + 1);
        if calls.get() == 1 {
            Ok(1200)
        } else {
            Err(RuntimeError::Uncertain("private-clock-detail".into()))
        }
    };
    let error = mllm_controller::runtime::NativeCandidateHandoff::arm(
        &fixture.store,
        &fixture.session,
        &fixture.step,
        fixture.context(),
        &resolve_native_credential,
        &|_| Ok(()),
        mllm_controller::runtime::NativeCandidateService {
            now_ms: &clock,
            ..native_service()
        },
    )
    .err()
    .unwrap();
    assert_eq!(
        error,
        RuntimeError::Uncertain("candidate clock unavailable".into())
    );
    fixture.assert_retained();
}

#[test]
fn native_candidate_wrapper_permissions_rechecked_before_spawn_and_acknowledgement() {
    use mllm_launchers::LaunchAssociation;
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = directory.path().join("sglang_entry.py");
    std::fs::write(&path, b"# Never executed by handoff tests.\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let fixture = NativeFixture::new();
    let handoff = mllm_controller::runtime::NativeCandidateHandoff::arm(
        &fixture.store,
        &fixture.session,
        &fixture.step,
        fixture.context(),
        &resolve_native_credential,
        &|_| Ok(()),
        mllm_controller::runtime::NativeCandidateService {
            wrapper: &path,
            ..native_service()
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!(handoff.command().argv[2], path.to_str().unwrap());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
    assert!(
        handoff
            .persist_api_identity(&mllm_domain::completion::ProcessIdentity {
                role: "api".into(),
                pid: 42,
                boot_id: "test-boot".into(),
                start_ticks: 100,
            })
            .is_err()
    );
    assert_eq!(
        handoff
            .spawn(&mllm_launchers::DurableSpawn::new())
            .err()
            .unwrap(),
        RuntimeError::Unsupported
    );
    fixture.assert_retained();
}

#[test]
fn native_candidate_handoff_is_single_use_secret_free_and_ordinary_dispatch_stays_closed() {
    use mllm_controller::runtime::NativeCandidateHandoff;
    let fixture = NativeFixture::new();
    let handoff = NativeCandidateHandoff::arm(
        &fixture.store,
        &fixture.session,
        &fixture.step,
        fixture.context(),
        &resolve_native_credential,
        &|_| Ok(()),
        native_service(),
    )
    .unwrap()
    .unwrap();
    let command = handoff.command();
    assert!(command.env.is_empty());
    assert_eq!(
        &command.argv[1..4],
        [
            "-I",
            native_service().wrapper.to_str().unwrap(),
            "--public-settings-json"
        ]
    );
    let text = format!("{command:?}");
    for private in [
        "private-inference-token",
        "private-admin-token",
        "secret://",
        fixture._directory.path().to_str().unwrap(),
    ] {
        assert!(!text.contains(private));
    }
    let private: serde_json::Value = serde_json::from_slice(
        &std::fs::read(format!("/proc/self/fd/{}", command.argv[6])).unwrap(),
    )
    .unwrap();
    assert_eq!(private["schema_version"], 1);
    assert_eq!(private["kind"], "sglang_candidate_private_launch");
    assert_eq!(
        private["checkpoint_root"],
        fixture._directory.path().to_str().unwrap()
    );
    assert_eq!(
        private["public_settings"],
        serde_json::from_str::<serde_json::Value>(&command.argv[4]).unwrap()
    );
    assert_eq!(private.as_object().unwrap().len(), 4);
    assert_eq!(
        std::fs::read(format!("/proc/self/fd/{}", command.argv[8])).unwrap(),
        b"private-inference-token"
    );
    assert_eq!(
        std::fs::read(format!("/proc/self/fd/{}", command.argv[10])).unwrap(),
        b"private-admin-token"
    );
    assert!(
        NativeCandidateHandoff::arm(
            &fixture.store,
            &fixture.session,
            &fixture.step,
            fixture.context(),
            &|_| panic!("replay resolved credentials"),
            &|_| panic!("replay preflight"),
            native_service(),
        )
        .unwrap()
        .is_none()
    );
    assert!(matches!(
        RuntimeBindings::default().binding(&fixture.deployment, 1),
        Err(RuntimeError::Missing)
    ));
    assert!(fixture.store.runtime_binding(&fixture.deployment).is_err());
    assert_eq!(
        fixture.store.resource_snapshot().unwrap().owners[&fixture.deployment].phase,
        ResourcePhase::Cold
    );
    let connection =
        rusqlite::Connection::open(fixture._directory.path().join("native.db")).unwrap();
    let flags: (i64, i64) = connection
        .query_row(
            "SELECT admission_enabled,dispatch_enabled FROM deployments WHERE id=?1",
            [&fixture.deployment],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(flags, (0, 0));
    for table in ["runtime_bindings", "lifecycle_steps", "management_events"] {
        let mut statement = connection
            .prepare(&format!("SELECT * FROM {table}"))
            .unwrap();
        let count = statement.column_count();
        let rows = statement
            .query_map([], |row| {
                (0..count)
                    .map(|column| row.get::<_, rusqlite::types::Value>(column))
                    .collect::<Result<Vec<_>, _>>()
            })
            .unwrap();
        for row in rows {
            let text = format!("{:?}", row.unwrap());
            assert!(!text.contains("private-inference-token"));
            assert!(!text.contains("private-admin-token"));
        }
    }
    drop(handoff);
    assert!(
        NativeCandidateHandoff::arm(
            &fixture.store,
            &fixture.session,
            &fixture.step,
            fixture.context(),
            &resolve_native_credential,
            &|_| Ok(()),
            native_service(),
        )
        .unwrap()
        .is_none()
    );
}

#[test]
fn native_candidate_preflight_failure_consumes_authority_without_handoff_or_secret_error() {
    use mllm_controller::runtime::NativeCandidateHandoff;
    let fixture = NativeFixture::new();
    let checked = std::cell::Cell::new(false);
    let result = NativeCandidateHandoff::arm(
        &fixture.store,
        &fixture.session,
        &fixture.step,
        fixture.context(),
        &|_| panic!("failed preflight resolved credentials"),
        &|_| {
            checked.set(true);
            Err(RuntimeError::Uncertain("private-checkpoint-secret".into()))
        },
        native_service(),
    );
    let error = result.err().unwrap();
    assert!(checked.get());
    assert!(!format!("{error:?}").contains("private-checkpoint-secret"));
    assert!(
        NativeCandidateHandoff::arm(
            &fixture.store,
            &fixture.session,
            &fixture.step,
            fixture.context(),
            &resolve_native_credential,
            &|_| Ok(()),
            native_service(),
        )
        .unwrap()
        .is_none()
    );
    assert_eq!(
        fixture.store.resource_snapshot().unwrap().owners[&fixture.deployment].phase,
        ResourcePhase::Cold
    );
}

#[test]
fn native_candidate_stale_session_cannot_spawn_prepared_handoff() {
    use mllm_controller::runtime::NativeCandidateHandoff;
    let fixture = NativeFixture::new();
    let handoff = NativeCandidateHandoff::arm(
        &fixture.store,
        &fixture.session,
        &fixture.step,
        fixture.context(),
        &resolve_native_credential,
        &|_| Ok(()),
        native_service(),
    )
    .unwrap()
    .unwrap();
    let _new_session = fixture.store.begin_coordinator_session().unwrap();
    // Revalidation must return before any process creation. Never execute Python here.
    assert!(handoff.spawn(&mllm_launchers::DurableSpawn::new()).is_err());
}

#[test]
fn native_candidate_stale_generation_cannot_spawn_prepared_handoff() {
    use mllm_controller::runtime::NativeCandidateHandoff;
    let fixture = NativeFixture::new();
    let handoff = NativeCandidateHandoff::arm(
        &fixture.store,
        &fixture.session,
        &fixture.step,
        fixture.context(),
        &resolve_native_credential,
        &|_| Ok(()),
        native_service(),
    )
    .unwrap()
    .unwrap();
    fixture.store.bump_generation(&fixture.deployment).unwrap();
    assert!(handoff.spawn(&mllm_launchers::DurableSpawn::new()).is_err());
}

#[test]
fn native_candidate_changed_binding_cannot_spawn_prepared_handoff() {
    use mllm_controller::runtime::NativeCandidateHandoff;
    let fixture = NativeFixture::new();
    let handoff = NativeCandidateHandoff::arm(
        &fixture.store,
        &fixture.session,
        &fixture.step,
        fixture.context(),
        &resolve_native_credential,
        &|_| Ok(()),
        native_service(),
    )
    .unwrap()
    .unwrap();
    let connection =
        rusqlite::Connection::open(fixture._directory.path().join("native.db")).unwrap();
    connection
        .execute(
            "UPDATE runtime_bindings SET incarnation=?1",
            [ulid::Ulid::new().to_string()],
        )
        .unwrap();
    assert!(handoff.spawn(&mllm_launchers::DurableSpawn::new()).is_err());
}

#[test]
fn native_candidate_ambiguous_api_association_retains_endpoint_grant_and_closed_dispatch() {
    use mllm_controller::runtime::NativeCandidateHandoff;
    use mllm_launchers::LaunchAssociation;
    let fixture = NativeFixture::new();
    let handoff = NativeCandidateHandoff::arm(
        &fixture.store,
        &fixture.session,
        &fixture.step,
        fixture.context(),
        &resolve_native_credential,
        &|_| Ok(()),
        native_service(),
    )
    .unwrap()
    .unwrap();
    let connection =
        rusqlite::Connection::open(fixture._directory.path().join("native.db")).unwrap();
    connection.execute_batch("CREATE TRIGGER fail_api BEFORE UPDATE OF identities_json ON runtime_bindings BEGIN SELECT RAISE(ABORT,'private-association-detail'); END;").unwrap();
    let error = handoff
        .persist_api_identity(&mllm_domain::completion::ProcessIdentity {
            role: "api".into(),
            pid: 42,
            boot_id: "test-boot".into(),
            start_ticks: 100,
        })
        .unwrap_err();
    assert!(!format!("{error:?}").contains("private-association-detail"));
    let retained: (String,String,i64,i64,i64,i64) = connection.query_row("SELECT b.state,b.identities_json,d.admission_enabled,d.dispatch_enabled,(SELECT COUNT(*) FROM endpoint_leases),(SELECT COUNT(*) FROM resource_grants) FROM runtime_bindings b JOIN deployments d ON d.id=b.deployment_id",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).unwrap();
    assert_eq!(retained, ("uncertain".into(), "[]".into(), 0, 0, 1, 1));
    assert!(
        NativeCandidateHandoff::arm(
            &fixture.store,
            &fixture.session,
            &fixture.step,
            fixture.context(),
            &resolve_native_credential,
            &|_| Ok(()),
            native_service(),
        )
        .unwrap()
        .is_none()
    );
}

#[test]
fn native_candidate_invalid_credentials_never_create_handoff_or_retry() {
    use mllm_controller::runtime::NativeCandidateHandoff;
    for secret in [b"duplicate-token".as_slice(), b"invalid\nsecret", b""] {
        let fixture = NativeFixture::new();
        assert!(
            NativeCandidateHandoff::arm(
                &fixture.store,
                &fixture.session,
                &fixture.step,
                fixture.context(),
                &|_| Ok(secret.to_vec()),
                &|_| Ok(()),
                native_service(),
            )
            .is_err()
        );
        assert!(
            NativeCandidateHandoff::arm(
                &fixture.store,
                &fixture.session,
                &fixture.step,
                fixture.context(),
                &resolve_native_credential,
                &|_| Ok(()),
                native_service(),
            )
            .unwrap()
            .is_none()
        );
    }
}

fn footprint(phase: ResourcePhase) -> PhaseFootprint {
    PhaseFootprint {
        phase,
        allocations: vec![Allocation {
            domain: "gpu:0".into(),
            bytes: 1,
            host_kv_bytes: 0,
        }],
        devices: vec![],
    }
}

#[test]
fn durable_spawn_attempt_survives_supervisor_recreation() {
    let store = Store::open_in_memory().unwrap();
    let deployment = DeploymentId::new();
    store
        .accept_deployment(AcceptDeployment {
            id: deployment,
            name: "durable-supervisor".into(),
            kind: "model".into(),
            route_model_id: None,
            desired_state: LifecycleState::Stopped,
            schema_version: 1,
            idempotency_key: "durable-supervisor".into(),
            initial_operation_id: OperationId("durable-supervisor-operation".into()),
        })
        .unwrap();
    let deployment_id = deployment.to_string();
    let fence = DeploymentFence {
        deployment_id: deployment_id.clone(),
        revision: 1,
        generation: 1,
    };
    let session = store.begin_coordinator_session().unwrap();
    store
        .reserve_runtime_binding(
            &session,
            &ReserveBinding {
                id: "durable-binding".into(),
                fence: fence.clone(),
                incarnation: "durable-incarnation".into(),
                qualification_id: "qualification".into(),
                ownership: "managed".into(),
                endpoint_host: "127.0.0.1".into(),
                endpoint_port: 31011,
                credential_ref: "credential-reference".into(),
                binding_payload: "recipe-reference".into(),
            },
        )
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("initialized");
    let command = RenderedCommand {
        argv: vec![
            "sh".into(),
            "-c".into(),
            format!("touch '{}'", marker.display()),
        ],
        env: Default::default(),
    };
    let first = DurableRuntimeSupervisor::new(&store, &session)
        .spawn(&fence, "durable-binding", &command)
        .unwrap();
    assert!(matches!(
        first,
        DurableSpawnOutcome::Uncertain {
            initialization_acknowledged: true,
            ..
        }
    ));
    let second =
        DurableRuntimeSupervisor::new(&store, &session).spawn(&fence, "durable-binding", &command);
    assert!(matches!(second, Err(RuntimeError::Uncertain(_))));
    let retained = store.runtime_binding(&deployment_id).unwrap().unwrap();
    assert_eq!(retained.state, "uncertain");
    assert_eq!(retained.endpoint, "127.0.0.1:31011");
}

fn recipe() -> RecipeFootprints {
    RecipeFootprints {
        cold: footprint(ResourcePhase::Cold),
        ready: footprint(ResourcePhase::Ready),
        parking: footprint(ResourcePhase::Parking),
        parked: footprint(ResourcePhase::Parked),
        wake: footprint(ResourcePhase::Wake),
    }
}

fn binding(id: &str, deployment: &str, revision: i64, port: u16) -> Arc<RuntimeBinding> {
    let engine = Arc::new(FakeEngine::new());
    Arc::new(RuntimeBinding {
        id: id.into(),
        deployment_id: deployment.into(),
        revision,
        incarnation: format!("inc-{id}"),
        qualification_id: "qualification-1".into(),
        recipe: recipe(),
        ownership: RuntimeOwnership::Managed,
        endpoint: format!("127.0.0.1:{port}"),
        credential_ref: format!("credential-{id}"),
        driver: engine.clone(),
        forward: engine,
    })
}

#[test]
fn deployment_bindings_are_distinct_and_survive_parking() {
    let bindings = RuntimeBindings::default();
    let first = binding("binding-a", "deployment-a", 1, 31001);
    let second = binding("binding-b", "deployment-b", 1, 31002);
    bindings.retain(first.clone()).unwrap();
    bindings.retain(second.clone()).unwrap();
    bindings.park("deployment-a", 1).unwrap();

    let parked = bindings.binding("deployment-a", 1).unwrap();
    assert_eq!(parked.id, "binding-a");
    assert_eq!(parked.endpoint, "127.0.0.1:31001");
    assert_eq!(parked.credential_ref, "credential-binding-a");
    assert_ne!(parked.id, second.id);
    assert_ne!(parked.endpoint, second.endpoint);
    assert_ne!(parked.credential_ref, second.credential_ref);
    assert!(matches!(
        bindings.binding("deployment-a", 2),
        Err(RuntimeError::StaleRevision)
    ));
}

#[test]
fn retained_binding_is_immutable_and_attached_control_is_unsupported() {
    let bindings = RuntimeBindings::default();
    let first = binding("binding-a", "deployment-a", 1, 31001);
    bindings.retain(first).unwrap();
    assert!(matches!(
        bindings.retain(binding("replacement", "deployment-a", 1, 31003)),
        Err(RuntimeError::Uncertain(_))
    ));

    let mut attached = binding("attached", "deployment-attached", 1, 31004);
    Arc::get_mut(&mut attached).unwrap().ownership = RuntimeOwnership::Attached;
    bindings.retain(attached).unwrap();
    for action in [
        RuntimeAction::Initialize,
        RuntimeAction::Drain,
        RuntimeAction::Park,
        RuntimeAction::Restore,
        RuntimeAction::Stop,
        RuntimeAction::Probe,
        RuntimeAction::Inspect,
    ] {
        assert!(matches!(
            bindings.control("deployment-attached", 1, action),
            Err(RuntimeError::Unsupported)
        ));
    }
}
