#![allow(dead_code)]
use mllm_adapters::{fake::FakeEngine, traits::EngineAdapter};
use mllm_config::effective::{
    candidate::validate_candidate_reviewed_snapshot_text, resolve_effective,
};
use mllm_controller::{RuntimeAction, RuntimeCommand};
use mllm_domain::completion::OwnedLaunchReceipt;
use mllm_domain::resources::{MemoryLimit, MemoryObservation};
use mllm_scheduler::residency::AdmissionContext;
use mllm_store::{
    candidate_creation::initialize::ArmResult,
    candidate_creation::progression::CandidateDispatchResult, Store,
};
use serde_json::{json, Value};

pub(crate) struct OwnedFixtureSource {
    pub(crate) dir: tempfile::TempDir,
    pub(crate) fence: mllm_store::lifecycle::DeploymentFence,
    pub(crate) other: mllm_store::lifecycle::DeploymentFence,
    pub(crate) observations: Vec<MemoryObservation>,
}

/// Reuse an immutable SQLite image only in tests. Every positive proof was
/// produced by the real writers below; each caller opens an independent copy.
pub(crate) async fn owned_source() -> &'static OwnedFixtureSource {
    static SOURCE: tokio::sync::OnceCell<OwnedFixtureSource> = tokio::sync::OnceCell::const_new();
    SOURCE
        .get_or_init(|| async {
            let (f, catalog) = qualified().await;
            let fence = managed(&f, &catalog);
            let other = managed_edit(&f, &catalog, "second", |_, _| {});
            let dir = tempfile::tempdir().unwrap();
            f.sql
                .execute(
                    "VACUUM INTO ?1",
                    [dir.path().join("srv.sqlite3").to_str().unwrap()],
                )
                .unwrap();
            OwnedFixtureSource {
                dir,
                fence,
                other,
                observations: f.observations.clone(),
            }
        })
        .await
}

pub(crate) fn marker_body(f: &Fixture, marker: &str, stream: bool) -> String {
    json!({"model":format!("candidate-{}", f.created.deployment_id()),"messages":[{"role":"user","content":format!("Repeat exactly: {marker}")}],"temperature":0,"max_tokens":16,"stream":stream}).to_string()
}

