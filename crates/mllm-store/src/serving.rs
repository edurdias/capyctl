//! ADR 0013 §10 (unit I3): the router's read of a deployment's serving
//! instances.
//!
//! A deployment with several instances has one retained binding per instance
//! (ADR 0013 §5). The router balances across them, so it needs every instance
//! that holds a runtime together with the facts its choice depends on: the
//! instance's generation (which fences its request lease and names its load
//! samples), the host whose ingress serves it, the launch command the host
//! reports load under, and whether its dispatch gate is open. The gate here is
//! only a hint for ranking: the lease grant re-checks it in its own transaction
//! (SPEC §10, T18), and the forwarder is resolved for exactly the generation the
//! lease names.

use rusqlite::{params, OptionalExtension};

use crate::lifecycle::{LifecycleError, StoredRuntimeBinding};

/// SPEC §17: the most instances one read returns. The configuration bound is
/// 64 instances per deployment (`mllm_config::instances::MAX_INSTANCES`).
pub const MAX_SERVING_INSTANCES: usize = 64;

/// One instance of a deployment that holds a retained runtime binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServingInstanceRow {
    pub instance_index: u32,
    /// ADR 0013 §5: one generation per instance activation, drawn from the
    /// deployment's counter, so it identifies this incarnation alone.
    pub generation: i64,
    pub revision: i64,
    pub binding_id: String,
    pub incarnation: String,
    /// The instance's placed host (`None` before placement is recorded).
    pub host_id: Option<String>,
    /// The enrolled host whose private ingress serves this binding; `None` for
    /// an engine the embedded (standalone) role runs itself.
    pub remote_host: Option<String>,
    /// The completed Initialize step that launched this binding. A host reports
    /// the engine's load under it (`LoadView::owned_handle`).
    pub launch_command_id: Option<String>,
    /// Ready, admission and dispatch open, and the deployment not suspended.
    pub dispatch_open: bool,
    /// SPEC §13.2 (W13): an owned process of this binding's launch exited;
    /// its dispatch is closed until cleanup settles it.
    pub engine_exited: bool,
}

/// The gate a dispatch needs, over `deployment_instances i` and `deployments d`.
/// The same predicate `dispatch::grant_in` refuses a lease on.
const OPEN: &str = "(i.observed_state='ready' AND i.admission_enabled=1 AND i.dispatch_enabled=1 AND d.suspended=0)";

impl crate::Store {
    /// Every instance of `deployment_id` that holds a retained binding, by
    /// index, at most [`MAX_SERVING_INSTANCES`].
    pub fn serving_instances(
        &self,
        deployment_id: &str,
    ) -> Result<Vec<ServingInstanceRow>, LifecycleError> {
        let sql = format!(
            "SELECT i.instance_index,i.generation,i.revision,b.id,b.incarnation,i.host_id,r.host_id,
                    (SELECT s.id FROM lifecycle_steps s JOIN operations o ON o.id=s.operation_id
                      WHERE s.binding_id=b.id AND o.kind='initialize' AND s.state='completed'
                      ORDER BY s.id DESC LIMIT 1),
                    {OPEN},
                    EXISTS(SELECT 1 FROM lifecycle_steps x JOIN operations o ON o.id=x.operation_id
                            JOIN journal_entries j ON j.operation_id=o.id
                      WHERE x.binding_id=b.id AND o.kind='initialize' AND j.state='engine_exited')
               FROM deployment_instances i
               JOIN deployments d ON d.id=i.deployment_id
               JOIN runtime_bindings b ON b.deployment_id=i.deployment_id
                    AND b.instance_index=i.instance_index AND b.state!='released'
               LEFT JOIN remote_binding_ingress r ON r.binding_id=b.id
              WHERE i.deployment_id=?1 AND i.generation IS NOT NULL AND i.revision IS NOT NULL
              ORDER BY i.instance_index LIMIT ?2"
        );
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement
            .query_map(params![deployment_id, MAX_SERVING_INSTANCES as i64], |r| {
                Ok(ServingInstanceRow {
                    instance_index: r.get(0)?,
                    generation: r.get(1)?,
                    revision: r.get(2)?,
                    binding_id: r.get(3)?,
                    incarnation: r.get(4)?,
                    host_id: r.get(5)?,
                    remote_host: r.get(6)?,
                    launch_command_id: r.get(7)?,
                    dispatch_open: r.get(8)?,
                    engine_exited: r.get(9)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The retained binding of exactly the instance incarnation `generation`
    /// names, with its dispatch gate. `None` once that incarnation holds no
    /// binding (stopped, replaced, or never launched): a lease granted for it
    /// must then not be forwarded anywhere else (ADR 0013 §10).
    pub fn serving_binding_at(
        &self,
        deployment_id: &str,
        generation: i64,
    ) -> Result<Option<(StoredRuntimeBinding, bool)>, LifecycleError> {
        let chosen: Option<(String, bool)> = self
            .conn
            .query_row(
                &format!(
                    "SELECT b.id,{OPEN}
                       FROM deployment_instances i
                       JOIN deployments d ON d.id=i.deployment_id
                       JOIN runtime_bindings b ON b.deployment_id=i.deployment_id
                            AND b.instance_index=i.instance_index AND b.state!='released'
                      WHERE i.deployment_id=?1 AND i.generation=?2"
                ),
                params![deployment_id, generation],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((id, open)) = chosen else {
            return Ok(None);
        };
        Ok(self.retained_binding(&id)?.map(|binding| (binding, open)))
    }
}
