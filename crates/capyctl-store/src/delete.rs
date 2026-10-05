//! Deployment deletion (SPEC §6.3, owner decisions D11 and 2026-09-23, plan
//! unit W6). The operator command is `capyctl delete deployment <name|id>`.
//!
//! SPEC §6.3: "delete deployment: Remove route and deployment after authorized
//! cleanup. Do not delete user-owned checkpoints or cache files implicitly."
//!
//! A delete is accepted only after cleanup has already been verified: every
//! instance is stopped and nothing the deployment was ever charged for is still
//! held (no runtime binding, endpoint lease, reservation owner, request lease,
//! lifecycle claim, open run or unresolved step, and no operation still
//! pending). A delete never stops anything itself and never releases
//! accounting: a deployment still holding any of those is refused, and the
//! operator stops it (`capyctl stop deployment <name>`, or `capyctl delete
//! deployment <name> --stop`, which issues that stop from the CLI and waits for
//! it) and retries once the stop has completed with verified cleanup.
//! Uncertainty therefore keeps its accounting (AGENTS.md), and a disconnected
//! host's retained launch keeps the deployment and its route until its cleanup
//! evidence arrives.
//!
//! What an accepted delete removes, in one transaction: the deployment's
//! routes (the router stops listing them and answers 404 for them at once),
//! its instance rows and its checkpoint digest records. Model files and caches
//! on hosts are never touched; the digest rows are only records of them.
//!
//! What it keeps: the deployment row itself as a tombstone (`kind` becomes
//! `deleted` and the name is released by renaming it to `deleted/<id>`), with
//! its operations, revisions, journal, receipts and events. A later deploy of
//! the same name is a new deployment with a new id. The tombstone's journal
//! entry records the name and routes it released.
//!
//! Idempotent: the command is answered under the deployment's action scope and
//! the caller's idempotency key, so an exact retry after a lost response
//! returns the original receipt even though the deployment is gone (T09).

use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::dispatch::CoordinatorSession;
use crate::events::{append_event, EventMetadata, EventOperationId};
use crate::lifecycle::completion::{check_session, decode, encode};
use crate::lifecycle::LifecycleError;

/// The `kind` a tombstoned deployment row carries. Every lifecycle fence
/// requires `kind='model'`, so a tombstone can never be started or stopped.
pub const DELETED_KIND: &str = "deleted";

/// The accepted delete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteReceipt {
    pub operation_id: String,
    pub deployment_id: String,
    /// The name the deployment held; it is free for a new deployment.
    pub name: String,
    /// The revision the deployment was at when it was deleted.
    pub revision: i64,
    /// The routes removed from the router.
    pub routes: Vec<String>,
    pub deleted_at_ms: i64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredReceipt {
    version: u8,
    method: String,
    action: String,
    principal: String,
    scope: String,
    key: String,
    request_hash: String,
    requested_deadline_ms: i64,
    receipt: DeleteReceipt,
}

/// The same scope `start` and `stop` answer under, so one idempotency key
/// cannot name two different actions on one deployment.
fn scope(deployment: &str) -> String {
    format!("POST:/management/v1/deployments/{deployment}/actions")
}

fn hash(
    principal: &str,
    scope: &str,
    revision: i64,
    deadline: i64,
) -> Result<String, LifecycleError> {
    Ok(format!(
        "{:x}",
        Sha256::digest(encode(&(1, principal, scope, revision, "delete", deadline))?.as_bytes())
    ))
}

fn check_request(
    principal: &str,
    deployment: &str,
    expected_revision: i64,
    key: &str,
    requested_deadline: i64,
) -> Result<(), LifecycleError> {
    if principal.trim().is_empty()
        || principal.len() > 256
        || key.trim().is_empty()
        || key.len() > 256
        || ulid::Ulid::from_string(deployment).is_err()
        || expected_revision < 1
        || requested_deadline <= 0
    {
        return Err(LifecycleError::Invalid);
    }
    Ok(())
}

