//! ADR 0013 §5–7: deployment instances in the durable store.
//!
//! An instance is one engine group realizing a deployment. This module owns the
//! instance rows (index, placement, generation, operator stop), the per-revision
//! count and placement constraints, and the per-host resolutions of a revision.
//! It chooses no host and reserves nothing: placement is the scheduler's (I2).
//!
//! Until the coordinator addresses instances (I2), the single-instance lifecycle
//! realizes instance 0 only. Every binding, run, claim, request lease, attempt and
//! resource owner it writes carries the column default `instance_index = 0`, and
//! instance 0's resource owner is the deployment id every existing reservation
//! already has, so nothing that runs today changes identity.

use capyctl_config::instances::{InstanceSpec, Placement};
use rusqlite::{params, OptionalExtension, Transaction};
use serde::Serialize;

/// ADR 0013 §5: the resource owner of one instance. Instance 0 keeps the
/// deployment id, which is the owner every reservation made before instances
/// existed carries (the migration keeps it); any other instance is
/// `deployment:<id>/instance:<k>`, so each reservation is charged and released
/// only on its own instance's evidence (SPEC §7.3, ADR 0011).
pub fn instance_owner_id(deployment_id: &str, instance_index: u32) -> String {
    if instance_index == 0 {
        deployment_id.to_owned()
    } else {
        format!("deployment:{deployment_id}/instance:{instance_index}")
    }
}

/// The inverse of [`instance_owner_id`] for an owner id of the per-instance
/// form; an owner id without it names instance 0 of the deployment it equals.
pub fn parse_instance_owner_id(owner_id: &str) -> (String, u32) {
    owner_id
        .strip_prefix("deployment:")
        .and_then(|rest| rest.rsplit_once("/instance:"))
        .and_then(|(deployment, index)| {
            index
                .parse::<u32>()
                .ok()
                .filter(|k| *k > 0 && index == k.to_string())
                .map(|k| (deployment.to_owned(), k))
        })
        .unwrap_or_else(|| (owner_id.to_owned(), 0))
}

/// One instance row. Historical record, not proof a process runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstanceRow {
    pub index: u32,
    /// The placed host; `None` until an activation places it.
    pub host_id: Option<String>,
    pub generation: Option<i64>,
    /// `active`, or `retiring` while a count decrease drains it.
    pub state: String,
    /// Owner decision Q7: stopped by `stop instance`; on-demand activation
    /// leaves it stopped until `start instance` or `start deployment`.
    pub operator_stopped: bool,
}

/// Why an instance command was refused.
#[derive(Debug, thiserror::Error)]
pub enum InstanceError {
    #[error("deployment or instance not found")]
    NotFound,
    #[error("instance declaration refused: {0}")]
    Invalid(String),
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
}

impl From<InstanceError> for crate::StoreError {
    fn from(error: InstanceError) -> Self {
        match error {
            InstanceError::Sql(error) => Self::Sql(error),
            InstanceError::NotFound | InstanceError::Invalid(_) => Self::Conflict,
        }
    }
}

fn has_column(tx: &Transaction<'_>, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut statement = tx.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = statement
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(names.iter().any(|name| name == column))
}

/// Schema v37 data step (ADR 0019, discrete GPU design §7): the GPU an
/// instance was placed on, NULL on a host without a GPU choice. Idempotent.
pub(crate) fn migrate_v37(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    if !has_column(tx, "deployment_instances", "device")? {
        tx.execute_batch(
            "ALTER TABLE deployment_instances ADD COLUMN device TEXT CHECK(device IS NULL OR length(device) BETWEEN 1 AND 128);",
        )?;
    }
    Ok(())
}

const INSTANCE_COLUMN: &str = "instance_index INTEGER NOT NULL DEFAULT 0 CHECK(instance_index>=0)";

const HOST_NAME: &str = "CASE WHEN json_valid(e.effective_json) AND json_type(e.effective_json,'$.host.name')='text' THEN json_extract(e.effective_json,'$.host.name') END";

