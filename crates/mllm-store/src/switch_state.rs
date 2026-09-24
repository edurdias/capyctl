//! W10 gaps (owner decisions 2026-09-23): why a dispatch gate is closed, the
//! switch in progress, and the warm-residency flag.
//!
//! **Closure reasons.** SPEC §10 lets a failed switch reopen the victims it
//! closed, and SPEC §13.2 closes a gate when the host session that proved
//! readiness is gone or the engine exits. Both write the same
//! `dispatch_enabled` bit, so a switch that failed during its drain window used
//! to reopen a gate a host loss had closed meanwhile. Each closure now records
//! its reason for the exact instance incarnation, and a gate reopens only when
//! the reopening party removes its own reason and no other reason remains.
//!
//! **Switch in progress.** The switch itself lives in the coordinator's memory
//! (`mllm_controller::switching`); its phase is mirrored here so status can
//! show it. A new coordinator session clears it: no switch survives the
//! process that ran it, and its closures are cleared with it.
//!
//! **Warm residency** (SPEC §6.5, ADR 0013 amendment 2026-09-23): a deployment
//! declaring `lifecycle.warm: true` is never chosen as a switch victim, never
//! parked or stopped by the idle policy, and never reclaimed from the parked
//! set by capacity. Only an explicit stop (or a recovery action) ends it.
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::Serialize;

/// Why an instance incarnation's dispatch gate is closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClosureReason {
    /// SPEC §10 step 3: a switch closed it to drain the victim.
    Switch,
    /// SPEC §13.2: the host session that proved readiness is gone, or the host
    /// is unresponsive or draining.
    HostSession,
    /// SPEC §13.2 (W13): an owned engine process exited.
    EngineExit,
}

impl ClosureReason {
    fn code(self) -> &'static str {
        match self {
            Self::Switch => "switch",
            Self::HostSession => "host_session",
            Self::EngineExit => "engine_exit",
        }
    }
}

/// Schema v28 data step: add the warm-residency flag unless it is present.
pub(crate) fn migrate(tx: &Transaction<'_>) -> rusqlite::Result<()> {
    let mut statement = tx.prepare("PRAGMA table_info(deployment_revision_instances)")?;
    let present = statement
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .iter()
        .any(|name| name == "warm");
    drop(statement);
    if !present {
        tx.execute_batch(
            "ALTER TABLE deployment_revision_instances ADD COLUMN warm INTEGER NOT NULL DEFAULT 0 CHECK(warm IN (0,1));",
        )?;
    }
    Ok(())
}

/// Record `reason` for the incarnation. Idempotent.
pub(crate) fn record_closure(
    conn: &Connection,
    deployment: &str,
    instance: u32,
    generation: i64,
    reason: ClosureReason,
) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "INSERT OR IGNORE INTO dispatch_closures(deployment_id,instance_index,generation,reason) VALUES(?1,?2,?3,?4)",
        params![deployment, instance, generation, reason.code()],
    )? == 1)
}

/// Remove `reason` for the incarnation. Returns whether it was recorded.
pub(crate) fn clear_closure(
    conn: &Connection,
    deployment: &str,
    instance: u32,
    generation: i64,
    reason: ClosureReason,
) -> rusqlite::Result<bool> {
    Ok(conn.execute(
        "DELETE FROM dispatch_closures WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3 AND reason=?4",
        params![deployment, instance, generation, reason.code()],
    )? == 1)
}

/// Whether `reason` is recorded for the incarnation.
pub(crate) fn has_closure(
    conn: &Connection,
    deployment: &str,
    instance: u32,
    generation: i64,
    reason: ClosureReason,
) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM dispatch_closures WHERE deployment_id=?1 AND instance_index=?2 AND generation=?3 AND reason=?4)",
        params![deployment, instance, generation, reason.code()],
        |r| r.get(0),
    )
}

/// Retire every closure reason of `deployment` that no longer names a current
/// instance incarnation: a reason is kept per incarnation, and once its
/// instance's generation moved on (a fence) or the instance row is gone
/// (retired, compacted away, deleted) nothing can ever reopen through it.
pub(crate) fn prune_closures(conn: &Connection, deployment: &str) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM dispatch_closures WHERE deployment_id=?1 AND NOT EXISTS(
            SELECT 1 FROM deployment_instances i WHERE i.deployment_id=dispatch_closures.deployment_id
              AND i.instance_index=dispatch_closures.instance_index AND i.generation=dispatch_closures.generation)",
        [deployment],
    )?;
    Ok(())
}

/// Retire every closure reason of one instance, whatever its generation: its
/// runtime is proven gone, so no gate of it is open or closed any more, and a
/// later start that keeps its generation must not inherit a dead reason.
pub(crate) fn clear_instance_closures(
    conn: &Connection,
    deployment: &str,
    instance: u32,
) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM dispatch_closures WHERE deployment_id=?1 AND instance_index=?2",
        params![deployment, instance],
    )?;
    Ok(())
}