/// An exact earlier answer to this command, if there is one.
fn replay(
    tx: &Transaction<'_>,
    principal: &str,
    scope: &str,
    deployment: &str,
    key: &str,
    request_hash: &str,
) -> Result<Option<DeleteReceipt>, LifecycleError> {
    let prior: Option<(String, String, String)> = tx
        .query_row(
            "SELECT request_hash,operation_id,response_json FROM command_receipts WHERE principal_id=?1 AND command_scope=?2 AND idempotency_key=?3",
            params![principal, scope, key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((old_hash, operation, raw)) = prior else {
        return Ok(None);
    };
    // A different request under the same key, including a start or stop.
    if old_hash != request_hash {
        return Err(LifecycleError::IdempotencyConflict);
    }
    let stored: StoredReceipt = decode(&raw)?;
    if stored.version != 1
        || stored.method != "POST"
        || stored.action != "delete"
        || stored.principal != principal
        || stored.scope != scope
        || stored.key != key
        || stored.request_hash != request_hash
        || stored.receipt.operation_id != operation
        || stored.receipt.deployment_id != deployment
    {
        return Err(LifecycleError::CorruptStoredData);
    }
    Ok(Some(stored.receipt))
}

/// SPEC §6.3 "after authorized cleanup": whether anything the deployment was
/// charged for is still held, on any instance. Mirrors the retained set a
/// configuration replacement refuses on, plus instance state and open
/// operations. A live reservation is its `resource_owners` row; committed
/// grants are immutable history.
fn holds_anything(tx: &Transaction<'_>, deployment: &str) -> Result<bool, LifecycleError> {
    Ok(tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM runtime_bindings WHERE deployment_id=?1 AND state!='released')
             OR EXISTS(SELECT 1 FROM endpoint_leases e JOIN runtime_bindings b ON b.id=e.binding_id WHERE b.deployment_id=?1)
             OR EXISTS(SELECT 1 FROM request_leases WHERE deployment_id=?1)
             OR EXISTS(SELECT 1 FROM resource_owners WHERE deployment_id=?1)
             OR EXISTS(SELECT 1 FROM group_plans WHERE deployment_id=?1 AND state!='settled')
             OR EXISTS(SELECT 1 FROM owners WHERE deployment_id=?1)
             OR EXISTS(SELECT 1 FROM lifecycle_claims WHERE deployment_id=?1)
             OR EXISTS(SELECT 1 FROM lifecycle_runs WHERE deployment_id=?1 AND state NOT IN ('succeeded','failed'))
             OR EXISTS(SELECT 1 FROM lifecycle_steps WHERE deployment_id=?1 AND state NOT IN ('completed','cancelled'))
             OR EXISTS(SELECT 1 FROM operations WHERE deployment_id=?1 AND state IN ('pending','running'))
             OR EXISTS(SELECT 1 FROM deployment_instances WHERE deployment_id=?1
                        AND (observed_state!='stopped' OR admission_enabled!=0 OR dispatch_enabled!=0))
             OR EXISTS(SELECT 1 FROM deployments WHERE id=?1
                        AND (observed_state!='stopped' OR admission_enabled!=0 OR dispatch_enabled!=0))",
        [deployment],
        |r| r.get(0),
    )?)
}