/// Schema v22 data step (ADR 0013 §5). Idempotent: every step checks what is
/// already there, so a store rolled back to an earlier version reapplies it.
pub(crate) fn migrate(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    for table in [
        "runtime_bindings",
        "lifecycle_runs",
        "request_leases",
        "deployment_attempts",
    ] {
        if !has_column(tx, table, "instance_index")? {
            tx.execute_batch(&format!(
                "ALTER TABLE {table} ADD COLUMN {INSTANCE_COLUMN};"
            ))?;
        }
    }
    tx.execute_batch(
        "DROP INDEX IF EXISTS one_retained_binding;
         CREATE UNIQUE INDEX one_retained_binding ON runtime_bindings(deployment_id,instance_index)
           WHERE state!='released';
         DROP INDEX IF EXISTS one_activation;
         CREATE UNIQUE INDEX one_activation ON lifecycle_runs(deployment_id,instance_index,revision,generation)
           WHERE action='activate' AND state IN ('queued','running','uncertain');",
    )?;
    if !has_column(tx, "lifecycle_claims", "instance_index")? {
        tx.execute_batch(&format!(
            "CREATE TABLE lifecycle_claims_v22(
               deployment_id TEXT NOT NULL REFERENCES deployments(id),
               operation_id TEXT NOT NULL REFERENCES lifecycle_runs(operation_id),
               revision INTEGER NOT NULL CHECK(revision>0),
               generation INTEGER NOT NULL CHECK(generation>0),
               {INSTANCE_COLUMN},
               PRIMARY KEY(deployment_id,instance_index));
             INSERT INTO lifecycle_claims_v22(deployment_id,operation_id,revision,generation,instance_index)
               SELECT deployment_id,operation_id,revision,generation,0 FROM lifecycle_claims;
             DROP TABLE lifecycle_claims;
             ALTER TABLE lifecycle_claims_v22 RENAME TO lifecycle_claims;"
        ))?;
    }
    if !has_column(tx, "resource_owners", "instance_index")? {
        tx.execute_batch(&format!(
            "CREATE TABLE resource_owners_v22(
               owner_id TEXT PRIMARY KEY,
               footprint_json TEXT NOT NULL,
               deployment_id TEXT NOT NULL REFERENCES deployments(id),
               {INSTANCE_COLUMN},
               UNIQUE(deployment_id,instance_index),
               CHECK((instance_index=0 AND owner_id=deployment_id)
                  OR (instance_index>0 AND owner_id='deployment:'||deployment_id||'/instance:'||instance_index)));
             INSERT INTO resource_owners_v22(owner_id,footprint_json,deployment_id,instance_index)
               SELECT owner_id,footprint_json,owner_id,0 FROM resource_owners;
             DROP TABLE resource_owners;
             ALTER TABLE resource_owners_v22 RENAME TO resource_owners;"
        ))?;
    }
    // Existing revisions: one instance, pinned to the host they resolved on.
    tx.execute_batch(&format!(
        "INSERT OR IGNORE INTO deployment_revision_instances(deployment_id,revision,instances,placement_json)
           SELECT e.deployment_id,e.revision,1,
                  json_object('hosts',CASE WHEN {HOST_NAME} IS NULL THEN NULL ELSE json_array({HOST_NAME}) END,
                              'selector',json_object(),'strategy','spread','max_per_host',NULL)
             FROM effective_revisions e;
         INSERT OR IGNORE INTO host_effective_revisions(deployment_id,revision,host_id,outcome,effective_json,fingerprint)
           SELECT e.deployment_id,e.revision,{HOST_NAME},'resolved',e.effective_json,e.fingerprint
             FROM effective_revisions e WHERE {HOST_NAME} IS NOT NULL;
         INSERT OR IGNORE INTO deployment_instances(deployment_id,instance_index,host_id,generation,placed_at)
           SELECT d.id,0,
                  (SELECT {HOST_NAME} FROM effective_revisions e WHERE e.deployment_id=d.id AND e.revision=d.revision),
                  d.current_generation,strftime('%Y-%m-%dT%H:%M:%fZ','now')
             FROM deployments d;
         CREATE TRIGGER IF NOT EXISTS deployment_instance_zero AFTER INSERT ON deployments
         BEGIN
           INSERT OR IGNORE INTO deployment_instances(deployment_id,instance_index) VALUES(NEW.id,0);
         END;
         CREATE TRIGGER IF NOT EXISTS instance_activation_generation AFTER INSERT ON lifecycle_runs
           WHEN NEW.action='activate'
         BEGIN
           INSERT OR IGNORE INTO deployment_instances(deployment_id,instance_index)
             VALUES(NEW.deployment_id,NEW.instance_index);
           UPDATE deployment_instances
              SET generation=NEW.generation,
                  host_id=COALESCE(host_id,(SELECT {HOST_NAME} FROM effective_revisions e
                                             WHERE e.deployment_id=NEW.deployment_id AND e.revision=NEW.revision)),
                  placed_at=COALESCE(placed_at,strftime('%Y-%m-%dT%H:%M:%fZ','now'))
            WHERE deployment_id=NEW.deployment_id AND instance_index=NEW.instance_index;
         END;"
    ))?;
    Ok(())
}