/// SQL: no closure reason remains for the incarnation of `alias` (a
/// `deployment_instances` row). A gate reopens only under this clause.
pub(crate) fn no_closure_clause(alias: &str) -> String {
    format!(
        "NOT EXISTS(SELECT 1 FROM dispatch_closures c WHERE c.deployment_id={alias}.deployment_id AND c.instance_index={alias}.instance_index AND c.generation={alias}.generation)"
    )
}

/// SQL: the deployment of `alias` (any row with a `deployment_id`) declares
/// warm residency on its current revision (SPEC §6.5).
pub(crate) fn warm_clause(alias: &str) -> String {
    format!(
        "EXISTS(SELECT 1 FROM deployment_revision_instances w JOIN deployments wd ON wd.id=w.deployment_id AND wd.revision=w.revision WHERE w.deployment_id={alias}.deployment_id AND w.warm=1)"
    )
}

/// SQL: the deployment of `alias` (a `deployment_instances` row) is the target
/// of a switch in progress on the instance's host (SPEC §10). Its parked
/// instance is waking, not reclaimable (found live 2026-09-23, matrix M31).
pub(crate) fn switch_target_clause(alias: &str) -> String {
    format!(
        "EXISTS(SELECT 1 FROM active_switches sw WHERE sw.target_deployment={alias}.deployment_id AND (sw.host_id IS NULL OR {alias}.host_id IS NULL OR sw.host_id={alias}.host_id))"
    )
}

/// A new coordinator session: no switch survives the process that ran it.
pub(crate) fn clear_switches(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM dispatch_closures WHERE reason='switch'", [])?;
    conn.execute("DELETE FROM active_switches", [])?;
    Ok(())
}

/// A switch in progress as status shows it on the target's and every
/// victim's deployment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SwitchStatus {
    pub switch_id: String,
    /// `target` on the deployment the switch makes room for, `victim` on a
    /// deployment it releases an instance of.
    pub role: &'static str,
    pub target: String,
    pub host: Option<String>,
    /// `<deployment>/<instance>` of every victim.
    pub victims: Vec<String>,
    /// `planned`, `admission_closed` or `released`.
    pub phase: String,
    /// Started by an explicit `start --evict` rather than a request.
    pub explicit: bool,
}

/// Mirror one switch transition. A terminal phase removes it.
pub(crate) fn record_switch_phase(
    conn: &Connection,
    switch_id: &str,
    target: &str,
    host: Option<&str>,
    victims: &[String],
    phase: Option<&str>,
    explicit: bool,
) -> rusqlite::Result<()> {
    if switch_id.is_empty() {
        return Ok(());
    }
    match phase {
        None => {
            conn.execute(
                "DELETE FROM active_switches WHERE switch_id=?1",
                [switch_id],
            )?;
        }
        Some(phase) => {
            let victims = serde_json::to_string(victims).unwrap_or_else(|_| "[]".into());
            conn.execute(
                "INSERT INTO active_switches(switch_id,target_deployment,host_id,victims_json,phase,evicting)
                 VALUES(?1,?2,?3,?4,?5,?6)
                 ON CONFLICT(switch_id) DO UPDATE SET phase=excluded.phase,victims_json=excluded.victims_json,host_id=excluded.host_id",
                params![switch_id, target, host, victims, phase, explicit],
            )?;
        }
    }
    Ok(())
}

/// The switch in progress that names `deployment` as its target or a victim.
pub(crate) fn status(
    conn: &Connection,
    deployment: &str,
) -> rusqlite::Result<Option<SwitchStatus>> {
    type Row = (String, String, Option<String>, String, String, bool);
    let rows: Vec<Row> = conn
        .prepare(
            "SELECT switch_id,target_deployment,host_id,victims_json,phase,evicting FROM active_switches ORDER BY switch_id",
        )?
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    for (switch_id, target, host, victims_json, phase, explicit) in rows {
        let victims: Vec<String> = serde_json::from_str(&victims_json).unwrap_or_default();
        let victim = victims
            .iter()
            .any(|v| v.rsplit_once('/').is_some_and(|(d, _)| d == deployment));
        if target == deployment || victim {
            return Ok(Some(SwitchStatus {
                switch_id,
                role: if target == deployment {
                    "target"
                } else {
                    "victim"
                },
                target,
                host,
                victims,
                phase,
                explicit,
            }));
        }
    }
    Ok(None)
}

/// Whether the deployment's current revision declares warm residency.
pub(crate) fn is_warm(conn: &Connection, deployment: &str) -> rusqlite::Result<bool> {
    Ok(conn
        .query_row(
            "SELECT w.warm FROM deployment_revision_instances w JOIN deployments d ON d.id=w.deployment_id AND d.revision=w.revision WHERE w.deployment_id=?1",
            [deployment],
            |r| r.get::<_, bool>(0),
        )
        .optional()?
        .unwrap_or(false))
}
