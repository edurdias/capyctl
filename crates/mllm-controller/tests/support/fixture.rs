#![allow(dead_code)]
//! Ordinary-lifecycle test fixture: a store with a host policy and two managed
//! deployments, vacuumed once into an immutable image that each test copies.
//! Nothing here creates a candidate run; ADR 0011 removed the concept.
use mllm_config::effective::resolve_effective;
use mllm_domain::resources::{MemoryLimit, MemoryObservation};
use mllm_scheduler::residency::AdmissionContext;
use mllm_store::Store;
use serde_json::{json, Value};

pub(crate) struct OwnedFixtureSource {
    pub(crate) dir: tempfile::TempDir,
    pub(crate) fence: mllm_store::lifecycle::DeploymentFence,
    pub(crate) other: mllm_store::lifecycle::DeploymentFence,
    pub(crate) observations: Vec<MemoryObservation>,
}

/// Reuse an immutable SQLite image only in tests. Every row in it was written by
/// the real writers below; each caller opens an independent copy.
pub(crate) async fn owned_source() -> &'static OwnedFixtureSource {
    static SOURCE: tokio::sync::OnceCell<OwnedFixtureSource> = tokio::sync::OnceCell::const_new();
    SOURCE
        .get_or_init(|| async {
            let f = fixture();
            let fence = managed(&f, "ordinary");
            let other = managed(&f, "second");
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

pub(crate) struct Fixture {
    pub(crate) store: Store,
    pub(crate) session: mllm_store::dispatch::CoordinatorSession,
    pub(crate) observations: Vec<MemoryObservation>,
    pub(crate) limits: Vec<MemoryLimit>,
    pub(crate) ttl: i64,
    pub(crate) max_parked: usize,
    pub(crate) sql: rusqlite::Connection,
    pub(crate) _dir: tempfile::TempDir,
}

impl Fixture {
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
}

/// A store whose only durable state is the host's imported resource policy.
pub(crate) fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ordinary.db");
    let store = Store::open(&path).unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let source: Value = serde_json::from_str(include_str!(
        "../../../mllm-config/tests/fixtures/effective-fake-golden.json"
    ))
    .unwrap();
    let policy = resolve_effective(&source["input"]["deployment"], &source["input"]["host"])
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
        observations,
        limits,
        ttl: resource.controls.observation_ttl_ms,
        max_parked: resource.controls.max_parked as usize,
        sql: rusqlite::Connection::open(path).unwrap(),
        _dir: dir,
    }
}

pub(crate) fn managed(f: &Fixture, name: &str) -> mllm_store::lifecycle::DeploymentFence {
    managed_edit(f, name, |_, _| {})
}

/// One stopped managed deployment, created through the ordinary writer. The
/// binding it produces derives its identity from the recipe and host alone
/// (ADR 0011 decision 1); nothing here declares one separately.
pub(crate) fn managed_edit(
    f: &Fixture,
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
            &json!({ "config": deployment }).to_string(),
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