/// ADR 0013 §5: the instance a fence names. A generation is drawn from the
/// deployment's one counter, so a deployment and a generation identify exactly
/// one instance incarnation; the fence is current only while that instance
/// still carries both the generation and the revision.
pub(crate) fn fence_instance(
    tx: &Transaction<'_>,
    fence: &crate::lifecycle::DeploymentFence,
) -> Result<u32, crate::lifecycle::LifecycleError> {
    tx.query_row(
        "SELECT instance_index FROM deployment_instances WHERE deployment_id=?1 AND revision=?2 AND generation=?3",
        params![fence.deployment_id, fence.revision, fence.generation],
        |r| r.get(0),
    )
    .optional()?
    .ok_or(crate::lifecycle::LifecycleError::Stale)
}

/// The instance an incarnation's generation belongs to, whatever its revision
/// is now: historical records keep naming their instance after it moves on.
pub(crate) fn generation_instance(
    tx: &Transaction<'_>,
    deployment_id: &str,
    generation: i64,
) -> rusqlite::Result<Option<u32>> {
    tx.query_row(
        "SELECT instance_index FROM deployment_instances WHERE deployment_id=?1 AND generation=?2",
        params![deployment_id, generation],
        |r| r.get(0),
    )
    .optional()
}

/// ADR 0013 §5: draw the deployment's next generation. One counter serves every
/// instance, so no two incarnations of a deployment share a generation; with a
/// single instance this is exactly the old `generation + 1`. The drawn value is
/// above every generation any instance holds, so a caller moving an instance to
/// it may match the instance by `generation IN (old, drawn)`: a deployment
/// without a managed revision mirrors its counter onto instance 0 at once.
pub(crate) fn draw_generation(
    tx: &Transaction<'_>,
    deployment_id: &str,
) -> Result<i64, crate::lifecycle::LifecycleError> {
    let current: i64 = tx
        .query_row(
            "SELECT MAX(current_generation,COALESCE((SELECT MAX(generation) FROM deployment_instances WHERE deployment_id=?1),0)) FROM deployments WHERE id=?1",
            [deployment_id],
            |r| r.get(0),
        )
        .optional()?
        .ok_or(crate::lifecycle::LifecycleError::NotFound)?;
    let next = current
        .checked_add(1)
        .ok_or(crate::lifecycle::LifecycleError::Invalid)?;
    tx.execute(
        "UPDATE deployments SET current_generation=?2 WHERE id=?1",
        params![deployment_id, next],
    )?;
    Ok(next)
}

/// The aggregate a managed deployment row carries of its instances (ADR 0013
/// §6): desired `ready` while any instance is wanted, observed `ready` while
/// any instance is Ready (`parked` while none is and one is parked), admission
/// and dispatch open while any instance's are. Readers that predate instances
/// read this row; every lifecycle fence reads the instance itself.
const AGGREGATE: &str = "UPDATE deployments SET
       desired_state=CASE WHEN EXISTS(SELECT 1 FROM deployment_instances a WHERE a.deployment_id=deployments.id AND a.desired_state='ready') THEN 'ready' ELSE 'stopped' END,
       observed_state=CASE WHEN EXISTS(SELECT 1 FROM deployment_instances a WHERE a.deployment_id=deployments.id AND a.observed_state='ready') THEN 'ready'
                           WHEN EXISTS(SELECT 1 FROM deployment_instances a WHERE a.deployment_id=deployments.id AND a.observed_state='parked') THEN 'parked'
                           ELSE 'stopped' END,
       admission_enabled=EXISTS(SELECT 1 FROM deployment_instances a WHERE a.deployment_id=deployments.id AND a.admission_enabled=1),
       dispatch_enabled=EXISTS(SELECT 1 FROM deployment_instances a WHERE a.deployment_id=deployments.id AND a.dispatch_enabled=1)";

