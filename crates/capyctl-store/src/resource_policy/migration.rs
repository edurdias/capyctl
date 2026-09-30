//! Migration of a capyctl-generated resource policy whose machine shape changed.
//!
//! ADR 0019 (upgrade of a generated policy): standalone generates its host's
//! resource policy from the machine it observes at boot. A release that reads
//! the machine differently (0.1.0-rc.4 recorded one `unified` domain for a
//! discrete-GPU machine; this release derives `system` and one `gpuN` per
//! card) would otherwise refuse to start with a revision conflict. The
//! generated policy is replaced on the first start that observes the new
//! shape, under the accounting rules every other change follows (SPEC §7,
//! §13.2):
//!
//! - nothing is released on the observation alone: every owner charged under
//!   the previous policy has to be stopped by the ordinary Stop, with verified
//!   cleanup, before the policy is replaced (this module only reports them);
//! - the replacement is one transaction that also re-registers the host's
//!   ledger keys and advances the ledger epoch;
//! - a hand-written policy is never replaced: the remote publication path
//!   refuses a changed shape with [`ResourcePolicyError::ShapeChanged`].
//!
//! Every frozen revision names the policy context it was resolved against
//! (`ordinary_lifecycle::policy_checked`), so no charged owner can be kept
//! across a context change: its next Stop or park would no longer find its
//! policy. Deployments are instead re-resolved against the new policy from
//! their stored source ([`crate::Store::deployments_resolved_elsewhere`]).
use super::*;

/// One owner the ledger charges under the previous policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviousPolicyCharge {
    pub owner_id: String,
    pub deployment_id: String,
    /// The previous policy's domains this owner holds memory on.
    pub domains: Vec<String>,
}

/// What a generated policy's migration found or did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GeneratedPolicyMigration {
    /// No stored policy, or the stored one already has this shape.
    NotNeeded,
    /// The stored policy has another shape, and these owners still hold
    /// memory under it. Nothing was changed; each must be stopped with
    /// verified cleanup first.
    ChargesRemain {
        previous_domains: Vec<String>,
        current_domains: Vec<String>,
        charges: Vec<PreviousPolicyCharge>,
    },
    /// The stored policy was replaced by the one generated for this machine.
    Migrated {
        previous_domains: Vec<String>,
        current_domains: Vec<String>,
        previous_revision: i64,
        revision: i64,
        epoch: u64,
    },
}

/// A deployment whose current revision was resolved against a policy context
/// other than the host's current one, with the source it was accepted from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedElsewhere {
    pub deployment_id: String,
    pub name: String,
    pub revision: i64,
    /// The stored deployment document (its instance count restored), to be
    /// accepted again as a new revision against the current host; `None` for
    /// a revision that has no stored document (accepted before documents
    /// were kept), which cannot be re-sized and is named to the operator.
    pub config: Option<serde_json::Value>,
}

/// A deployment row read for [`crate::Store::deployments_resolved_elsewhere`]:
/// id, name, revision, frozen effective JSON, stored source, instance count.
type StaleRow = (String, String, i64, String, Option<String>, Option<i64>);

/// The owners charged on any of `domains`, with their deployments.
fn charges_on(
    tx: &Transaction<'_>,
    snapshot: &LedgerSnapshot,
    domains: &BTreeSet<String>,
) -> Result<Vec<PreviousPolicyCharge>, ResourcePolicyError> {
    let mut charges = Vec::new();
    for (owner, footprint) in &snapshot.owners {
        let held: Vec<String> = footprint
            .allocations
            .iter()
            .filter(|allocation| domains.contains(&allocation.domain))
            .map(|allocation| allocation.domain.clone())
            .collect();
        if held.is_empty() {
            continue;
        }
        let deployment_id: String = tx
            .query_row(
                "SELECT deployment_id FROM resource_owners WHERE owner_id=?1",
                [owner],
                |r| r.get(0),
            )
            .optional()?
            .ok_or(ResourcePolicyError::CorruptStoredPolicy)?;
        charges.push(PreviousPolicyCharge {
            owner_id: owner.clone(),
            deployment_id,
            domains: held,
        });
    }
    Ok(charges)
}

