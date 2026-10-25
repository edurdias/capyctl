//! ADR 0035 §3 (schema v58): the startup peak and the parked residue a
//! launch measured are kept per tensor-parallel size N and read only at that
//! size. A record from before v58 is the N = 1 record. On N GPUs the per-GPU
//! figure (the largest any GPU showed) applies to every GPU of a later start
//! at N, whichever GPUs it was measured on. CPU only; nothing here qualifies
//! an engine.
use super::*;
use capyctl_config::effective::{resolve_effective, EffectiveDeployment, Residency};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::BTreeMap;

const GIB: i64 = 1 << 30;

/// The vLLM golden deployment, its phases derived from a 12 GiB request, on
/// a host with two identical 41 GiB cards managing 40 GiB beside host RAM;
/// `patch` adds or replaces fields.
fn revision(patch: Value) -> EffectiveDeployment {
    let golden: Value = serde_json::from_str(include_str!(
        "../../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    let (mut deployment, mut host) = (
        golden["input"]["deployment"].clone(),
        golden["input"]["host"].clone(),
    );
    host["resource_policy"]["domains"] = json!({
        "system": {"memory": "distinct", "managed_limit": "200GiB", "free_reserve": "8GiB",
                   "parked_limit": "64GiB"},
        "gpu0": {"memory": "device", "device": "gpu0", "managed_limit": "40GiB",
                 "free_reserve": "1GiB", "parked_limit": "4GiB"},
        "gpu1": {"memory": "device", "device": "gpu1", "managed_limit": "40GiB",
                 "free_reserve": "1GiB", "parked_limit": "4GiB"}
    });
    host["resource_policy"]["devices"] = json!({
        "gpu0": {"domain": "gpu0", "sharing": "shared", "product_name": "NVIDIA RTX A6000"},
        "gpu1": {"domain": "gpu1", "sharing": "shared", "product_name": "NVIDIA RTX A6000"}
    });
    let object = deployment.as_object_mut().unwrap();
    object.remove("resources");
    deployment["devices"] = json!([{"id": "gpu0", "sharing": "shared"}]);
    deployment["engine_config"] = json!({"memory": {"request": "12GiB", "kv_cache": "4GiB"}});
    for (field, value) in patch.as_object().unwrap() {
        deployment[field] = value.clone();
    }
    resolve_effective(&deployment, &host).unwrap()
}

/// The same deployment on one GPU, parking deep, and on both GPUs. Parking
/// at N > 1 is gated off in the shipped support data (ADR 0035 §9), so the
/// two-GPU revision is resolved restart-only and then given the deep tier
/// and a parked phase on its two GPUs, which is what the residue applies to
/// once the gate opens.
fn revisions() -> (EffectiveDeployment, EffectiveDeployment) {
    let one = revision(json!({"residency": "deep"}));
    let mut two = revision(json!({"residency": "restart_only",
                                  "topology": {"tensor_parallel": 2},
                                  "devices": [{"id": "gpu0", "sharing": "shared"},
                                              {"id": "gpu1", "sharing": "shared"}]}));
    two.residency = Residency::Deep;
    two.resources.parked = two.resources.ready.clone();
    assert_eq!(
        (one.local_tensor_parallel(), two.local_tensor_parallel()),
        (1, 2)
    );
    assert!(parked_charge::measurable(&one) && parked_charge::measurable(&two));
    (one, two)
}

fn bytes(pairs: &[(&str, i64)]) -> BTreeMap<String, i64> {
    pairs
        .iter()
        .map(|(domain, bytes)| ((*domain).to_owned(), *bytes))
        .collect()
}

// T46 T33 (ADR 0035 §3, schema v58): a startup peak recorded before v58 is
// read by a start at N = 1 and never by a start at N = 2; a peak recorded at
// N = 2 (measured on gpu1 alone) sizes both GPUs of every start at N = 2 and
// never a start at N = 1. `auto`'s choice reads the device peak of each size
// on its own.
#[test]
fn startup_peaks_are_read_only_at_their_own_size() {
    let (one, two) = revisions();
    let installation = startup::installation(&one);
    assert_eq!(installation, startup::installation(&two));
    let conn = Connection::open_in_memory().unwrap();
    crate::migrations::apply_through(&conn, 57).unwrap();
    conn.execute(
        "INSERT INTO startup_measurements(deployment_id,revision,host_id,installation,domain,peak_bytes,step_id,measured_at_ms)
         VALUES('d',1,'lab',?1,'gpu0',?2,'s1',1)",
        rusqlite::params![installation, 20 * GIB],
    )
    .unwrap();
    crate::migrations::apply(&conn).unwrap();

    let peaks = |e: &EffectiveDeployment| startup::frozen_startup(&conn, "d", 1, e).unwrap().1;
    assert_eq!(
        peaks(&one),
        bytes(&[("gpu0", 20 * GIB)]),
        "the pre-v58 peak is N = 1's"
    );
    assert!(peaks(&two).is_empty(), "N = 1's peak never sizes N = 2");

    conn.execute(
        "INSERT INTO startup_measurements(deployment_id,revision,host_id,installation,tensor_parallel,domain,peak_bytes,step_id,measured_at_ms)
         VALUES('d',1,'lab',?1,2,'gpu1',?2,'s2',2)",
        rusqlite::params![installation, 18 * GIB],
    )
    .unwrap();
    assert_eq!(
        peaks(&two),
        bytes(&[("gpu0", 18 * GIB), ("gpu1", 18 * GIB)]),
        "the per-GPU peak at N = 2 applies to every GPU of the set"
    );
    assert_eq!(
        peaks(&one),
        bytes(&[("gpu0", 20 * GIB)]),
        "N = 2's never sizes N = 1"
    );
    let device_peak = |n| startup::measured_device_peak(&conn, "d", 1, &one, n).unwrap();
    assert_eq!(
        (device_peak(1), device_peak(2), device_peak(3)),
        (Some(20 * GIB), Some(18 * GIB), None)
    );
}

// T46 T33 (ADR 0035 §3, schema v58): a parked residue recorded before v58 is
// read by a revision on one GPU and not on two; a residue recorded at N = 2
// on one GPU is charged on each GPU of the two-GPU revision's parked phase,
// beside its host RAM figure, and never read on one GPU.
#[test]
fn parked_residue_is_read_only_at_its_own_size() {
    let (one, two) = revisions();
    let installation = startup::installation(&one);
    let conn = Connection::open_in_memory().unwrap();
    crate::migrations::apply_through(&conn, 57).unwrap();
    conn.execute(
        "INSERT INTO parked_measurements(deployment_id,revision,host_id,installation,domain,bytes,step_id,measured_at_ms)
         VALUES('d',1,'lab',?1,'gpu0',?2,'s1',1)",
        rusqlite::params![installation, 3 * GIB],
    )
    .unwrap();
    crate::migrations::apply(&conn).unwrap();

    let residue = |e: &EffectiveDeployment| parked_charge::measured(&conn, "d", 1, e).unwrap();
    assert_eq!(
        residue(&one),
        bytes(&[("gpu0", 3 * GIB)]),
        "the pre-v58 residue is N = 1's"
    );
    assert!(
        residue(&two).is_empty(),
        "N = 1's residue never charges N = 2"
    );

    for (domain, gib) in [("gpu1", 2), ("system", 5)] {
        conn.execute(
            "INSERT INTO parked_measurements(deployment_id,revision,host_id,installation,tensor_parallel,domain,bytes,step_id,measured_at_ms)
             VALUES('d',1,'lab',?1,2,?2,?3,'s2',2)",
            rusqlite::params![installation, domain, gib * GIB],
        )
        .unwrap();
    }
    assert_eq!(
        residue(&two),
        bytes(&[("gpu0", 2 * GIB), ("gpu1", 2 * GIB), ("system", 5 * GIB)]),
        "the per-GPU residue at N = 2 applies to every GPU of the set"
    );
    assert_eq!(
        residue(&one),
        bytes(&[("gpu0", 3 * GIB)]),
        "N = 2's never charges N = 1"
    );
}