/// Schema v23 data step (ADR 0013 §4, §6, §7; unit I2). Idempotent: every step
/// checks what is already there.
pub(crate) fn migrate_v23(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    for (column, definition) in [
        ("revision", "INTEGER CHECK(revision IS NULL OR revision>0)"),
        ("desired_state", "TEXT NOT NULL DEFAULT 'stopped'"),
        ("observed_state", "TEXT NOT NULL DEFAULT 'stopped'"),
        // Unchecked like the deployment columns they mirror for a deployment
        // without a managed revision; status refuses a value other than 0 or 1.
        ("admission_enabled", "INTEGER NOT NULL DEFAULT 0"),
        ("dispatch_enabled", "INTEGER NOT NULL DEFAULT 0"),
        ("pending_start_until_ms", "INTEGER"),
        ("last_error", "TEXT"),
    ] {
        if !has_column(tx, "deployment_instances", column)? {
            tx.execute_batch(&format!(
                "ALTER TABLE deployment_instances ADD COLUMN {column} {definition};"
            ))?;
        }
    }
    if !has_column(tx, "host_effective_revisions", "source_json")? {
        tx.execute_batch("ALTER TABLE host_effective_revisions ADD COLUMN source_json TEXT;")?;
    }
    // ADR 0013 §7: the revision whose runtime this one shares. A count-only
    // revision shares its predecessor's, so its running instances keep going;
    // NULL is the revision itself.
    if !has_column(tx, "deployment_revision_instances", "runtime_revision")? {
        tx.execute_batch(
            "ALTER TABLE deployment_revision_instances ADD COLUMN runtime_revision INTEGER;",
        )?;
    }
    // Instance 0 of every deployment takes the runtime state its deployment
    // row held; nothing that runs changes identity or fence.
    tx.execute_batch(
        "UPDATE deployment_instances SET
           revision=(SELECT d.revision FROM deployments d WHERE d.id=deployment_instances.deployment_id),
           generation=(SELECT d.current_generation FROM deployments d WHERE d.id=deployment_instances.deployment_id),
           desired_state=(SELECT d.desired_state FROM deployments d WHERE d.id=deployment_instances.deployment_id),
           observed_state=(SELECT d.observed_state FROM deployments d WHERE d.id=deployment_instances.deployment_id),
           admission_enabled=(SELECT d.admission_enabled FROM deployments d WHERE d.id=deployment_instances.deployment_id),
           dispatch_enabled=(SELECT d.dispatch_enabled FROM deployments d WHERE d.id=deployment_instances.deployment_id)
         WHERE instance_index=0 AND revision IS NULL;
         UPDATE host_effective_revisions SET source_json=(SELECT s.config_json FROM managed_configuration_sources s
             WHERE s.deployment_id=host_effective_revisions.deployment_id AND s.revision=host_effective_revisions.revision)
           WHERE source_json IS NULL AND outcome='resolved';
         DROP VIEW IF EXISTS instance_runtime;
         CREATE VIEW instance_runtime AS
           SELECT d.id AS id, i.instance_index AS instance_index,
                  CASE WHEN d.revision=i.revision OR EXISTS(SELECT 1 FROM effective_revisions e WHERE e.deployment_id=d.id AND e.revision=d.revision)
                       THEN i.revision END AS revision,
                  i.generation AS current_generation, d.kind AS kind, d.name AS name,
                  d.route_model_id AS route_model_id, i.desired_state AS desired_state,
                  i.observed_state AS observed_state, i.admission_enabled AS admission_enabled,
                  i.dispatch_enabled AS dispatch_enabled, d.suspended AS suspended,
                  d.revision AS deployment_revision, i.host_id AS host_id, i.state AS lifecycle
             FROM deployments d JOIN deployment_instances i ON i.deployment_id=d.id;
         DROP TRIGGER IF EXISTS deployment_instance_zero;
         CREATE TRIGGER deployment_instance_zero AFTER INSERT ON deployments
         BEGIN
           INSERT OR IGNORE INTO deployment_instances(deployment_id,instance_index,revision,generation,desired_state,observed_state,admission_enabled,dispatch_enabled)
             VALUES(NEW.id,0,NEW.revision,NEW.current_generation,NEW.desired_state,NEW.observed_state,NEW.admission_enabled,NEW.dispatch_enabled);
         END;",
    )?;
    // A deployment without an accepted managed revision (the F1 path and its
    // fixtures) has exactly one instance and no aggregate: its row and its
    // instance 0 are kept equal in both directions, so the instance-level
    // fences read what its row says. A trigger never re-fires itself, so the
    // pair cannot loop.
    tx.execute_batch(
        "DROP TRIGGER IF EXISTS legacy_deployment_mirror;
         CREATE TRIGGER legacy_deployment_mirror
           AFTER UPDATE OF revision,current_generation,desired_state,observed_state,admission_enabled,dispatch_enabled ON deployments
           WHEN NOT EXISTS(SELECT 1 FROM deployment_revision_instances r WHERE r.deployment_id=NEW.id)
         BEGIN
           UPDATE deployment_instances SET revision=NEW.revision,generation=NEW.current_generation,
                  desired_state=NEW.desired_state,observed_state=NEW.observed_state,
                  admission_enabled=NEW.admission_enabled,dispatch_enabled=NEW.dispatch_enabled
            WHERE deployment_id=NEW.id AND instance_index=0;
         END;
         DROP TRIGGER IF EXISTS legacy_instance_mirror;
         CREATE TRIGGER legacy_instance_mirror
           AFTER UPDATE OF revision,generation,desired_state,observed_state,admission_enabled,dispatch_enabled ON deployment_instances
           WHEN NEW.instance_index=0 AND NOT EXISTS(SELECT 1 FROM deployment_revision_instances r WHERE r.deployment_id=NEW.deployment_id)
         BEGIN
           UPDATE deployments SET revision=COALESCE(NEW.revision,revision),
                  current_generation=MAX(current_generation,COALESCE(NEW.generation,0)),
                  desired_state=NEW.desired_state,observed_state=NEW.observed_state,
                  admission_enabled=NEW.admission_enabled,dispatch_enabled=NEW.dispatch_enabled
            WHERE id=NEW.deployment_id;
         END;",
    )?;
    for (name, event, row) in [
        ("instance_aggregate_insert", "INSERT", "NEW"),
        (
            "instance_aggregate_update",
            "UPDATE OF desired_state,observed_state,admission_enabled,dispatch_enabled",
            "NEW",
        ),
        ("instance_aggregate_delete", "DELETE", "OLD"),
    ] {
        tx.execute_batch(&format!(
            "DROP TRIGGER IF EXISTS {name};
             CREATE TRIGGER {name} AFTER {event} ON deployment_instances
             WHEN EXISTS(SELECT 1 FROM deployment_revision_instances r WHERE r.deployment_id={row}.deployment_id)
             BEGIN {AGGREGATE} WHERE id={row}.deployment_id; END;"
        ))?;
    }
    Ok(())
}

