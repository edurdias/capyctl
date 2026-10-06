//! ADR 0028 §2, §3: a multi-node group deployment resolved on every named
//! host at deploy time: the head's resolution is canonical, every member keeps
//! its rank-ordered host list, and a named host that cannot run the group
//! refuses the whole deploy with its closed code before anything is stored.
//!
//! CPU-only: nothing launches. Passing here never qualifies a group recipe;
//! the live MN rows do (SPEC §18).
use capyctl_config::remote_resources::scope_host_document;
use capyctl_domain::resources::MemoryObservation;
use capyctl_store::dispatch::CoordinatorSession;
use capyctl_store::managed_configuration::{
    HostTarget, ManagedConfigurationError, ManagedConfigurationReceipt,
};
use capyctl_store::Store;
use rusqlite::{params, Connection};
use serde_json::{json, Value};

const NOW: i64 = 10_000;
/// Host id, enrolled name and peer address; the head is host B, so the
/// canonical resolution is the head's, not the first host id.
const HOSTS: [(&str, &str, &str); 2] = [
    ("host-a", "spark-a", "192.0.2.10"),
    ("host-b", "spark-b", "192.0.2.11"),
];

struct World {
    _dir: tempfile::TempDir,
    store: Store,
    sql: Connection,
    session: CoordinatorSession,
    config: Value,
    hosts: Vec<(String, String, Value)>,
}

fn golden() -> (Value, Value) {
    let golden: Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    (
        golden["input"]["deployment"].clone(),
        golden["input"]["host"].clone(),
    )
}

/// Two enrolled hosts with the golden host policy and their peer addresses,
/// and a TP 2 group naming host B (spark-b) as its head. The golden
/// deployment names device `gpu0`: one device per host (R20).
fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("groups.sqlite3");
    let store = Store::open(&path).unwrap();
    let sql = Connection::open(&path).unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let (mut config, host) = golden();
    let mut hosts = Vec::new();
    for (id, name, peer) in HOSTS {
        sql.execute(
            "INSERT INTO enrolled_hosts(host_id,host_name,key_digest,revoked) VALUES(?1,?2,'key',0)",
            params![id, name],
        )
        .unwrap();
        let policy = capyctl_config::effective::normalize_host_policy(&host).unwrap();
        store
            .import_remote_resource_policy(&session, id, &policy, &[observed()], 1)
            .unwrap();
        let mut document = host.clone();
        document["resource_policy"]["groups"] = json!({ "peer_address": peer });
        hosts.push((id.to_owned(), name.to_owned(), document));
    }
    config.as_object_mut().unwrap().remove("host");
    config["topology"] = json!({"tensor_parallel": 2});
    config["placement"] = json!({"hosts": ["spark-b", "spark-a"]});
    World {
        _dir: dir,
        store,
        sql,
        session,
        config,
        hosts,
    }
}

fn observed() -> MemoryObservation {
    MemoryObservation {
        domain: "unified".into(),
        capacity_bytes: 1 << 40,
        available_bytes: 1 << 40,
        sampled_at_ms: 1,
    }
}

impl World {
    fn document(&mut self, id: &str) -> &mut Value {
        &mut self.hosts.iter_mut().find(|(h, ..)| h == id).unwrap().2
    }

    /// The registry's view of each enrolled host: its published document
    /// scoped to its ledger keys and composed with its current controls.
    fn targets(&self) -> Vec<HostTarget> {
        self.hosts
            .iter()
            .map(|(id, name, document)| {
                let trusted = scope_host_document(id, document).unwrap();
                let policy = self.store.resource_policy(id).unwrap().unwrap();
                HostTarget {
                    host_id: id.clone(),
                    host_name: name.clone(),
                    trusted_host: capyctl_config::effective::compose_current_resource_controls(
                        &trusted,
                        &policy.context,
                        &policy.controls,
                    )
                    .unwrap(),
                    scoped: true,
                }
            })
            .collect()
    }

    fn deploy_on(
        &self,
        key: &str,
        targets: &[HostTarget],
    ) -> Result<ManagedConfigurationReceipt, ManagedConfigurationError> {
        self.store.create_managed_configuration_on_hosts(
            &self.session,
            "owner",
            key,
            &json!({ "config": self.config }).to_string(),
            targets,
            &[],
            NOW,
        )
    }

    fn deploy(&self, key: &str) -> Result<ManagedConfigurationReceipt, ManagedConfigurationError> {
        self.deploy_on(key, &self.targets())
    }

    /// The closed reason a refused deploy names; nothing was stored.
    fn refusal(&self, key: &str) -> String {
        let error = self.deploy(key).unwrap_err();
        assert_eq!(self.store.deployment_count().unwrap(), 0, "{error}");
        match error {
            ManagedConfigurationError::Rejected(error) => error.to_string(),
            other => panic!("expected a configuration refusal, got {other:?}"),
        }
    }
}

