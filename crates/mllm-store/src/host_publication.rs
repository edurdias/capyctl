//! SPEC §§4.2,13: authenticated host preparation is durable but not qualification.
use crate::{Store, StoreError};
use rusqlite::{params, OptionalExtension, TransactionBehavior};
/// What a host's agent journal says about its launch claims (SPEC §§3.1,
/// 7.3; ADR 0013 §4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchClaims {
    /// One claim per launch; commands fenced per deployment (journal v4).
    PerLaunch,
    /// One claim per launch; commands fenced per deployment instance
    /// (journal v5), so two instances of one deployment may share the host.
    PerInstance,
}
#[derive(Clone)]
pub struct HostPublication {
    pub host_id: String,
    pub config_json: String,
    pub boot_id: String,
    pub fingerprint: String,
    pub received_at_ms: i64,
}
impl Store {
    /// Caller authenticates transport; this transaction independently checks the
    /// retained enrolled identity/revocation before replacing its publication.
    pub fn publish_host_configuration(
        &self,
        publication: &HostPublication,
    ) -> Result<(), StoreError> {
        self.publish_host_configuration_with_claims(publication, false)
    }
    /// As `publish_host_configuration`, recording in the same transaction
    /// whether the host's agent keeps one journal claim per launch (SPEC
    /// §§3.1, 7.3). Every publication replaces the previous answer, so a host
    /// that stops advertising it (an older agent) is single-claim again.
    pub fn publish_host_configuration_with_claims(
        &self,
        publication: &HostPublication,
        per_launch_claims: bool,
    ) -> Result<(), StoreError> {
        self.publish_host_configuration_with_launch_claims(
            publication,
            per_launch_claims.then_some(LaunchClaims::PerLaunch),
        )
    }
    /// As `publish_host_configuration_with_claims`, recording which kind of
    /// per-launch journal the host advertised, if any (ADR 0013 §4).
    pub fn publish_host_configuration_with_launch_claims(
        &self,
        publication: &HostPublication,
        claims: Option<LaunchClaims>,
    ) -> Result<(), StoreError> {
        if publication.config_json.len() > 32768
            || publication.received_at_ms < 0
            || publication.boot_id.is_empty()
            || publication.boot_id.len() > 128
            || !publication
                .boot_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        {
            return Err(StoreError::Conflict);
        }
        let config = mllm_config::remote_roles::HostConfig::parse(&publication.config_json)
            .map_err(|_| StoreError::Conflict)?;
        if mllm_config::remote_resources::policy_fingerprint(&config.document)
            != publication.fingerprint
        {
            return Err(StoreError::Conflict);
        }
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let enrolled: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM enrolled_hosts WHERE host_id=?1 AND revoked=0)",
            [&publication.host_id],
            |r| r.get(0),
        )?;
        if !enrolled {
            return Err(StoreError::Conflict);
        }
        tx.execute("INSERT INTO approved_host_publications VALUES(?1,?2,?3,?4,?5) ON CONFLICT(host_id) DO UPDATE SET config_json=excluded.config_json,boot_id=excluded.boot_id,fingerprint=excluded.fingerprint,received_at_ms=excluded.received_at_ms",params![publication.host_id,config.document.to_string(),publication.boot_id,publication.fingerprint,publication.received_at_ms])?;
        if let Some(claims) = claims {
            let mode = match claims {
                LaunchClaims::PerLaunch => "per_launch",
                LaunchClaims::PerInstance => "per_instance",
            };
            tx.execute(
                "INSERT INTO host_launch_claims(host_id,mode,recorded_at_ms) VALUES(?1,?3,?2)
                 ON CONFLICT(host_id) DO UPDATE SET mode=excluded.mode,recorded_at_ms=excluded.recorded_at_ms",
                params![publication.host_id, publication.received_at_ms, mode],
            )?;
        } else {
            tx.execute(
                "DELETE FROM host_launch_claims WHERE host_id=?1",
                [&publication.host_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    /// SPEC §§3.1, 7.3: whether the host's latest publication advertised
    /// per-launch journal claims (per-instance fencing implies them).
    pub fn host_has_per_launch_claims(&self, host_id: &str) -> Result<bool, StoreError> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM host_launch_claims WHERE host_id=?1 AND mode IN ('per_launch','per_instance'))",
            [host_id],
            |r| r.get(0),
        )?)
    }
    /// ADR 0013 §4, §5: whether the host's latest publication advertised a
    /// journal that fences commands per deployment instance.
    pub fn host_has_per_instance_fencing(&self, host_id: &str) -> Result<bool, StoreError> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM host_launch_claims WHERE host_id=?1 AND mode='per_instance')",
            [host_id],
            |r| r.get(0),
        )?)
    }
    pub fn host_publication(&self, selector: &str) -> Result<Option<HostPublication>, StoreError> {
        let mut query=self.conn.prepare("SELECT p.host_id,p.config_json,p.boot_id,p.fingerprint,p.received_at_ms FROM approved_host_publications p JOIN enrolled_hosts h USING(host_id) WHERE (h.host_id=?1 OR h.host_name=?1) AND h.revoked=0 LIMIT 2")?;
        let mut rows = query.query_map([selector], |r| {
            Ok(HostPublication {
                host_id: r.get(0)?,
                config_json: r.get(1)?,
                boot_id: r.get(2)?,
                fingerprint: r.get(3)?,
                received_at_ms: r.get(4)?,
            })
        })?;
        let result = rows.next().transpose()?;
        if rows.next().is_some() {
            return Err(StoreError::Conflict);
        }
        if let Some(publication) = &result {
            let parsed = mllm_config::remote_roles::HostConfig::parse(&publication.config_json)
                .map_err(|_| StoreError::Conflict)?;
            if mllm_config::remote_resources::policy_fingerprint(&parsed.document)
                != publication.fingerprint
            {
                return Err(StoreError::Conflict);
            }
        }
        Ok(result)
    }
    pub fn managed_configuration_source(
        &self,
        deployment: &str,
        revision: i64,
    ) -> Result<Option<serde_json::Value>, StoreError> {
        let source:Option<String>=self.conn.query_row("SELECT config_json FROM managed_configuration_sources WHERE deployment_id=?1 AND revision=?2",params![deployment,revision],|r|r.get(0)).optional()?;
        source
            .map(|s| {
                mllm_config::parse_strict(mllm_config::ConfigKind::Deployment, &s)
                    .map_err(|_| StoreError::Conflict)
            })
            .transpose()
    }

    /// ADR 0013 §3: the deployment document as scoped to `host` for one
    /// revision, which a launch placed on that host is rendered from. A
    /// revision recorded before per-host sources existed has only its
    /// canonical host's, the accepted source.
    pub fn host_configuration_source(
        &self,
        deployment: &str,
        revision: i64,
        host: &str,
    ) -> Result<Option<serde_json::Value>, StoreError> {
        let source: Option<String> = self
            .conn
            .query_row(
                "SELECT source_json FROM host_effective_revisions WHERE deployment_id=?1 AND revision=?2 AND host_id=?3 AND outcome='resolved' AND source_json IS NOT NULL",
                params![deployment, revision, host],
                |r| r.get(0),
            )
            .optional()?;
        match source {
            Some(source) => mllm_config::parse_strict(mllm_config::ConfigKind::Deployment, &source)
                .map(Some)
                .map_err(|_| StoreError::Conflict),
            None => self.managed_configuration_source(deployment, revision),
        }
    }
}

