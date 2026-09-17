//! Stopped-only configuration acceptance. No qualification or runtime authority.
use crate::dispatch::{check_session, CoordinatorSession};
use crate::events::{append_event, EventMetadata, EventOperationId};
use crate::resource_policy::read_singleton_policy;
use mllm_config::effective::{deployment_command_fingerprint, resolve_effective};
use mllm_config::resource_controls::{ResourceContext, ResourceControls};
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, value::RawValue, Value};
use sha2::{Digest, Sha256};

const MAX_BYTES: usize = 1 << 20;

#[derive(Debug, thiserror::Error)]
pub enum ManagedConfigurationError {
    #[error("invalid stopped configuration command")]
    Invalid,
    #[error("stale coordinator session")]
    StaleSession,
    #[error("idempotency conflict")]
    IdempotencyConflict,
    #[error("configuration revision conflict")]
    RevisionConflict,
    #[error("route or deployment name conflict")]
    RouteConflict,
    #[error("runtime retained")]
    RuntimeRetained,
    #[error("current resource policy required")]
    PolicyConflict,
    #[error("corrupt stored configuration")]
    CorruptStoredData,
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
}
type Result<T> = std::result::Result<T, ManagedConfigurationError>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedConfigurationReceipt {
    pub version: u8,
    pub operation_id: String,
    pub deployment_id: String,
    pub revision: i64,
    pub generation: i64,
    pub resource_policy_revision: i64,
    pub accepted_at_ms: i64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredReceiptV2 {
    version: u8,
    receipt: ManagedConfigurationReceipt,
    command_fingerprint: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateCommand<'a> {
    #[serde(borrow)]
    config: &'a RawValue,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplaceCommand<'a> {
    #[serde(borrow)]
    config: &'a RawValue,
    expected_revision: i64,
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

impl crate::Store {
    /// Service-only trusted host configuration must contain the current persisted
    /// resource controls, not stale startup values. This never imports policy,
    /// validates qualification authority, reserves a runtime, or enables dispatch.
    #[allow(clippy::too_many_arguments)]
    pub fn create_stopped_managed_configuration(
        &self,
        session: &CoordinatorSession,
        principal: &str,
        key: &str,
        request_json: &str,
        trusted_host: &Value,
        now_ms: i64,
    ) -> Result<ManagedConfigurationReceipt> {
        self.accept_stopped_configuration(
            session,
            principal,
            key,
            None,
            request_json,
            trusted_host,
            now_ms,
        )
    }

    /// Revision-aware replacement after all owned effects have been cleaned up.
    /// Historical revisions/receipts remain immutable; this is never a hot update.
    #[allow(clippy::too_many_arguments)]
    pub fn replace_stopped_managed_configuration(
        &self,
        session: &CoordinatorSession,
        principal: &str,
        key: &str,
        deployment_id: &str,
        request_json: &str,
        trusted_host: &Value,
        now_ms: i64,
    ) -> Result<ManagedConfigurationReceipt> {
        if ulid::Ulid::from_string(deployment_id).is_err() {
            return Err(ManagedConfigurationError::Invalid);
        }
        self.accept_stopped_configuration(
            session,
            principal,
            key,
            Some(deployment_id),
            request_json,
            trusted_host,
            now_ms,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn accept_stopped_configuration(
        &self,
        session: &CoordinatorSession,
        principal: &str,
        key: &str,
        target: Option<&str>,
        request_json: &str,
        trusted_host: &Value,
        now_ms: i64,
    ) -> Result<ManagedConfigurationReceipt> {
        if request_json.len() > MAX_BYTES
            || !valid_identifier(principal)
            || !valid_identifier(key)
            || now_ms < 0
        {
            return Err(ManagedConfigurationError::Invalid);
        }
        let (raw_config, expected_revision) = if target.is_some() {
            let command: ReplaceCommand<'_> = serde_json::from_str(request_json)
                .map_err(|_| ManagedConfigurationError::Invalid)?;
            if command.expected_revision < 1 {
                return Err(ManagedConfigurationError::Invalid);
            }
            (command.config, Some(command.expected_revision))
        } else {
            let command: CreateCommand<'_> = serde_json::from_str(request_json)
                .map_err(|_| ManagedConfigurationError::Invalid)?;
            (command.config, None)
        };
        let config =
            mllm_config::parse_strict(mllm_config::ConfigKind::Deployment, raw_config.get())
                .map_err(|_| ManagedConfigurationError::Invalid)?;
        let scope = target.map_or_else(
            || "POST /management/v1/deployments/stopped".to_string(),
            |id| format!("PUT /management/v1/deployments/{id}/stopped-configuration"),
        );
        let kind = if target.is_some() {
            "managed_configuration_replace"
        } else {
            "managed_configuration_create"
        };
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, session).map_err(|_| ManagedConfigurationError::StaleSession)?;
        if let Some(receipt) = replay(
            &tx,
            principal,
            &scope,
            key,
            kind,
            target,
            expected_revision,
            &config,
            trusted_host,
        )? {
            return Ok(receipt);
        }
        let mut effective = resolve_effective(&config, trusted_host)
            .map_err(|_| ManagedConfigurationError::Invalid)?;
        effective.routes.sort();
        let effective_json =
            serde_json::to_string(&effective).map_err(|_| ManagedConfigurationError::Invalid)?;
        if effective_json.len() > MAX_BYTES {
            return Err(ManagedConfigurationError::Invalid);
        }
        let command_fingerprint =
            deployment_command_fingerprint(&config, effective.request_deadline_ms)
                .map_err(|_| ManagedConfigurationError::Invalid)?;
        let hash = format!("{:x}", Sha256::digest(serde_json::to_vec(&json!({"version":2,"scope":scope,"expected_revision":expected_revision,"effective":effective,"command_fingerprint":command_fingerprint})).map_err(|_| ManagedConfigurationError::Invalid)?));
        let policy = read_singleton_policy(&tx, &effective.host.name)
            .map_err(|_| ManagedConfigurationError::PolicyConflict)?
            .ok_or(ManagedConfigurationError::PolicyConflict)?;
        if policy.context != ResourceContext::from_host(&effective.host)
            || policy.controls != ResourceControls::from_host(&effective.host)
        {
            return Err(ManagedConfigurationError::PolicyConflict);
        }
        let (revision, generation) = if let Some(id) = target {
            replacement_fence(
                &tx,
                id,
                expected_revision.ok_or(ManagedConfigurationError::Invalid)?,
            )?
        } else {
            (1, 1)
        };
        ensure_routes(
            &tx,
            &effective.name,
            &effective.routes,
            target.unwrap_or(""),
        )?;
        let deployment = match target {
            Some(id) => {
                ulid::Ulid::from_string(id).map_err(|_| ManagedConfigurationError::Invalid)?
            }
            None => ulid::Ulid::new(),
        };
        let operation = ulid::Ulid::new();
        let receipt = ManagedConfigurationReceipt {
            version: 1,
            operation_id: operation.to_string(),
            deployment_id: deployment.to_string(),
            revision,
            generation,
            resource_policy_revision: policy.revision,
            accepted_at_ms: now_ms,
        };
        if target.is_some() {
            tx.execute("UPDATE deployments SET name=?2,revision=?3,current_generation=?4,route_model_id=NULL,updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id=?1",params![receipt.deployment_id,effective.name,revision,generation])?;
            tx.execute(
                "DELETE FROM deployment_routes WHERE deployment_id=?1",
                [&receipt.deployment_id],
            )?;
        } else {
            tx.execute("INSERT INTO deployments(id,name,kind,desired_state,observed_state,admission_enabled,dispatch_enabled,suspended,current_generation,schema_version,revision) VALUES(?1,?2,'model','stopped','stopped',0,0,0,1,1,1)",params![receipt.deployment_id,effective.name])?;
        }
        for route in &effective.routes {
            tx.execute(
                "INSERT INTO deployment_routes(route,deployment_id) VALUES(?1,?2)",
                params![route, receipt.deployment_id],
            )?;
        }
        tx.execute("INSERT INTO effective_revisions(deployment_id,revision,effective_json,fingerprint) VALUES(?1,?2,?3,?4)",params![receipt.deployment_id,revision,effective_json,effective.recipe_fingerprint])?;
        persist_receipt(
            &tx,
            principal,
            &scope,
            key,
            &hash,
            &receipt,
            kind,
            &command_fingerprint,
        )?;
        append_event(
            &tx,
            &EventMetadata::ManagedConfigurationAccepted {
                operation_id: EventOperationId::generated(operation),
                deployment_id: EventOperationId::generated(deployment),
                revision,
                generation,
                session_epoch: session.epoch(),
            },
        )
        .map_err(|_| ManagedConfigurationError::CorruptStoredData)?;
        tx.commit()?;
        Ok(receipt)
    }
}

fn ensure_routes(tx: &Transaction<'_>, name: &str, routes: &[String], own: &str) -> Result<()> {
    // Reconcile every legacy alias before accepting any new route. UNION
    // deduplicates the same deployment represented in both old and new tables.
    let ambiguous: bool = tx.query_row("SELECT EXISTS(SELECT route FROM (SELECT route,deployment_id FROM deployment_routes UNION SELECT route_model_id,id FROM deployments WHERE route_model_id IS NOT NULL) GROUP BY route HAVING count(*)>1)",[],|r|r.get(0))?;
    if ambiguous {
        return Err(ManagedConfigurationError::RouteConflict);
    }
    let name_exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM deployments WHERE name=?1 AND id!=?2)",
        params![name, own],
        |r| r.get(0),
    )?;
    if name_exists {
        return Err(ManagedConfigurationError::RouteConflict);
    }
    for route in routes {
        let collision: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM deployment_routes WHERE route=?1 AND deployment_id!=?2 UNION ALL SELECT 1 FROM deployments WHERE route_model_id=?1 AND id!=?2)",params![route,own],|r|r.get(0))?;
        if collision {
            return Err(ManagedConfigurationError::RouteConflict);
        }
    }
    Ok(())
}

/// Link a frozen revision to its actual acceptance receipt without re-resolving
/// mutable profiles. Ordinary lifecycle authority must validate all settings,
/// including fields deliberately excluded from qualification identity.
pub(crate) fn validate_revision_history(
    tx: &Transaction<'_>,
    deployment: &str,
    revision: i64,
    effective_json: &str,
) -> Result<()> {
    let mut statement=tx.prepare("SELECT c.response_json,c.request_hash,c.command_scope,c.operation_id,o.kind FROM command_receipts c JOIN operations o ON o.id=c.operation_id WHERE o.deployment_id=?1 AND o.kind IN ('managed_configuration_create','managed_configuration_replace') AND o.state='succeeded' AND o.error_code IS NULL AND COALESCE(json_extract(c.response_json,'$.receipt.revision'),json_extract(c.response_json,'$.revision'))=?2")?;
    let rows=statement.query_map(params![deployment,revision],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?)))?.collect::<std::result::Result<Vec<_>,_>>()?;
    let [(body,hash,scope,operation,kind)]=rows.as_slice() else { return Err(ManagedConfigurationError::CorruptStoredData); };
    if body.len()>MAX_BYTES || effective_json.len()>MAX_BYTES { return Err(ManagedConfigurationError::CorruptStoredData); }
    let envelope:Value=serde_json::from_str(body).map_err(|_|ManagedConfigurationError::CorruptStoredData)?;
    let (receipt,command_fingerprint)=if envelope["version"]==2 {
        let stored:StoredReceiptV2=serde_json::from_str(body).map_err(|_|ManagedConfigurationError::CorruptStoredData)?;
        (stored.receipt,Some(stored.command_fingerprint))
    } else {
        (serde_json::from_str::<ManagedConfigurationReceipt>(body).map_err(|_|ManagedConfigurationError::CorruptStoredData)?,None)
    };
    let create=kind=="managed_configuration_create";
    let expected_scope=if create { "POST /management/v1/deployments/stopped".into() } else { format!("PUT /management/v1/deployments/{deployment}/stopped-configuration") };
    if receipt.version!=1 || receipt.deployment_id!=deployment || receipt.revision!=revision || receipt.operation_id!=*operation || receipt.generation<1 || receipt.accepted_at_ms<0 || receipt.resource_policy_revision<1 || *scope!=expected_scope || (create && revision!=1) || (!create && revision<=1) || command_fingerprint.as_ref().is_some_and(|fingerprint| fingerprint.len()!=64 || !fingerprint.bytes().all(|b|b.is_ascii_digit()||(b'a'..=b'f').contains(&b))) {
        return Err(ManagedConfigurationError::CorruptStoredData);
    }
    let effective:Value=serde_json::from_str(effective_json).map_err(|_|ManagedConfigurationError::CorruptStoredData)?;
    let mut input=json!({"version":1,"scope":scope,"expected_revision":if create {None} else {Some(revision-1)},"effective":effective});
    if let Some(fingerprint)=command_fingerprint {
        input["version"]=json!(2);
        input["command_fingerprint"]=json!(fingerprint);
    }
    let computed=format!("{:x}",Sha256::digest(serde_json::to_vec(&input).map_err(|_|ManagedConfigurationError::CorruptStoredData)?));
    if computed!=*hash { return Err(ManagedConfigurationError::CorruptStoredData); }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn replay(
    tx: &Transaction<'_>,
    principal: &str,
    scope: &str,
    key: &str,
    kind: &str,
    target: Option<&str>,
    requested_revision: Option<i64>,
    config: &Value,
    trusted_host: &Value,
) -> Result<Option<ManagedConfigurationReceipt>> {
    let row: Option<(String,String,String)> = tx.query_row("SELECT request_hash,operation_id,response_json FROM command_receipts WHERE principal_id=?1 AND command_scope=?2 AND idempotency_key=?3",params![principal,scope,key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let Some((stored_hash, operation, body)) = row else {
        return Ok(None);
    };
    if body.len() > MAX_BYTES {
        return Err(ManagedConfigurationError::CorruptStoredData);
    }
    let envelope: Value =
        serde_json::from_str(&body).map_err(|_| ManagedConfigurationError::CorruptStoredData)?;
    let (receipt, command_fingerprint) = if envelope["version"] == 2 {
        let stored: StoredReceiptV2 = serde_json::from_str(&body)
            .map_err(|_| ManagedConfigurationError::CorruptStoredData)?;
        if stored.command_fingerprint.len() != 64
            || !stored
                .command_fingerprint
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(ManagedConfigurationError::CorruptStoredData);
        }
        (stored.receipt, Some(stored.command_fingerprint))
    } else {
        (
            serde_json::from_str(&body)
                .map_err(|_| ManagedConfigurationError::CorruptStoredData)?,
            None,
        )
    };
    if receipt.version != 1
        || receipt.operation_id != operation
        || receipt.revision < 1
        || receipt.generation < 1
        || receipt.resource_policy_revision < 1
        || receipt.accepted_at_ms < 0
    {
        return Err(ManagedConfigurationError::CorruptStoredData);
    }
    let valid: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM operations o JOIN effective_revisions e ON e.deployment_id=o.deployment_id WHERE o.id=?1 AND o.deployment_id=?2 AND o.kind=?3 AND o.state='succeeded' AND o.error_code IS NULL AND e.revision=?4)",params![operation,receipt.deployment_id,kind,receipt.revision],|r|r.get(0))?;
    if !valid
        || target.is_some_and(|id| id != receipt.deployment_id)
        || ulid::Ulid::from_string(&receipt.deployment_id).is_err()
    {
        return Err(ManagedConfigurationError::CorruptStoredData);
    }
    let frozen: String = tx.query_row(
        "SELECT effective_json FROM effective_revisions WHERE deployment_id=?1 AND revision=?2",
        params![receipt.deployment_id, receipt.revision],
        |r| r.get(0),
    )?;
    if frozen.len() > MAX_BYTES {
        return Err(ManagedConfigurationError::CorruptStoredData);
    }
    let effective: Value =
        serde_json::from_str(&frozen).map_err(|_| ManagedConfigurationError::CorruptStoredData)?;
    let expected_revision = target.map(|_| receipt.revision - 1);
    let mut frozen_input = json!({"version":1,"scope":scope,"expected_revision":expected_revision,"effective":effective});
    if let Some(fingerprint) = &command_fingerprint {
        frozen_input["version"] = json!(2);
        frozen_input["command_fingerprint"] = json!(fingerprint);
    }
    let frozen_hash = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&frozen_input)
                .map_err(|_| ManagedConfigurationError::CorruptStoredData)?
        )
    );
    if frozen_hash != stored_hash {
        return Err(ManagedConfigurationError::CorruptStoredData);
    }
    if requested_revision != expected_revision {
        return Err(ManagedConfigurationError::IdempotencyConflict);
    }
    if let Some(fingerprint) = command_fingerprint {
        let deadline = effective["request_deadline_ms"]
            .as_i64()
            .filter(|v| *v > 0)
            .ok_or(ManagedConfigurationError::CorruptStoredData)?;
        let requested = deployment_command_fingerprint(config, deadline)
            .map_err(|_| ManagedConfigurationError::IdempotencyConflict)?;
        if requested != fingerprint {
            return Err(ManagedConfigurationError::IdempotencyConflict);
        }
    } else {
        // Pre-V2 receipts did not retain independent command identity. Preserve
        // their exact old resolution rule; never rewrite historical receipts.
        let mut requested = resolve_effective(config, trusted_host)
            .map_err(|_| ManagedConfigurationError::IdempotencyConflict)?;
        requested.routes.sort();
        if serde_json::to_value(requested).map_err(|_| ManagedConfigurationError::Invalid)?
            != effective
        {
            return Err(ManagedConfigurationError::IdempotencyConflict);
        }
    }
    Ok(Some(receipt))
}