impl crate::Store {
    /// Delete a deployment whose cleanup is already verified. See the module
    /// documentation for what is removed and what is kept.
    ///
    /// `RuntimeRetained` when anything is still held: stop the deployment and
    /// retry after the stop has completed. `NotFound` for an unknown or already
    /// deleted deployment, unless this is an exact retry.
    #[allow(clippy::too_many_arguments)]
    pub fn accept_delete_command(
        &self,
        session: &CoordinatorSession,
        principal: &str,
        deployment: &str,
        expected_revision: i64,
        key: &str,
        now: i64,
        requested_deadline: i64,
    ) -> Result<DeleteReceipt, LifecycleError> {
        check_request(
            principal,
            deployment,
            expected_revision,
            key,
            requested_deadline,
        )?;
        if now < 0 {
            return Err(LifecycleError::Invalid);
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, session)?;
        let scope = scope(deployment);
        let request_hash = hash(principal, &scope, expected_revision, requested_deadline)?;
        if let Some(receipt) = replay(&tx, principal, &scope, deployment, key, &request_hash)? {
            return Ok(receipt);
        }
        if requested_deadline <= now {
            return Err(LifecycleError::Invalid);
        }
        let row: Option<(String, String, i64, Option<String>)> = tx
            .query_row(
                "SELECT name,kind,revision,route_model_id FROM deployments WHERE id=?1",
                [deployment],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((name, kind, revision, legacy_route)) = row else {
            return Err(LifecycleError::NotFound);
        };
        if kind == DELETED_KIND {
            return Err(LifecycleError::NotFound);
        }
        if revision != expected_revision {
            return Err(LifecycleError::RevisionConflict);
        }
        // AGENTS.md: uncertainty retains accounting. Nothing is released here.
        if holds_anything(&tx, deployment)? {
            return Err(LifecycleError::RuntimeRetained);
        }
        let mut routes: Vec<String> = tx
            .prepare("SELECT route FROM deployment_routes WHERE deployment_id=?1 ORDER BY route")?
            .query_map([deployment], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        if let Some(route) = legacy_route {
            if !routes.contains(&route) {
                routes.push(route);
            }
        }
        let operation = ulid::Ulid::new();
        let deployment_ulid =
            ulid::Ulid::from_string(deployment).map_err(|_| LifecycleError::Invalid)?;
        let receipt = DeleteReceipt {
            operation_id: operation.to_string(),
            deployment_id: deployment.into(),
            name: name.clone(),
            revision,
            routes: routes.clone(),
            deleted_at_ms: now,
        };
        // SPEC §6.3: the route goes first; the router reads it per request.
        tx.execute(
            "DELETE FROM deployment_routes WHERE deployment_id=?1",
            [deployment],
        )?;
        // Records about the checkpoint, never the checkpoint.
        tx.execute(
            "DELETE FROM checkpoint_digests WHERE deployment_id=?1",
            [deployment],
        )?;
        tx.execute(
            "DELETE FROM checkpoint_host_digests WHERE deployment_id=?1",
            [deployment],
        )?;
        // ADR 0028 §11: settled group plans are history of a deleted deployment.
        tx.execute(
            "DELETE FROM group_members WHERE deployment_id=?1",
            [deployment],
        )?;
        tx.execute(
            "DELETE FROM group_plans WHERE deployment_id=?1",
            [deployment],
        )?;
        tx.execute(
            "DELETE FROM deployment_instances WHERE deployment_id=?1",
            [deployment],
        )?;
        // W10 gap (a): closure reasons are per instance incarnation; none is left.
        tx.execute(
            "DELETE FROM dispatch_closures WHERE deployment_id=?1",
            [deployment],
        )?;
        tx.execute(
            "UPDATE deployments SET kind=?2,name=?3,route_model_id=NULL,desired_state='stopped',
                    admission_enabled=0,dispatch_enabled=0,
                    updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')
              WHERE id=?1",
            params![deployment, DELETED_KIND, format!("deleted/{deployment}")],
        )?;
        tx.execute(
            "INSERT INTO operations(id,deployment_id,kind,state,error_code,idempotency_key) VALUES(?1,?2,'delete','succeeded',NULL,NULL)",
            params![receipt.operation_id, deployment],
        )?;
        tx.execute(
            "INSERT INTO journal_entries(id,host_id,operation_id,state,evidence) VALUES(?1,NULL,?2,'deleted',?3)",
            params![
                ulid::Ulid::new().to_string(),
                receipt.operation_id,
                encode(&serde_json::json!({
                    "tombstone": 1,
                    "deployment_id": deployment,
                    "name": name,
                    "revision": revision,
                    "routes": routes,
                    "principal": principal,
                }))?
            ],
        )?;
        let stored = StoredReceipt {
            version: 1,
            method: "POST".into(),
            action: "delete".into(),
            principal: principal.into(),
            scope: scope.clone(),
            key: key.into(),
            request_hash: request_hash.clone(),
            requested_deadline_ms: requested_deadline,
            receipt: receipt.clone(),
        };
        tx.execute(
            "INSERT INTO command_receipts(principal_id,command_scope,idempotency_key,request_hash,operation_id,response_json) VALUES(?1,?2,?3,?4,?5,?6)",
            params![principal, scope, key, request_hash, receipt.operation_id, encode(&stored)?],
        )?;
        append_event(
            &tx,
            &EventMetadata::DeploymentDeleted {
                operation_id: EventOperationId::generated(operation),
                deployment_id: EventOperationId::generated(deployment_ulid),
                revision,
                session_epoch: session.epoch(),
            },
        )
        .map_err(|_| LifecycleError::CorruptStoredData)?;
        tx.commit()?;
        Ok(receipt)
    }

    /// Whether the deployment has been deleted (its row is a tombstone).
    pub fn is_deleted(&self, deployment: &str) -> Result<bool, crate::StoreError> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM deployments WHERE id=?1 AND kind=?2)",
            params![deployment, DELETED_KIND],
            |r| r.get(0),
        )?)
    }
}