impl Store {
    // SPEC §§6,15: freeze the peer ingress with the binding; mutable discovery
    // cannot redirect an already admitted runtime or reuse its gate credential.
    pub fn bind_remote_ingress(
        &self,
        binding_id: &str,
        host_id: &str,
        endpoint: &str,
    ) -> Result<(), StoreError> {
        mllm_config::remote_roles::private_ingress_endpoint(endpoint)
            .map_err(|_| StoreError::Conflict)?;
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        // ADR 0013 §4: the binding's host is the one its instance was placed on,
        // else (a launch placed before instances existed) the revision's.
        let expected: Option<String> = tx.query_row("SELECT COALESCE(i.host_id,json_extract(e.effective_json,'$.host.name')) FROM runtime_bindings b JOIN effective_revisions e ON e.deployment_id=b.deployment_id AND e.revision=b.revision LEFT JOIN deployment_instances i ON i.deployment_id=b.deployment_id AND i.instance_index=b.instance_index JOIN enrolled_hosts h ON h.host_id=COALESCE(i.host_id,json_extract(e.effective_json,'$.host.name')) WHERE b.id=?1 AND h.revoked=0 AND b.state!='released'", [binding_id], |r|r.get(0)).optional()?;
        if expected.as_deref() != Some(host_id) {
            return Err(StoreError::Conflict);
        }
        let existing: Option<(String, String)> = tx
            .query_row(
                "SELECT host_id,endpoint FROM remote_binding_ingress WHERE binding_id=?1",
                [binding_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((host, address)) = existing {
            if host != host_id || address != endpoint {
                return Err(StoreError::Conflict);
            }
        } else {
            tx.execute(
                "INSERT INTO remote_binding_ingress VALUES(?1,?2,?3)",
                params![binding_id, host_id, endpoint],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn remote_ingress_endpoint(&self, binding_id: &str) -> Result<Option<String>, StoreError> {
        let row: Option<(String,Option<String>,Option<String>)> = self.conn.query_row("SELECT COALESCE(i.host_id,json_extract(e.effective_json,'$.host.name')),r.host_id,r.endpoint FROM runtime_bindings b JOIN effective_revisions e ON e.deployment_id=b.deployment_id AND e.revision=b.revision LEFT JOIN deployment_instances i ON i.deployment_id=b.deployment_id AND i.instance_index=b.instance_index LEFT JOIN remote_binding_ingress r ON r.binding_id=b.id WHERE b.id=?1",[binding_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let Some((expected, host, endpoint)) = row else {
            return Err(StoreError::Conflict);
        };
        let revoked: Option<bool> = self
            .conn
            .query_row(
                "SELECT revoked FROM enrolled_hosts WHERE host_id=?1",
                [&expected],
                |r| r.get(0),
            )
            .optional()?;
        if revoked == Some(true) {
            return Err(StoreError::Conflict);
        }
        let enrolled = revoked.is_some();
        match (host, endpoint) {
            (Some(host), Some(endpoint)) if enrolled && host == expected => {
                mllm_config::remote_roles::private_ingress_endpoint(&endpoint)
                    .map_err(|_| StoreError::Conflict)?;
                Ok(Some(endpoint))
            }
            (None, None) if !enrolled => Ok(None),
            _ => Err(StoreError::Conflict),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // T06, T16, T33: ingress credentials remain bound to a single retained host
    // and runtime, including when the mutable host publication disappears.
    #[test]
    fn frozen_ingress_cannot_move_and_missing_remote_endpoint_fails_closed() {
        let store = Store::open_in_memory().unwrap();
        store.conn.execute_batch("INSERT INTO enrolled_hosts VALUES('remote','spark','key',0);
            INSERT INTO deployments(id,name,kind,desired_state,admission_enabled,suspended,current_generation,schema_version) VALUES('deployment','model','managed','stopped',0,0,1,1);
            INSERT INTO effective_revisions VALUES('deployment',1,'{\"host\":{\"name\":\"remote\"}}','fingerprint');
            INSERT INTO runtime_bindings(id,deployment_id,revision,incarnation,ownership,binding_json,identities_json,state) VALUES('binding','deployment',1,'incarnation','managed','{}','[]','reserved');").unwrap();
        assert!(store.remote_ingress_endpoint("binding").is_err());
        assert!(store
            .bind_remote_ingress("binding", "remote", "http://8.8.8.8:9443")
            .is_err());
        assert!(store
            .bind_remote_ingress("binding", "other", "http://100.64.0.10:9443")
            .is_err());
        let address = "http://100.64.0.10:9443";
        store
            .bind_remote_ingress("binding", "remote", address)
            .unwrap();
        store
            .bind_remote_ingress("binding", "remote", address)
            .unwrap();
        assert!(store
            .bind_remote_ingress("binding", "remote", "http://100.64.0.11:9443")
            .is_err());
        assert_eq!(
            store.remote_ingress_endpoint("binding").unwrap().as_deref(),
            Some(address)
        );
        store.revoke_host("remote").unwrap();
        assert!(store.remote_ingress_endpoint("binding").is_err());
        assert!(store
            .bind_remote_ingress("binding", "remote", address)
            .is_err());
        store
            .conn
            .execute("DELETE FROM remote_binding_ingress", [])
            .unwrap();
        assert!(store.remote_ingress_endpoint("binding").is_err());
    }
}
