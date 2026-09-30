use super::*;
use crate::migrations::{apply, MIGRATIONS};
use crate::Store;
use rusqlite::Connection;
use serde_json::{json, Value};

fn v21_store() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    for (index, sql) in MIGRATIONS.iter().take(21).enumerate() {
        conn.execute_batch(sql).unwrap();
        conn.execute(
            "INSERT INTO schema_migrations(version) VALUES(?1)",
            [(index + 1) as i64],
        )
        .unwrap();
    }
    conn
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).unwrap()
}

/// ADR 0013 §5: the upgrade is additive. A running pre-instance deployment
/// becomes instance 0 on its current host with its generation, and its binding,
/// claim, run, request lease and reservation keep their identity. Reapplying
/// the migration (a rolled-back store) changes nothing.
// T08 T16 T32
#[test]
fn v22_makes_every_deployment_instance_zero_and_keeps_its_accounting() {
    let conn = v21_store();
    conn.execute_batch(
        r#"INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version,revision) VALUES('a','a','model','ready',1,0,3,1,1),('b','b','model','stopped',0,0,1,1,1);
        INSERT INTO effective_revisions VALUES('a',1,'{"host":{"name":"host-a"}}','fa'),('b',1,'not json','fb');
        INSERT INTO operations(id,deployment_id,kind,state) VALUES('op','a','initialize','running');
        INSERT INTO lifecycle_runs(operation_id,deployment_id,revision,generation,session_id,action,state,deadline_ms,plan_json) VALUES('op','a',1,3,'s','activate','running',1,'{}');
        INSERT INTO lifecycle_claims(deployment_id,operation_id,revision,generation) VALUES('a','op',1,3);
        INSERT INTO runtime_bindings(id,deployment_id,revision,incarnation,ownership,binding_json,identities_json,state) VALUES('binding','a',1,'inc','managed','{}','[]','live');
        INSERT INTO request_leases(id,deployment_id,revision,generation,session_id,disposition) VALUES('lease','a',1,3,'s','inflight');
        INSERT INTO resource_owners(owner_id,footprint_json) VALUES('a','{"kept":true}');"#,
    )
    .unwrap();
    apply(&conn).unwrap();
    apply(&conn).unwrap();
    let instance: (i64, Option<String>, Option<i64>, String, i64) = conn
        .query_row(
            "SELECT instance_index,host_id,generation,state,operator_stopped FROM deployment_instances WHERE deployment_id='a'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(
        instance,
        (0, Some("host-a".into()), Some(3), "active".into(), 0)
    );
    // A revision whose host cannot be read still gets its instance, unplaced.
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM deployment_instances WHERE deployment_id='b' AND instance_index=0 AND host_id IS NULL"),
        1
    );
    for table in [
        "runtime_bindings",
        "lifecycle_runs",
        "lifecycle_claims",
        "request_leases",
        "resource_owners",
    ] {
        assert_eq!(
            count(
                &conn,
                &format!(
                    "SELECT COUNT(*) FROM {table} WHERE deployment_id='a' AND instance_index=0"
                )
            ),
            1,
            "{table}"
        );
    }
    let owner: (String, String) = conn
        .query_row(
            "SELECT owner_id,footprint_json FROM resource_owners",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(owner, ("a".into(), r#"{"kept":true}"#.into()));
    let placement: (i64, String) = conn
        .query_row(
            "SELECT instances,placement_json FROM deployment_revision_instances WHERE deployment_id='a'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(placement.0, 1);
    let placement: Placement = serde_json::from_str(&placement.1).unwrap();
    assert_eq!(placement.pinned_host(), Some("host-a"));
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM host_effective_revisions WHERE deployment_id='a' AND host_id='host-a' AND fingerprint='fa'"),
        1
    );
    // A deployment created after the upgrade gets instance 0 by itself.
    conn.execute_batch("INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version) VALUES('c','c','model','stopped',0,0,1,1);").unwrap();
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM deployment_instances WHERE deployment_id='c'"
        ),
        1
    );
}