// T03, T14: a group resolves on every named host; the head's resolution is the
// canonical revision, and each host's recipe keeps the rank-ordered host list.
#[test]
fn a_group_resolves_on_every_named_host_with_the_head_canonical() {
    let w = world();
    let receipt = w.deploy("group").unwrap();
    let canonical: String = w
        .sql
        .query_row(
            "SELECT json_extract(effective_json,'$.host.name') FROM effective_revisions WHERE deployment_id=?1",
            [&receipt.deployment_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(canonical, "host-b", "the head's resolution is canonical");
    let rows: Vec<(String, String, String)> = w
        .sql
        .prepare(
            "SELECT host_id,outcome,source_json FROM host_effective_revisions WHERE deployment_id=?1 ORDER BY host_id",
        )
        .unwrap()
        .query_map([&receipt.deployment_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(rows.len(), 2);
    for (host, outcome, source) in rows {
        assert_eq!(outcome, "resolved", "{host}");
        let source: Value = serde_json::from_str(&source).unwrap();
        assert_eq!(source["placement"]["hosts"], json!(["spark-b", "spark-a"]));
        assert_eq!(source["topology"]["tensor_parallel"], 2);
        let spec = capyctl_config::instances::parse_instance_spec(&source).unwrap();
        assert_eq!(spec.group.unwrap().head(), "spark-b");
    }
    let accepted: String = w
        .sql
        .query_row(
            "SELECT config_json FROM managed_configuration_sources WHERE deployment_id=?1",
            [&receipt.deployment_id],
            |r| r.get(0),
        )
        .unwrap();
    let accepted: Value = serde_json::from_str(&accepted).unwrap();
    assert_eq!(
        accepted["placement"]["hosts"],
        json!(["spark-b", "spark-a"])
    );
}

// T14: one mismatched build, or a profile one named host lacks, refuses the deploy.
#[test]
fn every_named_host_runs_the_same_profile_build() {
    let mut w = world();
    w.document("host-a")["runtime_profiles"]["local"]["build_fingerprint"] = json!("other");
    let refusal = w.refusal("build");
    assert!(refusal.contains("group_profile_mismatch"), "{refusal}");
    assert!(refusal.contains("host-a"), "{refusal}");
    let mut w = world();
    let profile = w.document("host-a")["runtime_profiles"]["local"].take();
    w.document("host-a")["runtime_profiles"] = json!({ "other": profile });
    let refusal = w.refusal("missing");
    assert!(refusal.contains("group_profile_mismatch"), "{refusal}");
}

// T14: a named host without a peer address cannot be in a group.
#[test]
fn a_named_host_without_a_peer_address_is_refused() {
    let mut w = world();
    w.document("host-a")["resource_policy"]
        .as_object_mut()
        .unwrap()
        .remove("groups");
    let refusal = w.refusal("peer");
    assert!(refusal.contains("peer_address_missing"), "{refusal}");
    assert!(refusal.contains("spark-a"), "{refusal}");
}

// T14, T37: an engine env name must be approved on every named host.
#[test]
fn engine_env_is_approved_on_every_named_host() {
    let mut w = world();
    w.config["engine_config"]["env"] = json!({"VLLM_GROUP_PROBE": "1"});
    w.document("host-b")["runtime_profiles"]["local"]["security"]["approved_env"] =
        json!(["VLLM_GROUP_*"]);
    let refusal = w.refusal("env");
    assert!(
        refusal.contains("engine_env_not_approved:VLLM_GROUP_PROBE"),
        "{refusal}"
    );
    w.document("host-a")["runtime_profiles"]["local"]["security"]["approved_env"] =
        json!(["VLLM_GROUP_*"]);
    w.deploy("env-approved").unwrap();
}

// T34 (main #65): the capability gate's decision is made for every member; a
// deep SGLang group whose modelopt weights no rank can reload is refused.
#[test]
fn a_member_the_engine_cannot_park_refuses_the_group() {
    let mut w = world();
    for (id, ..) in HOSTS {
        let profile = &mut w.document(id)["runtime_profiles"]["local"];
        profile["engine"] = json!("sglang");
        profile["args"] = json!([]);
        profile["security"]["admin_credential_ref"] = json!("secret://engine-admin");
    }
    w.config["engine_config"]["quantization"] = json!("modelopt_fp4");
    let refusal = w.refusal("modelopt");
    assert!(
        refusal.contains("capability_missing:deep_park"),
        "{refusal}"
    );
    // T39: a single-host deploy of the same document is still accepted (its
    // host refuses the launch), so the refusal above is the group's gate.
    let mut single = w.config.clone();
    single.as_object_mut().unwrap().remove("topology");
    single["placement"] = json!({"hosts": ["spark-b"]});
    w.store
        .create_managed_configuration_on_hosts(
            &w.session,
            "owner",
            "modelopt-single",
            &json!({ "config": single }).to_string(),
            &w.targets(),
            &[],
            NOW,
        )
        .unwrap();
    w.config["name"] = json!("toy-restart");
    w.config["routes"] = json!(["toy-restart"]);
    w.config["residency"] = json!("restart_only");
    w.deploy("modelopt-restart").unwrap();
}

// T14: a named host the server cannot resolve against refuses the group; it is
// never deployed on the hosts that remain.
#[test]
fn a_named_host_without_a_target_refuses_the_group() {
    let w = world();
    let head_only: Vec<HostTarget> = w
        .targets()
        .into_iter()
        .filter(|t| t.host_id == "host-b")
        .collect();
    assert!(w.deploy_on("head-only", &head_only).is_err());
    assert_eq!(w.store.deployment_count().unwrap(), 0);
}