pub(crate) struct Fixture {
    pub(crate) store: Store,
    pub(crate) session: mllm_store::dispatch::CoordinatorSession,
    pub(crate) created: mllm_store::candidate_creation::CandidateCreationReceipt,
    pub(crate) init: mllm_store::candidate_creation::progression::CandidateActionReceipt,
    pub(crate) observations: Vec<MemoryObservation>,
    pub(crate) limits: Vec<MemoryLimit>,
    pub(crate) ttl: i64,
    pub(crate) max_parked: usize,
    pub(crate) sql: rusqlite::Connection,
    pub(crate) _dir: tempfile::TempDir,
}
impl Fixture {
    pub(crate) fn counts(&self) -> Vec<i64> {
        [
            "operations",
            "lifecycle_runs",
            "lifecycle_steps",
            "lifecycle_evidence",
            "command_receipts",
            "qualifications",
            "qualification_request_attempts",
            "qualification_request_results",
            "qualification_evidence_refs",
            "qualification_parked_status",
            "resource_owners",
            "resource_grants",
            "request_leases",
            "endpoint_leases",
            "management_events",
        ]
        .into_iter()
        .map(|table| self.scalar(&format!("SELECT COUNT(*) FROM {table}")))
        .collect()
    }
    pub(crate) fn fresh_binding(
        &self,
        receipt: &mllm_store::qualification::QualificationReceipt,
    ) -> (mllm_store::lifecycle::DeploymentFence, String) {
        use mllm_domain::{DeploymentId, LifecycleState, OperationId};
        let id = DeploymentId::new();
        self.store
            .accept_deployment(mllm_store::AcceptDeployment {
                id,
                name: format!("ordinary-{id}"),
                kind: "model".into(),
                route_model_id: None,
                desired_state: LifecycleState::Stopped,
                schema_version: 1,
                idempotency_key: format!("ordinary-{id}"),
                initial_operation_id: OperationId(format!("ordinary-{id}")),
            })
            .unwrap();
        let fence = mllm_store::lifecycle::DeploymentFence {
            deployment_id: id.to_string(),
            revision: 1,
            generation: 1,
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let binding = format!("ordinary-binding-{id}");
        self.store
            .reserve_runtime_binding(
                &self.session,
                &mllm_store::lifecycle::ReserveBinding {
                    id: binding.clone(),
                    fence: fence.clone(),
                    incarnation: format!("ordinary-incarnation-{id}"),
                    qualification_id: receipt.qualification_id().into(),
                    ownership: "managed".into(),
                    endpoint_host: "127.0.0.1".into(),
                    endpoint_port: port,
                    credential_ref: "secret://ordinary-runtime".into(),
                    binding_payload: receipt.binding_payload().unwrap(),
                },
            )
            .unwrap();
        (fence, binding)
    }
    pub(crate) async fn completed_suite(
        &self,
        fake: &FakeEngine,
    ) -> (
        mllm_store::candidate_creation::progression::CandidateCollector,
        Vec<mllm_domain::qualification::CandidateRequestObservation>,
        Vec<mllm_domain::qualification::CandidateParkedStatusObservation>,
    ) {
        let collector = self.secured(fake).await;
        let mut requests = Vec::new();
        let mut statuses = Vec::new();
        for (action, effects) in [
            ("park", vec![RuntimeAction::Drain, RuntimeAction::Park]),
            (
                "restore",
                vec![
                    RuntimeAction::Restore,
                    RuntimeAction::ReloadWeights,
                    RuntimeAction::InvalidateCache,
                ],
            ),
        ] {
            let body =
                json!({"expected_revision":1,"action":action,"deadline_ms":400000}).to_string();
            let receipt = self
                .store
                .accept_candidate_action(
                    &self.session,
                    "owner",
                    self.created.run_id(),
                    action,
                    &body,
                    1200,
                )
                .unwrap();
            for (index, action) in effects.into_iter().enumerate() {
                let child = &receipt.effect_ids()[index];
                assert!(matches!(
                    self.store
                        .arm_candidate_effect(&self.session, child, self.admission())
                        .unwrap(),
                    ArmResult::New { .. }
                ));
                let (_, context) = self
                    .store
                    .candidate_effect_execution(&self.session, child)
                    .unwrap();
                let observation = fake
                    .execute_persisted(&RuntimeCommand { action, context })
                    .await
                    .unwrap();
                self.store
                    .record_candidate_effect(&self.session, &collector, child, &observation, 1300)
                    .unwrap();
            }
            if action == "park" {
                let context = self
                    .store
                    .candidate_parked_status_execution(&self.session, receipt.step_id(), 1200)
                    .unwrap();
                let observation =
                    mllm_controller::qualification::collect_parked_status(fake, &context).unwrap();
                self.store
                    .record_candidate_parked_status(&self.session, &collector, &observation, 1300)
                    .unwrap();
                statuses.push(observation);
            } else {
                let CandidateDispatchResult::New(dispatch) = self
                    .store
                    .arm_candidate_probe(&self.session, receipt.step_id(), self.admission())
                    .unwrap()
                else {
                    panic!()
                };
                let observation = mllm_controller::qualification::collect_probe(fake, *dispatch)
                    .await
                    .unwrap();
                self.store
                    .record_candidate_result(&self.session, &collector, &observation, 1300)
                    .unwrap();
                requests.push(observation);
            }
        }
        for (index, (marker, stream)) in [
            ("MLLM_ALPHA_71", false),
            ("MLLM_BETA_29", false),
            ("MLLM_ALPHA_71", true),
            ("MLLM_BETA_29", true),
        ]
        .into_iter()
        .enumerate()
        {
            let CandidateDispatchResult::New(dispatch) = self
                .store
                .grant_candidate_inference(
                    &self.session,
                    "owner",
                    self.created.run_id(),
                    &format!("warm-{index}"),
                    &marker_body(self, marker, stream),
                    self.admission(),
                )
                .unwrap()
            else {
                panic!()
            };
            let observation = mllm_controller::qualification::collect_probe(fake, *dispatch)
                .await
                .unwrap();
            self.store
                .record_candidate_result(&self.session, &collector, &observation, 1300)
                .unwrap();
            requests.push(observation);
        }
        (collector, requests, statuses)
    }
    pub(crate) async fn secured(
        &self,
        fake: &FakeEngine,
    ) -> mllm_store::candidate_creation::progression::CandidateCollector {
        use mllm_store::candidate_creation::progression::CandidateSecurityDispatch;
        let collector = self.baseline(fake).await;
        loop {
            match self
                .store
                .advance_candidate_security(&self.session, &collector, self.admission())
                .unwrap()
            {
                CandidateSecurityDispatch::NewControl(d) => {
                    let o = mllm_controller::qualification::collect_security_control(fake, *d)
                        .await
                        .unwrap();
                    self.store
                        .record_candidate_security_control(&self.session, &collector, &o, 1300)
                        .unwrap();
                }
                CandidateSecurityDispatch::NewRequest(d) => {
                    let o = mllm_controller::qualification::collect_probe(fake, *d)
                        .await
                        .unwrap();
                    self.store
                        .record_candidate_result(&self.session, &collector, &o, 1300)
                        .unwrap();
                }
                CandidateSecurityDispatch::AlreadyRecorded { complete: true, .. } => break,
                _ => panic!("unfinished Security"),
            }
        }
        collector
    }
    pub(crate) async fn baseline(
        &self,
        fake: &FakeEngine,
    ) -> mllm_store::candidate_creation::progression::CandidateCollector {
        let collector = self.ready(fake).await;
        for (i, (marker, stream)) in [
            ("MLLM_ALPHA_71", false),
            ("MLLM_BETA_29", false),
            ("MLLM_ALPHA_71", true),
            ("MLLM_BETA_29", true),
        ]
        .into_iter()
        .enumerate()
        {
            let CandidateDispatchResult::New(d) = self
                .store
                .grant_candidate_inference(
                    &self.session,
                    "owner",
                    self.created.run_id(),
                    &format!("baseline-{i}"),
                    &marker_body(self, marker, stream),
                    self.admission(),
                )
                .unwrap()
            else {
                panic!()
            };
            let o = mllm_controller::qualification::collect_probe(fake, *d)
                .await
                .unwrap();
            self.store
                .record_candidate_result(&self.session, &collector, &o, 1300)
                .unwrap();
        }
        collector
    }
    pub(crate) async fn ready(
        &self,
        fake: &FakeEngine,
    ) -> mllm_store::candidate_creation::progression::CandidateCollector {
        let collector = self.initialized(fake).await;
        let CandidateDispatchResult::New(dispatch) = self
            .store
            .arm_candidate_probe(&self.session, self.init.step_id(), self.admission())
            .unwrap()
        else {
            panic!()
        };
        let result = mllm_controller::qualification::collect_probe(fake, *dispatch)
            .await
            .unwrap();
        self.store
            .record_candidate_result(&self.session, &collector, &result, 1300)
            .unwrap();
        collector
    }
    pub(crate) fn admission(&self) -> AdmissionContext<'_> {
        AdmissionContext::new(
            &self.observations,
            &self.limits,
            1200,
            self.ttl,
            self.max_parked,
        )
    }
    pub(crate) fn scalar(&self, sql: &str) -> i64 {
        self.sql.query_row(sql, [], |r| r.get(0)).unwrap()
    }
    pub(crate) async fn initialized(
        &self,
        fake: &FakeEngine,
    ) -> mllm_store::candidate_creation::progression::CandidateCollector {
        assert!(matches!(
            self.store
                .arm_candidate_effect(&self.session, &self.init.effect_ids()[0], self.admission())
                .unwrap(),
            ArmResult::New { .. }
        ));
        let (_, context) = self
            .store
            .candidate_effect_execution(&self.session, &self.init.effect_ids()[0])
            .unwrap();
        let observation = fake
            .execute_persisted(&RuntimeCommand {
                action: RuntimeAction::Initialize,
                context,
            })
            .await
            .unwrap();
        self.store
            .record_owned_launch(
                &self.session,
                self.init.step_id(),
                &OwnedLaunchReceipt {
                    binding_id: observation.binding_id.clone(),
                    incarnation: observation.incarnation.clone(),
                    identities: observation.identities.clone(),
                    observed_at_ms: observation.observed_at_ms,
                    receipt: observation.receipt.clone(),
                },
                1250,
            )
            .unwrap();
        let collector = self
            .store
            .candidate_collector(&self.session, "owner", self.created.run_id())
            .unwrap();
        self.store
            .record_candidate_effect(
                &self.session,
                &collector,
                &self.init.effect_ids()[0],
                &observation,
                1250,
            )
            .unwrap();
        collector
    }
}
pub(crate) fn fixture() -> Fixture {
    fixture_peaks(9663676416, 10737418240)
}
pub(crate) fn fixture_peaks(parking: i64, wake: i64) -> Fixture {
    fixture_custom(parking, wake, |_| {})
}
pub(crate) fn fixture_custom(parking: i64, wake: i64, edit: impl FnOnce(&mut Value)) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("qualification.db");
    let store = Store::open(&path).unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let ordinary: Value = serde_json::from_str(include_str!(
        "../../../mllm-config/tests/fixtures/effective-fake-golden.json"
    ))
    .unwrap();
    let mut manifest: Value = serde_json::from_str(include_str!(
        "../../../mllm-config/tests/fixtures/candidate-fake-qualification.json"
    ))
    .unwrap();
    manifest["limits"]["max_run_duration_ms"] = json!(600000);
    manifest["limits"]["max_cleanup_duration_ms"] = json!(60000);
    manifest["effective_recipe"]["resources"]["parking"]["allocations"][0]["bytes"] =
        json!(parking);
    manifest["effective_recipe"]["resources"]["wake"]["allocations"][0]["bytes"] = json!(wake);
    edit(&mut manifest);
    let reviewed = validate_candidate_reviewed_snapshot_text(&manifest.to_string()).unwrap();
    let mut host = ordinary["input"]["host"].clone();
    host["runtime_profiles"]["local"]["build_fingerprint"] = json!("qualification-fake-v1");
    host["runtime_profiles"]["local"]["security"]["admin_credential_ref"] =
        json!("secret://admin-key");
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
    let body = json!({"host_id":"lab","expected_host_revision":1,"recipe_digest":reviewed.manifest_digest(),"manifest":manifest,"deadline_ms":500000,"allow_owned_abort_cleanup":true}).to_string();
    let created = store
        .create_candidate_run(&session, "owner", "create", &body, &host, 1000)
        .unwrap();
    let init = store
        .accept_candidate_action(
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
    Fixture {
        store,
        session,
        created,
        init,
        observations,
        limits,
        ttl: resource.controls.observation_ttl_ms,
        max_parked: resource.controls.max_parked as usize,
        sql: rusqlite::Connection::open(path).unwrap(),
        _dir: dir,
    }
}