/// ADR 0013 §5: one retained binding, one claim, one open activation and one
/// resource owner per instance, not per deployment.
// T15 T16 T29
#[test]
fn uniqueness_rules_hold_per_instance() {
    let conn = v21_store();
    apply(&conn).unwrap();
    conn.execute_batch(
        r#"INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version) VALUES('a','a','model','ready',1,0,1,1);
        INSERT INTO deployment_instances(deployment_id,instance_index) VALUES('a',1);
        INSERT INTO runtime_bindings(id,deployment_id,revision,incarnation,ownership,binding_json,identities_json,state,instance_index) VALUES('b0','a',1,'i0','managed','{}','[]','live',0),('b1','a',1,'i1','managed','{}','[]','live',1);
        INSERT INTO operations(id,deployment_id,kind,state) VALUES('o0','a','initialize','running'),('o1','a','initialize','running'),('o2','a','initialize','running');
        INSERT INTO lifecycle_runs(operation_id,deployment_id,revision,generation,session_id,action,state,deadline_ms,plan_json,instance_index) VALUES('o0','a',1,1,'s','activate','running',1,'{}',0),('o1','a',1,2,'s','activate','running',1,'{}',1);
        INSERT INTO lifecycle_claims(deployment_id,operation_id,revision,generation,instance_index) VALUES('a','o0',1,1,0),('a','o1',1,2,1);
        INSERT INTO resource_owners(owner_id,footprint_json,deployment_id,instance_index) VALUES('a','{}','a',0),('deployment:a/instance:1','{}','a',1);"#,
    )
    .unwrap();
    for refused in [
        "INSERT INTO runtime_bindings(id,deployment_id,revision,incarnation,ownership,binding_json,identities_json,state,instance_index) VALUES('b2','a',1,'i2','managed','{}','[]','reserved',1)",
        "INSERT INTO lifecycle_claims(deployment_id,operation_id,revision,generation,instance_index) VALUES('a','o2',1,3,0)",
        "INSERT INTO lifecycle_runs(operation_id,deployment_id,revision,generation,session_id,action,state,deadline_ms,plan_json,instance_index) VALUES('o2','a',1,2,'s','activate','queued',1,'{}',1)",
        // The owner id must name its instance.
        "INSERT INTO resource_owners(owner_id,footprint_json,deployment_id,instance_index) VALUES('a-2','{}','a',2)",
    ] {
        assert!(conn.execute(refused, []).is_err(), "{refused}");
    }
}

