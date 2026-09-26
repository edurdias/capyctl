//! The start-time migration of standalone's generated resource policy.
//!
//! ADR 0019 (upgrade of a generated policy): standalone generates its host's
//! policy from the machine it observes. When this start observes a different
//! shape than the stored policy describes (0.1.0-rc.4 recorded a discrete-GPU
//! machine as one `unified` domain), the stored policy is replaced here,
//! before it is published:
//!
//! 1. every deployment that still holds memory under the previous policy is
//!    stopped by the ordinary Stop, whose verified cleanup is the only thing
//!    that releases its charge (SPEC §7, §13.2: nothing is released on the
//!    observation alone). A Stop that cannot prove its engine gone keeps the
//!    reservation, and the start is refused naming the deployment;
//! 2. the policy is replaced in one store transaction
//!    ([`mllm_store::Store::migrate_generated_resource_policy`]);
//! 3. each deployment resolved against the previous policy is accepted again
//!    from its stored document as a new revision for this machine, so the next
//!    request starts it on the new shape. One that does not resolve here is
//!    named in the notice with what to do.
//!
//! The migration notice is printed once: the next start finds the policy
//! current. Hand-written host policies never reach this module.

use std::time::{Duration, Instant};

use mllm_config::effective::HostPolicy;
use mllm_controller::coordinator::{CoordinatorCommands, ServiceObservation};
use mllm_management::configuration::{ConfigurationCommand, ConfigurationSource};
use mllm_store::resource_policy::GeneratedPolicyMigration;

/// How often the migration re-reads the ledger while the Stops run.
const POLL: Duration = Duration::from_millis(200);

/// The principal the migration's commands are recorded under.
const PRINCIPAL: &str = "standalone";

fn failed(message: String) -> crate::roles::StartError {
    crate::roles::StartError::Deploy(message)
}