/// One allowed host a revision resolved on (ADR 0013 §3).
#[derive(Debug, Clone)]
pub(crate) struct ResolvedHost {
    pub(crate) host_id: String,
    pub(crate) host_name: String,
    pub(crate) effective_json: String,
    pub(crate) fingerprint: String,
    /// The deployment document scoped to this host.
    pub(crate) source_json: String,
    /// ADR 0019 (discrete GPU design §7): the revision resolved once per GPU
    /// of a multi-GPU host, when the deployment pins no device; empty
    /// otherwise. The host's own row above is the first of them.
    pub(crate) devices: Vec<DeviceResolution>,
}

/// One GPU's resolution of a revision on a multi-GPU host.
#[derive(Debug, Clone)]
pub(crate) struct DeviceResolution {
    pub(crate) device: String,
    pub(crate) effective_json: String,
    pub(crate) fingerprint: String,
    pub(crate) source_json: String,
}

/// A revision accepted while the deployment runs (ADR 0013 §7, Q8).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Replacement {
    pub(crate) now_ms: i64,
    /// How long a restart after a non-count revision may wait for placement.
    pub(crate) restart_until_ms: i64,
}

/// The revision whose runtime `revision` shares (ADR 0013 §7).
pub(crate) fn runtime_revision(
    tx: &Transaction<'_>,
    deployment_id: &str,
    revision: i64,
) -> rusqlite::Result<i64> {
    Ok(tx
        .query_row(
            "SELECT COALESCE(runtime_revision,revision) FROM deployment_revision_instances WHERE deployment_id=?1 AND revision=?2",
            params![deployment_id, revision],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(revision))
}

/// ADR 0013 §7: whether `next` changes only the instance count of `previous`:
/// the same placement constraints and the same revision resolved on the same
/// hosts. Only then may running instances keep their incarnations.
fn count_only(
    tx: &Transaction<'_>,
    deployment_id: &str,
    previous: i64,
    placement_json: &str,
    hosts: &[ResolvedHost],
) -> rusqlite::Result<bool> {
    let stored: Option<String> = tx
        .query_row(
            "SELECT placement_json FROM deployment_revision_instances WHERE deployment_id=?1 AND revision=?2",
            params![deployment_id, previous],
            |r| r.get(0),
        )
        .optional()?;
    let same_placement = stored
        .and_then(|stored| serde_json::from_str::<Placement>(&stored).ok())
        .zip(serde_json::from_str::<Placement>(placement_json).ok())
        .is_some_and(|(a, b)| a == b);
    if !same_placement {
        return Ok(false);
    }
    let before: Vec<(String, String)> = tx
        .prepare(
            "SELECT host_id,effective_json FROM host_effective_revisions WHERE deployment_id=?1 AND revision=?2 AND outcome='resolved' ORDER BY host_id",
        )?
        .query_map(params![deployment_id, previous], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut after: Vec<(String, String)> = hosts
        .iter()
        .map(|h| (h.host_id.clone(), h.effective_json.clone()))
        .collect();
    after.sort();
    Ok(before == after)
}

/// ADR 0013 §3, §7: record an accepted revision's instance declaration, every
/// host it resolved or was refused on, and bring the instance rows to
/// `0..N-1`.
///
/// Instances that hold no runtime move to the new revision. A running
/// deployment (`replacement`) keeps every running instance's incarnation: a
/// count-only revision shares its predecessor's runtime, so they go on
/// serving; any other revision marks each for a stop and a restart on the new
/// revision (owner decision Q8), which the scheduler's reconciliation carries
/// out durably. A decrease retires surplus instances in the ADR order —
/// stopped first, then Ready ones on the host with most instances of the
/// deployment, then the highest index — removing a stopped one at once and
/// draining a running one first; indices are compacted only over instances
/// that hold no runtime.
pub(crate) fn record_accepted_revision(
    tx: &Transaction<'_>,
    deployment_id: &str,
    revision: i64,
    spec: &InstanceSpec,
    resolved: &[ResolvedHost],
    refused: &[crate::managed_configuration::HostRefusal],
    replacement: Option<Replacement>,
) -> Result<(), InstanceError> {
    if resolved.is_empty() {
        return Err(InstanceError::Invalid("no allowed host resolves".into()));
    }
    for host in resolved {
        if !spec.placement.allows(&host.host_id) && !spec.placement.allows(&host.host_name) {
            return Err(InstanceError::Invalid(format!(
                "host {} is not in the allowed host set",
                host.host_id
            )));
        }
    }
    if !spec.placeable_on(resolved.len()) {
        return Err(InstanceError::Invalid(
            "unplaceable: max_per_host times the resolving hosts is below instances".into(),
        ));
    }
    let placement = serde_json::to_string(&spec.placement)
        .map_err(|_| InstanceError::Invalid("placement does not encode".into()))?;
    let shares = match replacement {
        Some(_) if count_only(tx, deployment_id, revision - 1, &placement, resolved)? => {
            Some(runtime_revision(tx, deployment_id, revision - 1)?)
        }
        _ => None,
    };
    tx.execute(
        "INSERT INTO deployment_revision_instances(deployment_id,revision,instances,placement_json,runtime_revision,warm) VALUES(?1,?2,?3,?4,?5,?6)",
        params![deployment_id, revision, spec.instances, placement, shares, spec.warm],
    )?;
    for host in resolved {
        tx.execute(
            "INSERT INTO host_effective_revisions(deployment_id,revision,host_id,outcome,effective_json,fingerprint,source_json) VALUES(?1,?2,?3,'resolved',?4,?5,?6)",
            params![deployment_id, revision, host.host_id, host.effective_json, host.fingerprint, host.source_json],
        )?;
        for choice in &host.devices {
            tx.execute(
                "INSERT INTO host_device_effective_revisions(deployment_id,revision,host_id,device,effective_json,fingerprint,source_json) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![deployment_id, revision, host.host_id, choice.device, choice.effective_json, choice.fingerprint, choice.source_json],
            )?;
        }
    }
    for refusal in refused {
        if resolved.iter().any(|h| h.host_id == refusal.host_id) {
            continue;
        }
        tx.execute(
            "INSERT OR IGNORE INTO host_effective_revisions(deployment_id,revision,host_id,outcome,diagnostic) VALUES(?1,?2,?3,'refused',?4)",
            params![deployment_id, revision, refusal.host_id, refusal.diagnostic],
        )?;
    }
    let generation: i64 = tx.query_row(
        "SELECT current_generation FROM deployments WHERE id=?1",
        [deployment_id],
        |r| r.get(0),
    )?;
    // Instances holding no runtime take the new revision; instance 0 also the
    // new generation, so a stopped deployment is fenced by its receipt exactly
    // as before instances existed.
    tx.execute(
        "UPDATE deployment_instances SET revision=?2,
                generation=CASE WHEN instance_index=0 THEN ?3 ELSE generation END
          WHERE deployment_id=?1 AND NOT EXISTS(SELECT 1 FROM runtime_bindings b
                WHERE b.deployment_id=deployment_instances.deployment_id
                  AND b.instance_index=deployment_instances.instance_index AND b.state!='released')",
        params![deployment_id, revision, generation],
    )?;
    if let Some(replacement) = replacement {
        if shares.is_none() {
            // Owner decision Q8: every running instance stops and restarts on
            // the new revision; the reconciliation stops it and places it again.
            tx.execute(
                "UPDATE deployment_instances SET pending_start_until_ms=?2,last_error=NULL
                  WHERE deployment_id=?1 AND state='active' AND EXISTS(SELECT 1 FROM runtime_bindings b
                        WHERE b.deployment_id=deployment_instances.deployment_id
                          AND b.instance_index=deployment_instances.instance_index AND b.state!='released')",
                params![deployment_id, replacement.restart_until_ms],
            )?;
        }
        let _ = replacement.now_ms;
    }
    resize(tx, deployment_id, spec.instances)?;
    Ok(())
}

/// ADR 0013 §7: bring the active instance rows to `0..N-1`.
fn resize(tx: &Transaction<'_>, deployment_id: &str, instances: u32) -> Result<(), InstanceError> {
    // A retiring instance inside the new count is wanted again.
    tx.execute(
        "UPDATE deployment_instances SET state='active' WHERE deployment_id=?1 AND instance_index<?2 AND state='retiring'",
        params![deployment_id, instances],
    )?;
    // (index, holds runtime, instances of this deployment on its host)
    let active: Vec<(u32, bool, i64)> = tx
        .prepare(
            "SELECT i.instance_index,
                    EXISTS(SELECT 1 FROM runtime_bindings b WHERE b.deployment_id=i.deployment_id AND b.instance_index=i.instance_index AND b.state!='released'),
                    (SELECT COUNT(*) FROM deployment_instances o WHERE o.deployment_id=i.deployment_id AND o.host_id=i.host_id
                        AND EXISTS(SELECT 1 FROM runtime_bindings b WHERE b.deployment_id=o.deployment_id AND b.instance_index=o.instance_index AND b.state!='released'))
               FROM deployment_instances i WHERE i.deployment_id=?1 AND i.state='active' ORDER BY i.instance_index",
        )?
        .query_map([deployment_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let surplus = active.len().saturating_sub(instances as usize);
    let mut victims = active.clone();
    // Stopped first, then running on the host with most instances, then the
    // highest index.
    victims.sort_by(|a, b| a.1.cmp(&b.1).then(b.2.cmp(&a.2)).then(b.0.cmp(&a.0)));
    for (index, running, _) in victims.into_iter().take(surplus) {
        if running {
            tx.execute(
                "UPDATE deployment_instances SET state='retiring',pending_start_until_ms=NULL WHERE deployment_id=?1 AND instance_index=?2",
                params![deployment_id, index],
            )?;
        } else {
            tx.execute(
                "DELETE FROM deployment_instances WHERE deployment_id=?1 AND instance_index=?2",
                params![deployment_id, index],
            )?;
        }
    }
    compact(tx, deployment_id, instances)?;
    // Fill the lowest free indices until the active count is the declared one;
    // a running survivor still above the count is one of them until it stops
    // and compacts.
    let active: u32 = tx.query_row(
        "SELECT COUNT(*) FROM deployment_instances WHERE deployment_id=?1 AND state='active'",
        [deployment_id],
        |r| r.get(0),
    )?;
    let mut missing = instances.saturating_sub(active);
    for index in 0..instances {
        if missing == 0 {
            break;
        }
        missing -= tx.execute(
            "INSERT OR IGNORE INTO deployment_instances(deployment_id,instance_index) VALUES(?1,?2)",
            params![deployment_id, index],
        )? as u32;
    }
    Ok(())
}

/// Move active instances above the count that hold no runtime into the free
/// indices below it. A running one keeps its index until it stops: renaming
/// it would re-key the accounting its incarnation is charged under.
pub(crate) fn compact(
    tx: &Transaction<'_>,
    deployment_id: &str,
    instances: u32,
) -> Result<(), InstanceError> {
    let movable: Vec<u32> = tx
        .prepare(
            "SELECT instance_index FROM deployment_instances i WHERE deployment_id=?1 AND instance_index>=?2 AND state='active'
               AND NOT EXISTS(SELECT 1 FROM runtime_bindings b WHERE b.deployment_id=i.deployment_id AND b.instance_index=i.instance_index AND b.state!='released')
               AND NOT EXISTS(SELECT 1 FROM lifecycle_runs r WHERE r.deployment_id=i.deployment_id AND r.instance_index=i.instance_index AND r.state NOT IN ('succeeded','failed'))
             ORDER BY instance_index",
        )?
        .query_map(params![deployment_id, instances], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for index in movable {
        let free: Option<u32> = tx
            .query_row(
                "WITH RECURSIVE slot(n) AS (SELECT 0 UNION ALL SELECT n+1 FROM slot WHERE n+1<?2)
                 SELECT n FROM slot WHERE NOT EXISTS(SELECT 1 FROM deployment_instances WHERE deployment_id=?1 AND instance_index=slot.n) ORDER BY n LIMIT 1",
                params![deployment_id, instances],
                |r| r.get(0),
            )
            .optional()?;
        match free {
            Some(slot) => {
                tx.execute(
                    "UPDATE deployment_instances SET instance_index=?3 WHERE deployment_id=?1 AND instance_index=?2",
                    params![deployment_id, index, slot],
                )?;
            }
            None => {
                tx.execute(
                    "DELETE FROM deployment_instances WHERE deployment_id=?1 AND instance_index=?2",
                    params![deployment_id, index],
                )?;
            }
        }
    }
    // W10 gap (a): a retired or moved instance leaves no closure reason behind.
    crate::switch_state::prune_closures(tx, deployment_id)?;
    Ok(())
}

impl crate::Store {
    /// Every instance row of a deployment, by index.
    pub fn deployment_instances(
        &self,
        deployment_id: &str,
    ) -> Result<Vec<InstanceRow>, InstanceError> {
        let mut statement = self.conn.prepare(
            "SELECT instance_index,host_id,generation,state,operator_stopped FROM deployment_instances
             WHERE deployment_id=?1 ORDER BY instance_index",
        )?;
        let rows = statement
            .query_map([deployment_id], |r| {
                Ok(InstanceRow {
                    index: r.get(0)?,
                    host_id: r.get(1)?,
                    generation: r.get(2)?,
                    state: r.get(3)?,
                    operator_stopped: r.get::<_, i64>(4)? != 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The declared count and placement of a revision.
    pub fn revision_instances(
        &self,
        deployment_id: &str,
        revision: i64,
    ) -> Result<Option<InstanceSpec>, InstanceError> {
        let row: Option<(u32, String, bool)> = self
            .conn
            .query_row(
                "SELECT instances,placement_json,warm FROM deployment_revision_instances WHERE deployment_id=?1 AND revision=?2",
                params![deployment_id, revision],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        row.map(|(instances, placement, warm)| {
            let placement: Placement = serde_json::from_str(&placement)
                .map_err(|_| InstanceError::Invalid("stored placement is unreadable".into()))?;
            Ok(InstanceSpec {
                instances,
                placement,
                warm,
                // ADR 0028 §2: a group is read from the revision's config_json,
                // never from these rows, so it is `None` here by design.
                group: None,
            })
        })
        .transpose()
    }

    /// Owner decision Q7: record or lift the operator's stop of one instance.
    /// Returns the previous mark so a caller whose lifecycle command is then
    /// refused can restore it. Records intent only; it stops nothing.
    pub fn set_instance_operator_stopped(
        &self,
        deployment_id: &str,
        instance_index: u32,
        stopped: bool,
    ) -> Result<bool, InstanceError> {
        let tx = self.conn.unchecked_transaction()?;
        let previous: Option<i64> = tx
            .query_row(
                "SELECT operator_stopped FROM deployment_instances WHERE deployment_id=?1 AND instance_index=?2 AND state='active'",
                params![deployment_id, instance_index],
                |r| r.get(0),
            )
            .optional()?;
        let previous = previous.ok_or(InstanceError::NotFound)? != 0;
        tx.execute(
            "UPDATE deployment_instances SET operator_stopped=?3 WHERE deployment_id=?1 AND instance_index=?2",
            params![deployment_id, instance_index, stopped as i64],
        )?;
        tx.commit()?;
        Ok(previous)
    }

    /// Owner decision Q5: an explicit `start deployment` targets all N
    /// instances, so it lifts every per-instance operator stop. On-demand
    /// activation never calls this.
    pub fn clear_instance_operator_stops(&self, deployment_id: &str) -> Result<(), InstanceError> {
        self.conn.execute(
            "UPDATE deployment_instances SET operator_stopped=0 WHERE deployment_id=?1",
            [deployment_id],
        )?;
        Ok(())
    }

    /// Owner decisions Q5, Q7: whether on-demand activation must leave this
    /// deployment alone because the operator stopped every active instance.
    /// On demand, the lowest-index instance the operator did not stop is the
    /// one brought up; a deployment with none left is not activated.
    pub fn on_demand_instance_stopped(&self, deployment_id: &str) -> Result<bool, InstanceError> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM deployment_instances WHERE deployment_id=?1 AND state='active')
                AND NOT EXISTS(SELECT 1 FROM deployment_instances WHERE deployment_id=?1 AND state='active' AND operator_stopped=0)",
            [deployment_id],
            |r| r.get(0),
        )?)
    }

    /// Whether instance `k` holds any runtime: a retained binding, an open run,
    /// a claim, a request lease or a reservation.
    pub fn instance_holds_runtime(
        &self,
        deployment_id: &str,
        instance_index: u32,
    ) -> Result<bool, InstanceError> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_bindings WHERE deployment_id=?1 AND instance_index=?2 AND state!='released')
                 OR EXISTS(SELECT 1 FROM lifecycle_runs WHERE deployment_id=?1 AND instance_index=?2 AND state NOT IN ('succeeded','failed'))
                 OR EXISTS(SELECT 1 FROM lifecycle_claims WHERE deployment_id=?1 AND instance_index=?2)
                 OR EXISTS(SELECT 1 FROM request_leases WHERE deployment_id=?1 AND instance_index=?2)
                 OR EXISTS(SELECT 1 FROM resource_owners WHERE deployment_id=?1 AND instance_index=?2)",
            params![deployment_id, instance_index],
            |r| r.get(0),
        )?)
    }
}

#[cfg(test)]
mod tests;
