use std::net::TcpListener;

use mllm_domain::completion::ProcessIdentity;
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};

use crate::dispatch::CoordinatorSession;

const MAX_DTO_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeploymentFence {
    pub deployment_id: String,
    pub revision: i64,
    pub generation: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum LifecycleError {
    #[error("store: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("stale coordinator or deployment fence")]
    Stale,
    #[error("lifecycle conflict")]
    Conflict,
    #[error("activation disabled")]
    Disabled,
    #[error("invalid lifecycle input")]
    Invalid,
    #[error("resource or evidence check failed: {0}")]
    Rejected(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReserveBinding {
    pub id: String,
    pub fence: DeploymentFence,
    pub incarnation: String,
    pub qualification_id: String,
    pub ownership: String,
    pub endpoint_host: String,
    pub endpoint_port: u16,
    pub credential_ref: String,
    pub binding_payload: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredRuntimeBinding {
    pub id: String,
    pub deployment_id: String,
    pub revision: i64,
    pub incarnation: String,
    pub qualification_id: String,
    pub ownership: String,
    pub endpoint: String,
    pub credential_ref: String,
    pub identities: Vec<ProcessIdentity>,
    pub state: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BindingDto {
    version: u32,
    qualification_id: String,
    endpoint: String,
    credential_ref: String,
    payload: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityDto {
    role: String,
    pid: u32,
    boot_id: String,
    start_ticks: u64,
}

fn valid_text(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_DTO_BYTES
}

fn fenced(
    transaction: &Transaction<'_>,
    session: &CoordinatorSession,
    fence: &DeploymentFence,
) -> Result<(), LifecycleError> {
    crate::dispatch::check_session(transaction, session).map_err(|_| LifecycleError::Stale)?;
    let current: Option<(i64, i64)> = transaction
        .query_row(
            "SELECT revision,current_generation FROM deployments WHERE id=?1",
            [&fence.deployment_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if current != Some((fence.revision, fence.generation)) {
        return Err(LifecycleError::Stale);
    }
    Ok(())
}

impl crate::Store {
    /// Durably consumes the one spawn attempt before process construction.
    pub fn arm_runtime_spawn(
        &self,
        session: &CoordinatorSession,
        fence: &DeploymentFence,
        binding_id: &str,
        incarnation: &str,
    ) -> Result<(), LifecycleError> {
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        fenced(&transaction, session, fence)?;
        let changed = transaction.execute(
            "UPDATE runtime_bindings SET state='uncertain'
             WHERE id=?1 AND deployment_id=?2 AND revision=?3 AND incarnation=?4 AND state='reserved'",
            params![binding_id, fence.deployment_id, fence.revision, incarnation],
        )?;
        if changed != 1 {
            return Err(LifecycleError::Conflict);
        }
        transaction.commit()?;
        Ok(())
    }

    /// Atomically retains immutable binding metadata and its endpoint accounting lease.
    pub fn reserve_runtime_binding(
        &self,
        session: &CoordinatorSession,
        request: &ReserveBinding,
    ) -> Result<(), LifecycleError> {
        if !valid_text(&request.id)
            || !valid_text(&request.fence.deployment_id)
            || request.fence.revision < 1
            || request.fence.generation < 1
            || !valid_text(&request.incarnation)
            || !valid_text(&request.qualification_id)
            || !matches!(request.ownership.as_str(), "managed" | "attached")
            || request.endpoint_host != "127.0.0.1"
            || request.endpoint_port == 0
            || !valid_text(&request.credential_ref)
            || !valid_text(&request.binding_payload)
        {
            return Err(LifecycleError::Invalid);
        }
        let endpoint = format!("{}:{}", request.endpoint_host, request.endpoint_port);
        let listener = TcpListener::bind((&*request.endpoint_host, request.endpoint_port))
            .map_err(|_| LifecycleError::Conflict)?;
        let dto = serde_json::to_string(&BindingDto {
            version: 1,
            qualification_id: request.qualification_id.clone(),
            endpoint,
            credential_ref: request.credential_ref.clone(),
            payload: request.binding_payload.clone(),
        })
        .map_err(|_| LifecycleError::Invalid)?;
        if dto.len() > MAX_DTO_BYTES {
            return Err(LifecycleError::Invalid);
        }
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        fenced(&transaction, session, &request.fence)?;
        transaction.execute(
            "INSERT INTO runtime_bindings(id,deployment_id,revision,incarnation,ownership,binding_json,identities_json,state)
             VALUES (?1,?2,?3,?4,?5,?6,'[]','reserved')",
            params![request.id, request.fence.deployment_id, request.fence.revision,
                    request.incarnation, request.ownership, dto],
        ).map_err(|error| match error {
            rusqlite::Error::SqliteFailure(_, _) => LifecycleError::Conflict,
            other => LifecycleError::Sql(other),
        })?;
        transaction
            .execute(
                "INSERT INTO endpoint_leases(host,port,binding_id) VALUES (?1,?2,?3)",
                params![request.endpoint_host, request.endpoint_port, request.id],
            )
            .map_err(|error| match error {
                rusqlite::Error::SqliteFailure(_, _) => LifecycleError::Conflict,
                other => LifecycleError::Sql(other),
            })?;
        transaction.commit()?;
        drop(listener);
        Ok(())
    }

    /// Records API membership only; it never establishes complete worker ownership.
    pub fn record_api_identity(
        &self,
        session: &CoordinatorSession,
        fence: &DeploymentFence,
        binding_id: &str,
        identity: &ProcessIdentity,
    ) -> Result<(), LifecycleError> {
        if identity.role != "api"
            || identity.pid == 0
            || identity.boot_id.is_empty()
            || identity.start_ticks == 0
        {
            return Err(LifecycleError::Invalid);
        }
        let dto = [IdentityDto {
            role: identity.role.clone(),
            pid: identity.pid,
            boot_id: identity.boot_id.clone(),
            start_ticks: identity.start_ticks,
        }];
        let json = serde_json::to_string(&dto).map_err(|_| LifecycleError::Invalid)?;
        if json.len() > MAX_DTO_BYTES {
            return Err(LifecycleError::Invalid);
        }
        let transaction = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        fenced(&transaction, session, fence)?;
        let changed = transaction.execute(
            "UPDATE runtime_bindings SET identities_json=?1,state='uncertain'
             WHERE id=?2 AND deployment_id=?3 AND revision=?4 AND state!='released'",
            params![json, binding_id, fence.deployment_id, fence.revision],
        )?;
        if changed != 1 {
            return Err(LifecycleError::Conflict);
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn runtime_binding(
        &self,
        deployment_id: &str,
    ) -> Result<Option<StoredRuntimeBinding>, LifecycleError> {
        let row: Option<(String, i64, String, String, String, String, String)> = self
            .conn
            .query_row(
                "SELECT id,revision,incarnation,ownership,binding_json,identities_json,state
             FROM runtime_bindings WHERE deployment_id=?1 AND state!='released'",
                [deployment_id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .optional()?;
        let Some((id, revision, incarnation, ownership, binding_json, identities_json, state)) =
            row
        else {
            return Ok(None);
        };
        if binding_json.len() > MAX_DTO_BYTES || identities_json.len() > MAX_DTO_BYTES {
            return Err(LifecycleError::Invalid);
        }
        let binding: BindingDto =
            serde_json::from_str(&binding_json).map_err(|_| LifecycleError::Invalid)?;
        if binding.version != 1 {
            return Err(LifecycleError::Invalid);
        }
        let identity_dtos: Vec<IdentityDto> =
            serde_json::from_str(&identities_json).map_err(|_| LifecycleError::Invalid)?;
        let identities = identity_dtos
            .into_iter()
            .map(|identity| ProcessIdentity {
                role: identity.role,
                pid: identity.pid,
                boot_id: identity.boot_id,
                start_ticks: identity.start_ticks,
            })
            .collect();
        Ok(Some(StoredRuntimeBinding {
            id,
            deployment_id: deployment_id.into(),
            revision,
            incarnation,
            qualification_id: binding.qualification_id,
            ownership,
            endpoint: binding.endpoint,
            credential_ref: binding.credential_ref,
            identities,
            state,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AcceptDeployment, Store};
    use mllm_domain::{DeploymentId, LifecycleState, OperationId};

    fn accepted(store: &Store, name: &str) -> String {
        let id = DeploymentId::new();
        store
            .accept_deployment(AcceptDeployment {
                id,
                name: name.into(),
                kind: "model".into(),
                route_model_id: None,
                desired_state: LifecycleState::Stopped,
                schema_version: 1,
                initial_operation_id: OperationId(format!("operation-{name}")),
                idempotency_key: format!("idempotency-{name}"),
            })
            .unwrap();
        id.to_string()
    }

    #[test]
    fn binding_and_endpoint_are_reserved_atomically_under_fences() {
        let store = Store::open_in_memory().unwrap();
        let deployment_a = accepted(&store, "deployment-a");
        let deployment_b = accepted(&store, "deployment-b");
        let session = store.begin_coordinator_session().unwrap();
        let reserve = |deployment: &str, port| ReserveBinding {
            id: format!("binding-{deployment}"),
            fence: DeploymentFence {
                deployment_id: deployment.into(),
                revision: 1,
                generation: 1,
            },
            incarnation: format!("incarnation-{deployment}"),
            qualification_id: "qualified".into(),
            ownership: "managed".into(),
            endpoint_host: "127.0.0.1".into(),
            endpoint_port: port,
            credential_ref: format!("credential-{deployment}"),
            binding_payload: "recipe-reference".into(),
        };
        store
            .reserve_runtime_binding(&session, &reserve(&deployment_a, 31001))
            .unwrap();
        store
            .reserve_runtime_binding(&session, &reserve(&deployment_b, 31002))
            .unwrap();
        let a = store.runtime_binding(&deployment_a).unwrap().unwrap();
        let b = store.runtime_binding(&deployment_b).unwrap().unwrap();
        assert_ne!(a.id, b.id);
        assert_ne!(a.endpoint, b.endpoint);
        assert_ne!(a.credential_ref, b.credential_ref);

        let mut stale = reserve(&deployment_a, 31003);
        stale.fence.revision = 2;
        assert!(store.reserve_runtime_binding(&session, &stale).is_err());
        assert_eq!(store.runtime_binding(&deployment_a).unwrap().unwrap(), a);
    }

    #[test]
    fn incomplete_identity_and_consumed_spawn_attempt_retain_accounting() {
        let store = Store::open_in_memory().unwrap();
        let deployment = accepted(&store, "uncertain-runtime");
        let session = store.begin_coordinator_session().unwrap();
        let fence = DeploymentFence {
            deployment_id: deployment.clone(),
            revision: 1,
            generation: 1,
        };
        store
            .reserve_runtime_binding(
                &session,
                &ReserveBinding {
                    id: "binding-uncertain".into(),
                    fence: fence.clone(),
                    incarnation: "incarnation-uncertain".into(),
                    qualification_id: "qualified".into(),
                    ownership: "managed".into(),
                    endpoint_host: "127.0.0.1".into(),
                    endpoint_port: 31012,
                    credential_ref: "credential-reference".into(),
                    binding_payload: "recipe-reference".into(),
                },
            )
            .unwrap();
        store
            .arm_runtime_spawn(
                &session,
                &fence,
                "binding-uncertain",
                "incarnation-uncertain",
            )
            .unwrap();
        assert!(matches!(
            store.arm_runtime_spawn(
                &session,
                &fence,
                "binding-uncertain",
                "incarnation-uncertain",
            ),
            Err(LifecycleError::Conflict)
        ));
        store
            .record_api_identity(
                &session,
                &fence,
                "binding-uncertain",
                &ProcessIdentity {
                    role: "api".into(),
                    pid: 42,
                    boot_id: "boot".into(),
                    start_ticks: 7,
                },
            )
            .unwrap();
        let retained = store.runtime_binding(&deployment).unwrap().unwrap();
        assert_eq!(retained.state, "uncertain");
        assert_eq!(retained.identities.len(), 1);
        let leases: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM endpoint_leases WHERE binding_id='binding-uncertain'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(leases, 1);

        let _new_session = store.begin_coordinator_session().unwrap();
        assert!(matches!(
            store.record_api_identity(
                &session,
                &fence,
                "binding-uncertain",
                &ProcessIdentity {
                    role: "api".into(),
                    pid: 43,
                    boot_id: "boot".into(),
                    start_ticks: 8,
                },
            ),
            Err(LifecycleError::Stale)
        ));
        let still_retained = store.runtime_binding(&deployment).unwrap().unwrap();
        assert_eq!(still_retained.endpoint, "127.0.0.1:31012");
        assert_eq!(still_retained.state, "uncertain");
    }
}