/// ADR 0013 §5 (I2): a deployment's instance 0 starts with the deployment's
/// own revision, generation and state, and stays unplaced until the scheduler
/// places it: no host is inferred from the canonical revision any more.
// T18
#[test]
fn a_new_deployment_starts_instance_zero_on_its_own_fence_unplaced() {
    let conn = v21_store();
    apply(&conn).unwrap();
    conn.execute_batch(
        r#"INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version) VALUES('a','a','model','ready',1,0,5,1);
        INSERT INTO effective_revisions VALUES('a',1,'{"host":{"name":"host-b"}}','f');
        INSERT INTO operations(id,deployment_id,kind,state) VALUES('o','a','initialize','running');
        INSERT INTO lifecycle_runs(operation_id,deployment_id,revision,generation,session_id,action,state,deadline_ms,plan_json) VALUES('o','a',1,5,'s','activate','running',1,'{}');"#,
    )
    .unwrap();
    let row: (Option<i64>, Option<i64>, Option<String>, String, i64) = conn
        .query_row(
            "SELECT generation,revision,host_id,desired_state,admission_enabled FROM deployment_instances WHERE deployment_id='a' AND instance_index=0",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(row, (Some(5), Some(1), None, "ready".into(), 1));
}

#[test]
fn owner_ids_name_their_instance() {
    assert_eq!(instance_owner_id("d", 0), "d");
    assert_eq!(instance_owner_id("d", 2), "deployment:d/instance:2");
    for k in [0, 1, 7] {
        assert_eq!(
            parse_instance_owner_id(&instance_owner_id("d", k)),
            ("d".into(), k)
        );
    }
    assert_eq!(
        parse_instance_owner_id("deployment:d/instance:01"),
        ("deployment:d/instance:01".into(), 0)
    );
}

fn fixture() -> (Value, Value) {
    let value: Value = serde_json::from_str(include_str!(
        "../../../capyctl-config/tests/fixtures/f2-deployment.json"
    ))
    .unwrap();
    (value["deployment"].clone(), value["host"].clone())
}

fn setup() -> (Store, crate::dispatch::CoordinatorSession, Value, Value) {
    let (config, host) = fixture();
    let store = Store::open_in_memory().unwrap();
    let session = store.begin_coordinator_session().unwrap();
    let effective = capyctl_config::effective::resolve_effective(&config, &host).unwrap();
    store
        .import_resource_policy(
            &session,
            &effective.host,
            &[capyctl_domain::resources::MemoryObservation {
                domain: "unified".into(),
                capacity_bytes: 64 << 30,
                available_bytes: 60 << 30,
                sampled_at_ms: 1,
            }],
            1,
        )
        .unwrap();
    (store, session, config, host)
}

/// ADR 0013 §7 (count change is a revision) and §3 (validation): an accepted
/// revision records its count and placement and brings the instance rows to
/// `0..N-1`; a later revision with a smaller count retires the surplus rows.
// T09 T10 T14
#[test]
fn accepted_revisions_record_count_and_instance_rows() {
    let (store, session, mut config, host) = setup();
    config["instances"] = json!(3);
    let first = store
        .create_stopped_managed_configuration(
            &session,
            "p",
            "k1",
            &json!({"config":config}).to_string(),
            &host,
            1,
        )
        .unwrap();
    let id = first.deployment_id.clone();
    let rows = store.deployment_instances(&id).unwrap();
    assert_eq!(
        rows.iter().map(|r| r.index).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    // Instance 0 is fenced by the receipt's generation; the others draw one
    // when they first start. None is placed at deploy time (ADR 0013 §3).
    assert!(rows
        .iter()
        .all(|r| r.host_id.is_none() && !r.operator_stopped));
    assert_eq!(
        rows.iter().map(|r| r.generation).collect::<Vec<_>>(),
        vec![Some(first.generation), None, None]
    );
    let spec = store.revision_instances(&id, 1).unwrap().unwrap();
    assert_eq!(spec.instances, 3);
    assert!(spec.placement.hosts.is_none());
    config["instances"] = json!(1);
    config["placement"] = json!({"hosts": [host["name"].clone()], "strategy": "pack"});
    let second = store
        .replace_stopped_managed_configuration(
            &session,
            "p",
            "k2",
            &id,
            &json!({"config":config,"expected_revision":1}).to_string(),
            &host,
            2,
        )
        .unwrap();
    assert_eq!(second.revision, 2);
    assert_eq!(store.deployment_instances(&id).unwrap().len(), 1);
    let spec = store.revision_instances(&id, 2).unwrap().unwrap();
    assert_eq!(spec.instances, 1);
    assert_eq!(spec.placement.pinned_host(), host["name"].as_str());
    // The first revision's declaration is history and stays readable.
    assert_eq!(
        store.revision_instances(&id, 1).unwrap().unwrap().instances,
        3
    );
}

/// ADR 0013 §3: a host outside the allowed set is never a candidate, and a
/// count the resolving hosts cannot hold is unplaceable.
// T03 T14
#[test]
fn acceptance_refuses_disallowed_host_and_unplaceable_count() {
    let (store, session, config, host) = setup();
    for (key, patch) in [
        ("elsewhere", json!({"placement": {"hosts": ["elsewhere"]}})),
        (
            "too-many",
            json!({"instances": 2, "placement": {"max_per_host": 1}}),
        ),
    ] {
        let mut declared = config.clone();
        for (field, value) in patch.as_object().unwrap() {
            declared[field] = value.clone();
        }
        assert!(
            matches!(
                store.create_stopped_managed_configuration(
                    &session,
                    "p",
                    key,
                    &json!({"config":declared}).to_string(),
                    &host,
                    1
                ),
                Err(crate::managed_configuration::ManagedConfigurationError::Invalid)
            ),
            "{key}"
        );
    }
}

/// Owner decision Q7: `stop instance` records the operator's stop on that
/// instance; on-demand activation of the realized instance is then refused
/// until `start instance` or `start deployment` lifts it (Q5).
// T18
#[test]
fn operator_instance_stop_is_recorded_and_lifted() {
    let (store, session, mut config, host) = setup();
    config["instances"] = json!(2);
    let id = store
        .create_stopped_managed_configuration(
            &session,
            "p",
            "k",
            &json!({"config":config}).to_string(),
            &host,
            1,
        )
        .unwrap()
        .deployment_id;
    assert!(!store.on_demand_instance_stopped(&id).unwrap());
    assert!(!store.set_instance_operator_stopped(&id, 1, true).unwrap());
    assert!(!store.on_demand_instance_stopped(&id).unwrap());
    assert!(!store.set_instance_operator_stopped(&id, 0, true).unwrap());
    assert!(store.on_demand_instance_stopped(&id).unwrap());
    assert!(matches!(
        store.set_instance_operator_stopped(&id, 2, true),
        Err(InstanceError::NotFound)
    ));
    store.clear_instance_operator_stops(&id).unwrap();
    assert!(store
        .deployment_instances(&id)
        .unwrap()
        .iter()
        .all(|r| !r.operator_stopped));
    assert!(!store.instance_holds_runtime(&id, 1).unwrap());
}

/// ADR 0013 (I2, schema v23): an upgrade moves a running deployment's runtime
/// state onto its instance 0 unchanged — the same revision, generation and
/// desired, observed, admission and dispatch state — and the deployment row
/// keeps reading the same values as their aggregate.
// T08 T18 T32
#[test]
fn v23_moves_runtime_state_onto_instance_zero_unchanged() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    for (index, sql) in MIGRATIONS.iter().take(22).enumerate() {
        conn.execute_batch(sql).unwrap();
        if index + 1 == 22 {
            let tx = conn.unchecked_transaction().unwrap();
            migrate(&tx).unwrap();
            tx.commit().unwrap();
        }
        conn.execute(
            "INSERT INTO schema_migrations(version) VALUES(?1)",
            [(index + 1) as i64],
        )
        .unwrap();
    }
    conn.execute_batch(
        r#"INSERT INTO deployments(id,name,kind,desired_state,observed_state,admission_enabled,dispatch_enabled,suspended,current_generation,schema_version,revision) VALUES('a','a','model','ready','ready',1,1,0,4,1,2);
        INSERT INTO effective_revisions VALUES('a',2,'{"host":{"name":"host-a"}}','f');
        INSERT INTO deployment_revision_instances(deployment_id,revision,instances,placement_json) VALUES('a',2,1,'{"hosts":null,"selector":{},"strategy":"spread","max_per_host":null}');"#,
    )
    .unwrap();
    apply(&conn).unwrap();
    apply(&conn).unwrap();
    let row: (i64, i64, String, String, i64, i64) = conn
        .query_row(
            "SELECT revision,generation,desired_state,observed_state,admission_enabled,dispatch_enabled FROM deployment_instances WHERE deployment_id='a' AND instance_index=0",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .unwrap();
    assert_eq!(row, (2, 4, "ready".into(), "ready".into(), 1, 1));
    // The aggregate follows the instance: closing its dispatch closes the row's.
    conn.execute(
        "UPDATE deployment_instances SET dispatch_enabled=0 WHERE deployment_id='a'",
        [],
    )
    .unwrap();
    let dispatch: i64 = conn
        .query_row(
            "SELECT dispatch_enabled FROM deployments WHERE id='a'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(dispatch, 0);
}