pub(crate) async fn qualified() -> (Fixture, mllm_store::qualification::QualificationReceipt) {
    let f = fixture();
    let fake = FakeEngine::for_qualification();
    f.completed_suite(&fake).await;
    let catalog = f
        .store
        .finish_candidate_run(
            &f.session,
            "owner",
            f.created.run_id(),
            "finish",
            r#"{"expected_revision":1,"action":"finish"}"#,
            1400,
        )
        .unwrap();
    let before_cleanup = managed_edit(&f, &catalog, "before-cleanup", |_, _| {});
    let before = f.counts();
    assert!(f
        .store
        .accept_qualified_start(&f.session, &before_cleanup, 1800, 10000)
        .is_err());
    assert_eq!(f.counts(), before);
    let cleanup = f
        .store
        .accept_candidate_cleanup(
            &f.session,
            "owner",
            f.created.run_id(),
            "cleanup",
            r#"{"expected_revision":1,"action":"cleanup","deadline_ms":60000}"#,
            1500,
        )
        .unwrap();
    f.store
        .arm_candidate_cleanup(&f.session, cleanup.step_id(), 1550)
        .unwrap();
    let context = f
        .store
        .candidate_cleanup_execution(&f.session, cleanup.step_id())
        .unwrap();
    let gone = mllm_controller::qualification::collect_cleanup(&fake, &context, 1600).unwrap();
    f.store
        .complete_cleanup(&f.session, cleanup.step_id(), &gone, 1650, f.ttl)
        .unwrap();
    (f, catalog)
}