/// Migrate the embedded host's generated policy to `host` when the machine
/// changed shape, then re-size the deployments resolved for another shape.
/// Returns the notices to print at start (none when nothing changed).
///
/// `stop_bound` bounds the whole wait for the Stops' verified cleanup.
pub async fn migrate(
    commands: &CoordinatorCommands,
    configuration: &dyn ConfigurationSource,
    observation: &dyn ServiceObservation,
    host: &HostPolicy,
    stop_bound: Duration,
) -> Result<Vec<String>, crate::roles::StartError> {
    let deadline = Instant::now() + stop_bound;
    let mut stopping: Vec<(String, String)> = Vec::new();
    let mut refusals: std::collections::BTreeMap<String, String> = Default::default();
    let mut notices = Vec::new();
    loop {
        let observations = observation
            .observe(host.name.clone())
            .await
            .map_err(|error| failed(error.to_string()))?;
        let now = commands
            .now_ms()
            .map_err(|error| failed(error.to_string()))?;
        let outcome = {
            let owner = commands
                .owner_for_read()
                .map_err(|error| failed(error.to_string()))?;
            owner
                .store()
                .migrate_generated_resource_policy(owner.session(), host, &observations, now)
                .map_err(|error| failed(format!("resource policy migration: {error}")))?
        };
        match outcome {
            GeneratedPolicyMigration::NotNeeded => break,
            GeneratedPolicyMigration::Migrated {
                previous_domains,
                current_domains,
                revision,
                ..
            } => {
                let mut notice = if previous_domains == current_domains {
                    format!(
                        "the resource policy mllm generated for this machine was replaced \
                         (revision {revision}): its engine port range or device map changed"
                    )
                } else {
                    format!(
                        "this machine's memory shape changed, so the resource policy mllm \
                         generated for it was replaced (revision {revision}): domains [{}] are \
                         now [{}]",
                        previous_domains.join(", "),
                        current_domains.join(", ")
                    )
                };
                if !stopping.is_empty() {
                    let names: Vec<&str> = stopping.iter().map(|(_, name)| name.as_str()).collect();
                    notice.push_str(&format!(
                        "; stopped with verified cleanup first: {}",
                        names.join(", ")
                    ));
                }
                notices.push(notice);
                break;
            }
            GeneratedPolicyMigration::ChargesRemain {
                previous_domains,
                current_domains,
                charges,
            } => {
                for charge in &charges {
                    if stopping.iter().any(|(id, _)| *id == charge.deployment_id) {
                        continue;
                    }
                    let (name, revision) = commands
                        .read(|store| {
                            Ok((
                                store
                                    .get_deployment(&charge.deployment_id)?
                                    .map(|d| d.name)
                                    .unwrap_or_else(|| charge.deployment_id.clone()),
                                store.current_revision(&charge.deployment_id)?,
                            ))
                        })
                        .map_err(|error| failed(error.to_string()))?;
                    let Some(revision) = revision else {
                        continue;
                    };
                    // SPEC §6.3: an ordinary Stop, so the deployment stays
                    // eligible for on-demand activation on the new shape.
                    let key = format!(
                        "policy-migration-stop-{}-{revision}-{now}",
                        charge.deployment_id
                    );
                    let stop_deadline = now
                        .saturating_add(i64::try_from(stop_bound.as_millis()).unwrap_or(i64::MAX));
                    match commands.stop(
                        PRINCIPAL,
                        &charge.deployment_id,
                        revision,
                        &key,
                        stop_deadline,
                    ) {
                        Ok(_) => {}
                        // SPEC §4.3: the restarted coordinator adopts the
                        // launches its previous session left before a Stop
                        // can reach them; until then the Stop is refused and
                        // is asked again (bounded by `stop_bound`).
                        Err(error) => {
                            refusals.insert(name.clone(), error.to_string());
                            continue;
                        }
                    }
                    refusals.remove(&name);
                    stopping.push((charge.deployment_id.clone(), name));
                }
                if Instant::now() >= deadline {
                    let names: Vec<String> = charges
                        .iter()
                        .map(|charge| {
                            let name = stopping
                                .iter()
                                .find(|(id, _)| *id == charge.deployment_id)
                                .map_or(charge.deployment_id.clone(), |(_, name)| name.clone());
                            match refusals.get(&name) {
                                Some(refusal) => {
                                    format!("{name} (its Stop was refused: {refusal})")
                                }
                                None => name,
                            }
                        })
                        .collect();
                    return Err(failed(format!(
                        "this machine's memory shape changed (domains [{}] are now [{}]); the \
                         engines of {} could not be proven stopped within {} s, so their memory \
                         stays reserved under the previous resource policy and nothing was \
                         released. Start again to retry",
                        previous_domains.join(", "),
                        current_domains.join(", "),
                        names.join(", "),
                        stop_bound.as_secs()
                    )));
                }
                tokio::time::sleep(POLL).await;
            }
        }
    }
    notices.extend(resize(commands, configuration, &host.name)?);
    Ok(notices)
}

/// Accept every deployment resolved against another policy context again,
/// from its stored document, as a new revision for the current one.
fn resize(
    commands: &CoordinatorCommands,
    configuration: &dyn ConfigurationSource,
    host: &str,
) -> Result<Vec<String>, crate::roles::StartError> {
    let stale = {
        let owner = commands
            .owner_for_read()
            .map_err(|error| failed(error.to_string()))?;
        owner
            .store()
            .deployments_resolved_elsewhere(host)
            .map_err(|error| failed(format!("resource policy migration: {error}")))?
    };
    let (mut resized, mut refused) = (Vec::new(), Vec::new());
    for deployment in stale {
        let key = format!(
            "policy-migration-resize-{}-{}",
            deployment.deployment_id, deployment.revision
        );
        match configuration.accept(
            &key,
            ConfigurationCommand::Replace {
                deployment_id: deployment.deployment_id.clone(),
                expected_revision: deployment.revision,
                config_json: deployment.config.to_string(),
            },
        ) {
            Ok(_) => resized.push(deployment.name),
            Err(failure) => refused.push(format!("{} ({failure:?})", deployment.name)),
        }
    }
    let mut notices = Vec::new();
    if !resized.is_empty() {
        notices.push(format!(
            "re-sized for this machine's resource policy: {}",
            resized.join(", ")
        ));
    }
    if !refused.is_empty() {
        notices.push(format!(
            "these deployments were sized for another memory shape and do not resolve on this \
             machine as written, so they cannot start; deploy each again with a file for this \
             machine (`mllm deploy --file`): {}",
            refused.join(", ")
        ));
    }
    Ok(notices)
}