fn replacement_fence(tx: &Transaction<'_>, id: &str, expected: i64) -> Result<(i64, i64)> {
    let row:Option<(i64,i64,bool)> = tx.query_row("SELECT revision,current_generation,(desired_state='stopped' AND observed_state='stopped' AND admission_enabled=0 AND dispatch_enabled=0 AND suspended=0 AND kind='model') FROM deployments WHERE id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let (revision, generation, stopped) = row.ok_or(ManagedConfigurationError::RevisionConflict)?;
    if revision != expected {
        return Err(ManagedConfigurationError::RevisionConflict);
    }
    if !stopped {
        return Err(ManagedConfigurationError::RuntimeRetained);
    }
    let retained: bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM runtime_bindings WHERE deployment_id=?1 AND state!='released' UNION ALL SELECT 1 FROM endpoint_leases e JOIN runtime_bindings b ON b.id=e.binding_id WHERE b.deployment_id=?1 UNION ALL SELECT 1 FROM request_leases WHERE deployment_id=?1 UNION ALL SELECT 1 FROM resource_owners WHERE owner_id=?1 UNION ALL SELECT 1 FROM owners WHERE deployment_id=?1 UNION ALL SELECT 1 FROM lifecycle_claims WHERE deployment_id=?1 UNION ALL SELECT 1 FROM lifecycle_runs WHERE deployment_id=?1 AND state NOT IN ('succeeded','failed') UNION ALL SELECT 1 FROM lifecycle_steps WHERE deployment_id=?1 AND state NOT IN ('completed','cancelled'))",[id],|r|r.get(0))?;
    let managed:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE deployment_id=?1 AND kind='managed_configuration_create' AND state='succeeded')",[id],|r|r.get(0))?;
    if retained || !managed {
        return Err(ManagedConfigurationError::RuntimeRetained);
    }
    Ok((
        revision
            .checked_add(1)
            .ok_or(ManagedConfigurationError::RevisionConflict)?,
        generation
            .checked_add(1)
            .filter(|v| *v > 1)
            .ok_or(ManagedConfigurationError::RevisionConflict)?,
    ))
}

#[allow(clippy::too_many_arguments)]
fn persist_receipt(
    tx: &Transaction<'_>,
    principal: &str,
    scope: &str,
    key: &str,
    hash: &str,
    receipt: &ManagedConfigurationReceipt,
    kind: &str,
    command_fingerprint: &str,
) -> Result<()> {
    tx.execute(
        "INSERT INTO operations(id,deployment_id,kind,state) VALUES(?1,?2,?3,'succeeded')",
        params![receipt.operation_id, receipt.deployment_id, kind],
    )?;
    let stored = StoredReceiptV2 {
        version: 2,
        receipt: receipt.clone(),
        command_fingerprint: command_fingerprint.into(),
    };
    tx.execute("INSERT INTO command_receipts(principal_id,command_scope,idempotency_key,request_hash,operation_id,response_json) VALUES(?1,?2,?3,?4,?5,?6)",params![principal,scope,key,hash,receipt.operation_id,serde_json::to_string(&stored).map_err(|_| ManagedConfigurationError::Invalid)?])?;
    Ok(())
}

#[cfg(test)]
mod tests;
