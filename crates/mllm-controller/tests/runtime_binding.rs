use std::sync::Arc;

use mllm_testkit::FakeEngine;
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
    binding: String,
    incarnation: String,
    endpoint: String,
    root: std::path::PathBuf,
    observations: Vec<mllm_domain::resources::MemoryObservation>,
    limits: Vec<mllm_domain::resources::MemoryLimit>,
    ttl: i64,
    max_parked: usize,
    _directory: tempfile::TempDir,
}

/// An ordinary managed deployment whose initialize has been accepted.
/// No candidate run takes part: the frozen descriptor comes from the test source.
impl NativeFixture {
    fn new() -> Self {
        use mllm_config::effective::resolve_effective;
        use mllm_domain::resources::{MemoryLimit, MemoryObservation};
        use serde_json::{Value, json};
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let source: Value = serde_json::from_str(include_str!(
            "../../mllm-config/tests/fixtures/effective-vllm-golden.json"
        ))
        .unwrap();
        let host = source["input"]["host"].clone();
        let mut deployment = source["input"]["deployment"].clone();
        deployment["name"] = json!("ordinary");
        deployment["routes"] = json!(["ordinary"]);
        let policy = resolve_effective(&deployment, &host).unwrap().host;
        let store = Store::open(&root.join("native.db")).unwrap();
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
        let receipt = store
            .create_stopped_managed_configuration(
                &session,
                "owner",
                "ordinary",
                &json!({ "config": deployment }).to_string(),
                &host,
                1000,
            )
            .unwrap();
        let fence = DeploymentFence {
            deployment_id: receipt.deployment_id.clone(),
            revision: receipt.revision,
            generation: receipt.generation,
        };
        let accepted = store
            .accept_start(&session, &fence, 1100, 300000)
            .unwrap();
        let connection = rusqlite::Connection::open(root.join("native.db")).unwrap();
        let (incarnation, endpoint): (String, String) = connection
            .query_row(
                "SELECT incarnation,json_extract(binding_json,'$.endpoint') FROM runtime_bindings WHERE id=?1",
                [&accepted.binding_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
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
            deployment: fence.deployment_id,
            step: accepted.step_id,
            binding: accepted.binding_id,
            incarnation,
            endpoint,
            root,
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

    fn sql(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.root.join("native.db")).unwrap()
    }

    /// A frozen descriptor for this deployment's accepted initialize. The store
    /// holds none; the digest distinguishes one descriptor from another.
    fn descriptor(&self, digest: &str) -> mllm_domain::launch::NativeLaunch {
        use mllm_config::effective::sglang::{
            NATIVE_CHECKPOINT_REVISION, NATIVE_SGLANG_RECIPE, NATIVE_SGLANG_SOURCE_REVISION,
        };
        use mllm_domain::launch::{
            NativeDeviceSelection, NativeLaunch, NativeLaunchMetadata, SglangLaunchSettings,
            SglangRequestedBudget,
        };
        NativeLaunch::from_frozen_store(
            NativeLaunchMetadata {
                engine: "sglang".into(),
                recipe: NATIVE_SGLANG_RECIPE.into(),
                source_revision: NATIVE_SGLANG_SOURCE_REVISION.into(),
                checkpoint_revision: NATIVE_CHECKPOINT_REVISION.into(),
                binding_id: self.binding.clone(),
                incarnation: self.incarnation.clone(),
                endpoint: format!("http://{}", self.endpoint),
                served_name: "ordinary".into(),
                rendered_settings_digest: digest.into(),
                placement_digest: None,
                device: NativeDeviceSelection {
                    host_id: "lab".into(),
                    hardware_fingerprint: "hw-01".into(),
                    device_id: "gpu0".into(),
                    memory_domain: "unified".into(),
                },
            },
            self.root.to_str().unwrap().into(),
            "/usr/bin/python3".into(),
            "secret://engine-key".into(),
            "secret://admin-key".into(),
            SglangLaunchSettings {
                recipe: NATIVE_SGLANG_RECIPE.into(),
                tensor_parallel_size: 1,
                data_parallel_size: 1,
                tokenizer_workers: 1,
                model_dtype: "bfloat16".into(),
                context_tokens: 4096,
                max_running_requests: 8,
                max_total_tokens: 4096,
                prefill_cuda_graphs: false,
                decode_cuda_graphs: false,
                memory_saver: true,
                cpu_weight_backup: false,
                speculative_decoding: false,
                lora: false,
                trust_remote_code: false,
                disaggregation: false,
                external_cache: false,
                cpu_kv_offload: false,
                native_grpc: false,
                weight_restore: "disk_reload".into(),
                requested_budget: SglangRequestedBudget {
                    kv_cache_bytes: 1_i64 << 30,
                    static_memory_fraction_bps: 9000,
                },
            },
        )
    }

    fn source(&self) -> FixedSource {
        FixedSource(std::sync::Mutex::new(Some(self.descriptor(FIRST_DIGEST))))
    }

    fn assert_retained(&self) {
        let retained: (String, String, i64, i64, i64, i64) = self.sql().query_row("SELECT b.state,b.identities_json,d.admission_enabled,d.dispatch_enabled,(SELECT COUNT(*) FROM endpoint_leases),(SELECT COUNT(*) FROM resource_grants) FROM runtime_bindings b JOIN deployments d ON d.id=b.deployment_id", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).unwrap();
        // The accepted start enables admission; dispatch stays closed until the
        // initialize completes, and neither lease nor grant may be released here.
        assert_eq!(retained, ("uncertain".into(), "[]".into(), 1, 0, 1, 1));
    }
}

const FIRST_DIGEST: &str = "1f2e3d4c5b6a79881f2e3d4c5b6a79881f2e3d4c5b6a79881f2e3d4c5b6a7988";
const SECOND_DIGEST: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

/// The application's descriptor source. It must answer with the same value for
/// the same step; answering with another one must stop the handoff.
struct FixedSource(std::sync::Mutex<Option<mllm_domain::launch::NativeLaunch>>);

impl FixedSource {
    fn replace(&self, launch: mllm_domain::launch::NativeLaunch) {
        *self.0.lock().unwrap() = Some(launch);
    }
}

impl mllm_controller::runtime::NativeLaunchSource for FixedSource {
    fn frozen(
        &self,
        _: &mllm_store::dispatch::CoordinatorSession,
        _: &str,
        _: i64,
    ) -> Result<mllm_domain::launch::NativeLaunch, RuntimeError> {
        self.0
            .lock()
            .unwrap()
            .clone()
            .ok_or(RuntimeError::Uncertain("no descriptor".into()))
    }
}

fn native_service(source: &FixedSource) -> mllm_controller::runtime::NativeLaunchService<'_> {
    static PATH: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    mllm_controller::runtime::NativeLaunchService {
        wrapper: PATH.get_or_init(|| {
            std::path::Path::new("/usr/bin/true")
                .canonicalize()
                .unwrap()
        }),
        now_ms: &|| Ok(1200),
        source,
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
fn native_launch_source_change_during_preflight_rejects_handoff() {
    let fixture = NativeFixture::new();
    let source = fixture.source();
    let result = mllm_controller::runtime::NativeLaunchHandoff::arm(
        &fixture.store,
        &fixture.session,
        &fixture.step,
        fixture.context(),
        &resolve_native_credential,
        &|_| {
            source.replace(fixture.descriptor(SECOND_DIGEST));
            Ok(())
        },
        native_service(&source),
    );
    assert_eq!(
        result.err(),
        Some(RuntimeError::Uncertain("handoff changed".into())),
        "a changed descriptor still produced a launch handoff"
    );
    assert_eq!(
        fixture.store.resource_snapshot().unwrap().owners[&fixture.deployment].phase,
        ResourcePhase::Cold
    );
    fixture.assert_retained();
}

#[test]
fn native_launch_deadline_advanced_during_callbacks_rejects_handoff() {
    for during_preflight in [true, false] {
        let fixture = NativeFixture::new();
        let source = fixture.source();
        let now = std::cell::Cell::new(1200);
        let clock = || Ok(now.get());
        let result = mllm_controller::runtime::NativeLaunchHandoff::arm(
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
            mllm_controller::runtime::NativeLaunchService {
                now_ms: &clock,
                ..native_service(&source)
            },
        );
        assert_eq!(
            result.err(),
            Some(RuntimeError::Uncertain("handoff is stale".into())),
            "expired callbacks produced handoff"
        );
        fixture.assert_retained();
    }
}

#[test]
fn native_launch_change_and_deadline_are_checked_before_spawn_and_acknowledgement() {
    use mllm_launchers::LaunchAssociation;
    for changed in [true, false] {
        let fixture = NativeFixture::new();
        let source = fixture.source();
        let now = std::cell::Cell::new(1200);
        let clock = || Ok(now.get());
        let handoff = mllm_controller::runtime::NativeLaunchHandoff::arm(
            &fixture.store,
            &fixture.session,
            &fixture.step,
            fixture.context(),
            &resolve_native_credential,
            &|_| Ok(()),
            mllm_controller::runtime::NativeLaunchService {
                now_ms: &clock,
                ..native_service(&source)
            },
        )
        .unwrap()
        .unwrap();
        if changed {
            source.replace(fixture.descriptor(SECOND_DIGEST));
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
        assert!(format!("{error:?}").contains("handoff is stale"));
        let error = handoff
            .spawn(&mllm_launchers::DurableSpawn::new())
            .err()
            .unwrap();
        assert_eq!(
            error,
            RuntimeError::Uncertain(if changed {
                "handoff changed".into()
            } else {
                "handoff is stale".into()
            })
        );
        fixture.assert_retained();
    }
}

#[test]
fn native_launch_clock_failure_is_redacted_and_retains_reservations() {
    let fixture = NativeFixture::new();
    let source = fixture.source();
    let calls = std::cell::Cell::new(0);
    let clock = || {
        calls.set(calls.get() + 1);
        if calls.get() == 1 {
            Ok(1200)
        } else {
            Err(RuntimeError::Uncertain("private-clock-detail".into()))
        }
    };
    let error = mllm_controller::runtime::NativeLaunchHandoff::arm(
        &fixture.store,
        &fixture.session,
        &fixture.step,
        fixture.context(),
        &resolve_native_credential,
        &|_| Ok(()),
        mllm_controller::runtime::NativeLaunchService {
            now_ms: &clock,
            ..native_service(&source)
        },
    )
    .err()
    .unwrap();
    assert_eq!(error, RuntimeError::Uncertain("clock unavailable".into()));
    fixture.assert_retained();
}

#[test]
fn native_launch_wrapper_permissions_rechecked_before_spawn_and_acknowledgement() {
    use mllm_launchers::LaunchAssociation;
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = directory.path().join("sglang_entry.py");
    std::fs::write(&path, b"# Never executed by handoff tests.\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let fixture = NativeFixture::new();
    let source = fixture.source();
    let handoff = mllm_controller::runtime::NativeLaunchHandoff::arm(
        &fixture.store,
        &fixture.session,
        &fixture.step,
        fixture.context(),
        &resolve_native_credential,
        &|_| Ok(()),
        mllm_controller::runtime::NativeLaunchService {
            wrapper: &path,
            ..native_service(&source)
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
fn native_launch_handoff_is_single_use_secret_free_and_ordinary_dispatch_stays_closed() {
    use mllm_controller::runtime::NativeLaunchHandoff;
    let fixture = NativeFixture::new();
    let source = fixture.source();
    let handoff = NativeLaunchHandoff::arm(
        &fixture.store,
        &fixture.session,
        &fixture.step,
        fixture.context(),
        &resolve_native_credential,
        &|_| Ok(()),
        native_service(&source),
    )
    .unwrap()
    .unwrap();
    let command = handoff.command();
    assert!(command.env.is_empty());
    assert_eq!(
        &command.argv[1..4],
        [
            "-IS",
            native_service(&source).wrapper.to_str().unwrap(),
            "--public-settings-json"
        ]
    );
    let text = format!("{command:?}");
    for private in [
        "private-inference-token",
        "private-admin-token",
        "secret://",
        fixture.root.to_str().unwrap(),
    ] {
        assert!(!text.contains(private));
    }
    let private: serde_json::Value = serde_json::from_slice(
        &std::fs::read(format!("/proc/self/fd/{}", command.argv[6])).unwrap(),
    )
    .unwrap();
    assert_eq!(private["schema_version"], 2);
    assert_eq!(private["kind"], "sglang_private_launch");
    assert_eq!(private["checkpoint_root"], fixture.root.to_str().unwrap());
    assert_eq!(
        private["public_settings"],
        serde_json::from_str::<serde_json::Value>(&command.argv[4]).unwrap()
    );
    let execution = fixture
        .store
        .initialize_execution(&fixture.session, &fixture.step)
        .unwrap();
    assert_eq!(
        private["launch_scope"],
        serde_json::json!({
            "session_id": fixture.session.id(),
            "deployment_id": execution.token.deployment_id,
            "operation_id": execution.token.operation_id,
            "step_id": execution.token.step_id,
            "revision": execution.token.revision,
            "generation": execution.token.generation,
            "binding_id": execution.binding_id,
            "incarnation": execution.incarnation,
            "issued_at_ms": execution.issued_at_ms,
            "deadline_ms": execution.deadline_ms,
        })
    );
    assert_eq!(private.as_object().unwrap().len(), 5);
    for field in ["session_id", "deployment_id", "operation_id", "step_id"] {
        assert!(!text.contains(private["launch_scope"][field].as_str().unwrap()));
    }
    assert_eq!(
        std::fs::read(format!("/proc/self/fd/{}", command.argv[8])).unwrap(),
        b"private-inference-token"
    );
    assert_eq!(
        std::fs::read(format!("/proc/self/fd/{}", command.argv[10])).unwrap(),
        b"private-admin-token"
    );
    assert!(
        NativeLaunchHandoff::arm(
            &fixture.store,
            &fixture.session,
            &fixture.step,
            fixture.context(),
            &|_| panic!("replay resolved credentials"),
            &|_| panic!("replay preflight"),
            native_service(&source),
        )
        .unwrap()
        .is_none()
    );
    assert!(matches!(
        RuntimeBindings::default().binding(&fixture.deployment, 1),
        Err(RuntimeError::Missing)
    ));
    let stored = fixture
        .store
        .runtime_binding(&fixture.deployment)
        .unwrap()
        .unwrap();
    assert_eq!(stored.state, "uncertain");
    assert!(stored.identities.is_empty());
    assert_eq!(
        fixture.store.resource_snapshot().unwrap().owners[&fixture.deployment].phase,
        ResourcePhase::Cold
    );
    let connection = fixture.sql();
    let flags: (i64, i64) = connection
        .query_row(
            "SELECT admission_enabled,dispatch_enabled FROM deployments WHERE id=?1",
            [&fixture.deployment],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(flags, (1, 0));
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
        NativeLaunchHandoff::arm(
            &fixture.store,
            &fixture.session,
            &fixture.step,
            fixture.context(),
            &resolve_native_credential,
            &|_| Ok(()),
            native_service(&source),
        )
        .unwrap()
        .is_none()
    );
}

#[test]
fn native_launch_preflight_failure_consumes_authority_without_handoff_or_secret_error() {
    use mllm_controller::runtime::NativeLaunchHandoff;
    let fixture = NativeFixture::new();
    let source = fixture.source();
    let checked = std::cell::Cell::new(false);
    let result = NativeLaunchHandoff::arm(
        &fixture.store,
        &fixture.session,
        &fixture.step,
        fixture.context(),
        &|_| panic!("failed preflight resolved credentials"),
        &|_| {
            checked.set(true);
            Err(RuntimeError::Uncertain("private-checkpoint-secret".into()))
        },
        native_service(&source),
    );
    let error = result.err().unwrap();
    assert!(checked.get());
    assert!(!format!("{error:?}").contains("private-checkpoint-secret"));
    assert!(
        NativeLaunchHandoff::arm(
            &fixture.store,
            &fixture.session,
            &fixture.step,
            fixture.context(),
            &resolve_native_credential,
            &|_| Ok(()),
            native_service(&source),
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
fn native_launch_stale_session_cannot_spawn_prepared_handoff() {
    use mllm_controller::runtime::NativeLaunchHandoff;
    let fixture = NativeFixture::new();
    let source = fixture.source();
    let handoff = NativeLaunchHandoff::arm(
        &fixture.store,
        &fixture.session,
        &fixture.step,
        fixture.context(),
        &resolve_native_credential,
        &|_| Ok(()),
        native_service(&source),
    )
    .unwrap()
    .unwrap();
    let _new_session = fixture.store.begin_coordinator_session().unwrap();
    // Revalidation must return before any process creation. Never execute Python here.
    assert!(handoff.spawn(&mllm_launchers::DurableSpawn::new()).is_err());
}

#[test]
fn native_launch_stale_generation_cannot_spawn_prepared_handoff() {
    use mllm_controller::runtime::NativeLaunchHandoff;
    let fixture = NativeFixture::new();
    let source = fixture.source();
    let handoff = NativeLaunchHandoff::arm(
        &fixture.store,
        &fixture.session,
        &fixture.step,
        fixture.context(),
        &resolve_native_credential,
        &|_| Ok(()),
        native_service(&source),
    )
    .unwrap()
    .unwrap();
    fixture.store.bump_generation(&fixture.deployment).unwrap();
    assert!(handoff.spawn(&mllm_launchers::DurableSpawn::new()).is_err());
}

#[test]
fn native_launch_changed_binding_cannot_spawn_prepared_handoff() {
    use mllm_controller::runtime::NativeLaunchHandoff;
    let fixture = NativeFixture::new();
    let source = fixture.source();
    let handoff = NativeLaunchHandoff::arm(
        &fixture.store,
        &fixture.session,
        &fixture.step,
        fixture.context(),
        &resolve_native_credential,
        &|_| Ok(()),
        native_service(&source),
    )
    .unwrap()
    .unwrap();
    fixture
        .sql()
        .execute(
            "UPDATE runtime_bindings SET incarnation=?1",
            [ulid::Ulid::new().to_string()],
        )
        .unwrap();
    assert!(handoff.spawn(&mllm_launchers::DurableSpawn::new()).is_err());
}

#[test]
fn native_launch_ambiguous_api_association_retains_endpoint_grant_and_closed_dispatch() {
    use mllm_controller::runtime::NativeLaunchHandoff;
    use mllm_launchers::LaunchAssociation;
    let fixture = NativeFixture::new();
    let source = fixture.source();
    let handoff = NativeLaunchHandoff::arm(
        &fixture.store,
        &fixture.session,
        &fixture.step,
        fixture.context(),
        &resolve_native_credential,
        &|_| Ok(()),
        native_service(&source),
    )
    .unwrap()
    .unwrap();
    let connection = fixture.sql();
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
    fixture.assert_retained();
    assert!(
        NativeLaunchHandoff::arm(
            &fixture.store,
            &fixture.session,
            &fixture.step,
            fixture.context(),
            &resolve_native_credential,
            &|_| Ok(()),
            native_service(&source),
        )
        .unwrap()
        .is_none()
    );
}

#[test]
fn native_launch_invalid_credentials_never_create_handoff_or_retry() {
    use mllm_controller::runtime::NativeLaunchHandoff;
    for secret in [b"duplicate-token".as_slice(), b"invalid\nsecret", b""] {
        let fixture = NativeFixture::new();
        let source = fixture.source();
        assert!(
            NativeLaunchHandoff::arm(
                &fixture.store,
                &fixture.session,
                &fixture.step,
                fixture.context(),
                &|_| Ok(secret.to_vec()),
                &|_| Ok(()),
                native_service(&source),
            )
            .is_err()
        );
        assert!(
            NativeLaunchHandoff::arm(
                &fixture.store,
                &fixture.session,
                &fixture.step,
                fixture.context(),
                &resolve_native_credential,
                &|_| Ok(()),
                native_service(&source),
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
                identity_id: "identity".into(),
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
        identity_id: "identity-1".into(),
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
