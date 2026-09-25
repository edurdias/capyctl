//! SPEC §§7 and 11: stable host ownership without rewriting reservation receipts.
use crate::resource_policy::{read_policy, ResourcePolicyError};
use mllm_config::resource_controls::ResourceContext;
use rusqlite::{params, OptionalExtension, Transaction};

pub(crate) fn insert(
    tx: &Transaction<'_>,
    host_id: &str,
    policy_key: &str,
    kind: &str,
    local: &ResourceContext,
    scoped: &ResourceContext,
) -> Result<(), ResourcePolicyError> {
    tx.execute(
        "INSERT INTO host_resource_namespaces(host_id,policy_key,kind) VALUES(?1,?2,?3)",
        params![host_id, policy_key, kind],
    )?;
    // The caller constructs scoped keys from each local ID, never from map order.
    for id in &local.domain_ids {
        let key = if kind == "embedded" {
            id.clone()
        } else {
            ledger_key(host_id, "domain", id)
        };
        if !scoped.domain_ids.contains(&key) {
            return Err(ResourcePolicyError::Invalid);
        }
        tx.execute(
            "INSERT INTO host_resource_keys VALUES(?1,'domain',?2,?3)",
            params![host_id, id, key],
        )?;
    }
    for id in local.device_domains.keys() {
        let key = if kind == "embedded" {
            id.clone()
        } else {
            ledger_key(host_id, "device", id)
        };
        if !scoped.device_domains.contains_key(&key) {
            return Err(ResourcePolicyError::Invalid);
        }
        tx.execute(
            "INSERT INTO host_resource_keys VALUES(?1,'device',?2,?3)",
            params![host_id, id, key],
        )?;
    }
    Ok(())
}
pub(crate) fn ledger_key(host: &str, kind: &str, local: &str) -> String {
    mllm_config::remote_resources::ledger_key(host, kind, local)
}
pub(crate) fn ensure_embedded(
    tx: &Transaction<'_>,
    context: &ResourceContext,
) -> Result<(), ResourcePolicyError> {
    let current: Option<String> = tx
        .query_row(
            "SELECT policy_key FROM host_resource_namespaces WHERE kind='embedded'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(key) = current {
        return if key == context.host_id {
            Ok(())
        } else {
            Err(ResourcePolicyError::RevisionConflict)
        };
    }
    let policies: i64 = tx.query_row("SELECT count(*) FROM host_resource_policies", [], |r| {
        r.get(0)
    })?;
    // An unmapped existing policy has ambiguous provenance. Never guess a host.
    if policies != 0 {
        return Err(ResourcePolicyError::NeedsReconciliation);
    }
    insert(
        tx,
        &ulid::Ulid::new().to_string(),
        &context.host_id,
        "embedded",
        context,
        context,
    )
}
pub(crate) fn ensure_resolved(tx: &Transaction<'_>) -> Result<(), ResourcePolicyError> {
    let unresolved: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM host_resource_policies p WHERE NOT EXISTS(SELECT 1 FROM host_resource_namespaces n WHERE n.policy_key=p.host_id))",[],|r|r.get(0))?;
    if unresolved {
        Err(ResourcePolicyError::NeedsReconciliation)
    } else {
        Ok(())
    }
}
pub(crate) fn selected_policy_key(
    tx: &Transaction<'_>,
    host: &str,
) -> Result<Option<String>, ResourcePolicyError> {
    ensure_resolved(tx)?;
    let keys: Vec<String> = tx
        .prepare(
            "SELECT policy_key FROM host_resource_namespaces WHERE host_id=?1 OR policy_key=?1",
        )?
        .query_map([host], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    match keys.as_slice() {
        [] => {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM host_resource_policies WHERE host_id=?1)",
                [host],
                |r| r.get(0),
            )?;
            if exists {
                Err(ResourcePolicyError::NeedsReconciliation)
            } else {
                Ok(None)
            }
        }
        [key] => Ok(Some(key.clone())),
        _ => Err(ResourcePolicyError::NeedsReconciliation),
    }
}
/// Upgrade only the historically supported singleton; ambiguous databases stay closed.
pub(crate) fn migrate(tx: &Transaction<'_>) -> Result<(), rusqlite::Error> {
    let keys: Vec<String> = tx
        .prepare("SELECT host_id FROM host_resource_policies ORDER BY host_id LIMIT 2")?
        .query_map([], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    if let [key] = keys.as_slice() {
        let policy = read_policy(tx, key)
            .map_err(|_| rusqlite::Error::InvalidQuery)?
            .ok_or(rusqlite::Error::InvalidQuery)?;
        insert(
            tx,
            &ulid::Ulid::new().to_string(),
            key,
            "embedded",
            &policy.context,
            &policy.context,
        )
        .map_err(|_| rusqlite::Error::InvalidQuery)?;
    }
    Ok(())
}
impl crate::Store {
    pub fn embedded_host_id(&self) -> Result<Option<String>, ResourcePolicyError> {
        Ok(self
            .conn
            .query_row(
                "SELECT host_id FROM host_resource_namespaces WHERE kind='embedded'",
                [],
                |r| r.get(0),
            )
            .optional()?)
    }
    /// Resolve explicit host/local identity to its persisted accounting key.
    pub fn host_resource_key(
        &self,
        host: &str,
        kind: &str,
        local: &str,
    ) -> Result<Option<String>, ResourcePolicyError> {
        Ok(self.conn.query_row("SELECT ledger_key FROM host_resource_keys WHERE host_id=?1 AND kind=?2 AND local_id=?3",params![host,kind,local],|r|r.get(0)).optional()?)
    }
}