pub(crate) fn managed(
    f: &Fixture,
    catalog: &mllm_store::qualification::QualificationReceipt,
) -> mllm_store::lifecycle::DeploymentFence {
    managed_edit(f, catalog, "ordinary", |_, _| {})
}
pub(crate) fn managed_edit(
    f: &Fixture,
    catalog: &mllm_store::qualification::QualificationReceipt,
    name: &str,
    edit: impl FnOnce(&mut Value, &mut Value),
) -> mllm_store::lifecycle::DeploymentFence {
    let source: Value = serde_json::from_str(include_str!(
        "../../../mllm-config/tests/fixtures/effective-fake-golden.json"
    ))
    .unwrap();
    let mut host = source["input"]["host"].clone();
    host["runtime_profiles"]["local"]["build_fingerprint"] = json!("qualification-fake-v1");
    host["runtime_profiles"]["local"]["security"]["admin_credential_ref"] =
        json!("secret://another-admin");
    host["runtime_profiles"]["local"]["qualification_id"] =
        json!(format!("qualified:{}", catalog.qualification_id()));
    let mut deployment = source["input"]["deployment"].clone();
    deployment["name"] = json!(name);
    deployment["routes"] = json!([name]);
    edit(&mut deployment, &mut host);
    let receipt = f
        .store
        .create_stopped_managed_configuration(
            &f.session,
            "owner",
            name,
            &json!({"config":deployment}).to_string(),
            &host,
            1700,
        )
        .unwrap();
    mllm_store::lifecycle::DeploymentFence {
        deployment_id: receipt.deployment_id,
        revision: receipt.revision,
        generation: receipt.generation,
    }
}