impl crate::Store {
    /// ADR 0019: replace the embedded host's generated policy when the machine
    /// it describes changed shape. Callers pass only a policy capyctl generated
    /// (standalone's); a hand-written policy is never handed here.
    ///
    /// Uncertainty retains accounting: while any owner holds memory under the
    /// previous policy nothing changes and the owners are returned
    /// ([`GeneratedPolicyMigration::ChargesRemain`]); the caller stops them
    /// through the ordinary Stop (verified cleanup releases them) and calls
    /// again.
    pub fn migrate_generated_resource_policy(
        &self,
        session: &CoordinatorSession,
        host: &HostPolicy,
        observations: &[MemoryObservation],
        now_ms: i64,
    ) -> Result<GeneratedPolicyMigration, ResourcePolicyError> {
        let context = ResourceContext::from_host(host);
        let controls = ResourceControls::from_host(host);
        if !valid_id(&context.host_id) {
            return Err(ResourcePolicyError::Invalid);
        }
        controls
            .validate(&context)
            .map_err(|_| ResourcePolicyError::Invalid)?;
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        check_session(&tx, session).map_err(map_session)?;
        crate::resource_namespace::ensure_resolved(&tx)?;
        // Only the embedded host's policy is generated; its namespace must
        // already name this host (a renamed host stays a conflict).
        let embedded: Option<String> = tx
            .query_row(
                "SELECT host_id FROM host_resource_namespaces WHERE kind='embedded' AND policy_key=?1",
                [&context.host_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(namespace) = embedded else {
            return Ok(GeneratedPolicyMigration::NotNeeded);
        };
        let Some(current) = read_policy(&tx, &context.host_id)? else {
            return Ok(GeneratedPolicyMigration::NotNeeded);
        };
        if current.context == context {
            return Ok(GeneratedPolicyMigration::NotNeeded);
        }
        let previous_domains: Vec<String> = current.context.domain_ids.iter().cloned().collect();
        let current_domains: Vec<String> = context.domain_ids.iter().cloned().collect();
        let snapshot = read_snapshot(&tx).map_err(map_ledger)?;
        let charges = charges_on(&tx, &snapshot, &current.context.domain_ids)?;
        if !charges.is_empty() {
            return Ok(GeneratedPolicyMigration::ChargesRemain {
                previous_domains,
                current_domains,
                charges,
            });
        }
        // The new policy is justified by its own observations, like a bootstrap.
        validate_observations(
            &context,
            &controls,
            observations,
            now_ms,
            controls.observation_ttl_ms,
        )?;
        let revision = current
            .revision
            .checked_add(1)
            .filter(|v| *v > 0)
            .ok_or(ResourcePolicyError::Invalid)?;
        let stored = StoredPolicy {
            version: 1,
            host_id: context.host_id.clone(),
            revision,
            context: StoredContext::from_public(&context),
            controls: StoredControls::from_public(&controls),
        };
        tx.execute(
            "UPDATE host_resource_policies SET revision=?2,policy_json=?3 WHERE host_id=?1",
            params![context.host_id, revision, encode_policy(&stored)?],
        )?;
        // SPEC §7: the host-scoped key registry follows the policy. No owner
        // references a removed key (checked above), so it can go.
        tx.execute(
            "DELETE FROM host_resource_keys WHERE host_id=?1",
            [&namespace],
        )?;
        for id in &context.domain_ids {
            tx.execute(
                "INSERT INTO host_resource_keys VALUES(?1,'domain',?2,?2)",
                params![namespace, id],
            )?;
        }
        for id in context.device_domains.keys() {
            tx.execute(
                "INSERT INTO host_resource_keys VALUES(?1,'device',?2,?2)",
                params![namespace, id],
            )?;
        }
        let epoch = next_epoch(&tx)?;
        append_event(
            &tx,
            &EventMetadata::HostResourcePolicyMigrated {
                previous_revision: current.revision,
                current_revision: revision,
                ledger_epoch: epoch,
                session_epoch: session.epoch(),
            },
        )
        .map_err(map_event)?;
        tx.commit()?;
        Ok(GeneratedPolicyMigration::Migrated {
            previous_domains,
            current_domains,
            previous_revision: current.revision,
            revision,
            epoch,
        })
    }

    /// Every deployment that is not deleted and whose current revision was
    /// resolved against a policy context other than `host`'s current one,
    /// oldest first, with the document it was accepted from.
    pub fn deployments_resolved_elsewhere(
        &self,
        host: &str,
    ) -> Result<Vec<ResolvedElsewhere>, ResourcePolicyError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let Some(policy) = read_selected_policy(&tx, host)? else {
            return Ok(Vec::new());
        };
        let rows: Vec<StaleRow> = tx
            .prepare(
                "SELECT d.id,d.name,d.revision,e.effective_json,s.config_json,i.instances
                 FROM deployments d
                 JOIN effective_revisions e ON e.deployment_id=d.id AND e.revision=d.revision
                 LEFT JOIN managed_configuration_sources s ON s.deployment_id=d.id AND s.revision=d.revision
                 LEFT JOIN deployment_revision_instances i ON i.deployment_id=d.id AND i.revision=d.revision
                 WHERE d.kind<>?1 ORDER BY d.created_at,d.id",
            )?
            .query_map([crate::delete::DELETED_KIND], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })?
            .collect::<Result<_, _>>()?;
        tx.commit()?;
        let mut found = Vec::new();
        for (deployment_id, name, revision, effective, source, instances) in rows {
            let effective = capyctl_config::effective::decode_effective_snapshot(&effective)
                .map_err(|_| ResourcePolicyError::CorruptStoredPolicy)?;
            if effective.host.name != policy.context.host_id
                || ResourceContext::from_host(&effective.host) == policy.context
            {
                continue;
            }
            let config = match source {
                None => None,
                Some(source) => {
                    let mut config: serde_json::Value = serde_json::from_str(&source)
                        .map_err(|_| ResourcePolicyError::CorruptStoredPolicy)?;
                    // ADR 0013 §2: the stored source is the per-host recipe,
                    // which drops the deployment-level instance count;
                    // restore it.
                    if let (Some(object), Some(count)) = (config.as_object_mut(), instances) {
                        if count > 1 {
                            object.insert("instances".into(), count.into());
                        }
                    }
                    Some(config)
                }
            };
            found.push(ResolvedElsewhere {
                deployment_id,
                name,
                revision,
                config,
            });
        }
        Ok(found)
    }
}
