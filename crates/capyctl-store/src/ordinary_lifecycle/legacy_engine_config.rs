//! Schema v19: carry pre-E1 state into the ADR 0014 shape.
//!
//! Owner decision (2026-09-22): an upgrade migrates old state forward instead of
//! refusing it. Before E1 an effective revision carried engine tuning in the host
//! profile's `launch_settings`; WE1 reads such a revision as corrupt. This step
//! rewrites every one of them into the shape WE1 resolution produces
//! (`capyctl_config::effective::migrate_legacy_effective`), together with every
//! stored record that embeds or digests it, in the migration's transaction:
//!
//! - the frozen revision and its fingerprint (`effective_revisions`);
//! - the retained deployment source, which gains the mapped `engine_config`, so
//!   a remote host resolving it later agrees with the frozen revision;
//! - lifecycle plans, receipts, associations and evidence that embed the frozen
//!   revision's exact bytes (a byte-exact substitution of the JSON string, so no
//!   other byte of those records changes);
//! - the configuration acceptance receipt and start receipts, whose digests bind
//!   the revision and are recomputed exactly as their validators check them.
//!
//! Bindings, reservations, owners, grants and leases are not touched. The legacy
//! recipe fingerprint is recorded so a binding keeps the identity it was created
//! with (SPEC §13.2: a running engine must not become foreign), and the original
//! bytes are retained verbatim.
//!
//! A revision whose settings the E1 model cannot express is left exactly as it
//! was and recorded as refused, with a diagnostic the status snapshot shows and a
//! journal entry. It keeps everything it owns: nothing is released on a refusal
//! (AGENTS.md: uncertainty retains accounting).
//!
//! Stored host publications lose their retired `launch_settings`; a host's own
//! YAML is configuration and stays refused at parse with a pointer.

use super::*;
use capyctl_config::effective::{
    is_legacy_effective, migrate_legacy_effective, strip_legacy_launch_settings,
};
use serde_json::Value;

/// Records that may embed a frozen revision's exact JSON text.
const CARRIERS: &[(&str, &str)] = &[
    ("lifecycle_steps", "step_json"),
    ("lifecycle_runs", "plan_json"),
    ("command_receipts", "response_json"),
    ("owned_launch_associations", "association_json"),
    ("lifecycle_evidence", "evidence_json"),
];

const MAX_EFFECTIVE_BYTES: usize = 1 << 20;

/// The text a JSON encoder writes for `text` inside a string literal.
fn escaped(text: &str) -> String {
    let quoted = serde_json::to_string(text).expect("a string always encodes");
    quoted[1..quoted.len() - 1].to_owned()
}

/// Substitute the revision's bytes wherever a record embeds them as a string,
/// directly or inside another embedded record.
fn substitute(tx: &Transaction<'_>, old: &str, new: &str) -> Result<(), rusqlite::Error> {
    let (mut from, mut to) = (old.to_owned(), new.to_owned());
    for _ in 0..2 {
        from = escaped(&from);
        to = escaped(&to);
        for (table, column) in CARRIERS {
            tx.execute(
                &format!(
                    "UPDATE {table} SET {column}=replace({column},?1,?2) WHERE instr({column},?1)>0"
                ),
                params![from, to],
            )?;
        }
    }
    Ok(())
}

struct Carried {
    fingerprint: String,
    legacy_fingerprint: String,
    legacy_command_fingerprint: Option<String>,
}

/// Rewrite one revision. `Ok(Err(diagnostic))` means it has no mapping; the
/// caller rolls back whatever this wrote.
fn carry(
    tx: &Transaction<'_>,
    deployment: &str,
    revision: i64,
    text: &str,
    source: Option<&str>,
) -> Result<Result<Carried, String>, rusqlite::Error> {
    let migrated = match migrate_legacy_effective(text) {
        Ok(Some(migrated)) => migrated,
        Ok(None) => return Ok(Err("the revision is not in the pre-E1 shape".into())),
        Err(refusal) => return Ok(Err(refusal.to_string())),
    };
    if migrated.effective_json.len() > MAX_EFFECTIVE_BYTES {
        return Ok(Err("the migrated revision exceeds 1MiB".into()));
    }
    tx.execute(
        "UPDATE effective_revisions SET effective_json=?3,fingerprint=?4 WHERE deployment_id=?1 AND revision=?2",
        params![deployment, revision, migrated.effective_json, migrated.effective.recipe_fingerprint],
    )?;
    let (legacy_source, migrated_source) = match source {
        None => (None, None),
        Some(source) => {
            let Ok(legacy) = serde_json::from_str::<Value>(source) else {
                return Ok(Err("the retained deployment source is not JSON".into()));
            };
            let mut carried = legacy.clone();
            let Some(object) = carried.as_object_mut() else {
                return Ok(Err("the retained deployment source is not an object".into()));
            };
            if !object.contains_key("engine_config") {
                object.insert("engine_config".into(), migrated.engine_config.clone());
            }
            if let Err(error) = capyctl_config::parse_strict(
                capyctl_config::ConfigKind::Deployment,
                &carried.to_string(),
            ) {
                return Ok(Err(format!(
                    "the retained deployment source does not parse once migrated: {error}"
                )));
            }
            tx.execute(
                "UPDATE managed_configuration_sources SET config_json=?3 WHERE deployment_id=?1 AND revision=?2",
                params![deployment, revision, carried.to_string()],
            )?;
            (Some(legacy), Some(carried))
        }
    };
    substitute(tx, text, &migrated.effective_json)?;
    let legacy_command_fingerprint = match crate::managed_configuration::reseal_migrated_receipt(
        tx,
        deployment,
        revision,
        &migrated.effective_json,
        migrated_source.as_ref(),
        legacy_source.as_ref(),
    ) {
        Ok(fingerprint) => fingerprint,
        Err(crate::managed_configuration::ManagedConfigurationError::Sql(error)) => {
            return Err(error)
        }
        Err(error) => {
            return Ok(Err(format!(
                "its configuration acceptance receipt cannot be re-sealed: {error}"
            )))
        }
    };
    match super::receipt::reseal_start_receipts(tx, deployment) {
        Ok(()) => {}
        Err(LifecycleError::Sql(error)) => return Err(error),
        Err(error) => {
            return Ok(Err(format!(
                "its start receipts cannot be re-sealed: {error}"
            )))
        }
    }
    Ok(Ok(Carried {
        fingerprint: migrated.effective.recipe_fingerprint,
        legacy_fingerprint: migrated.legacy_fingerprint,
        legacy_command_fingerprint,
    }))
}

fn journal(
    tx: &Transaction<'_>,
    host: Option<&str>,
    state: &str,
    evidence: &str,
) -> Result<(), rusqlite::Error> {
    tx.execute(
        "INSERT INTO journal_entries(id,host_id,operation_id,state,evidence) VALUES(?1,?2,NULL,?3,?4)",
        params![ulid::Ulid::new().to_string(), host, state, evidence],
    )?;
    Ok(())
}

/// The v19 data step. Runs inside the migration's transaction.
pub(crate) fn migrate(tx: &Transaction<'_>) -> Result<(), rusqlite::Error> {
    let revisions = tx
        .prepare("SELECT deployment_id,revision,effective_json,fingerprint FROM effective_revisions ORDER BY deployment_id,revision")?
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (deployment, revision, text, stored_fingerprint) in revisions {
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if !is_legacy_effective(&value) {
            continue;
        }
        let source: Option<String> = tx
            .query_row(
                "SELECT config_json FROM managed_configuration_sources WHERE deployment_id=?1 AND revision=?2",
                params![deployment, revision],
                |r| r.get(0),
            )
            .optional()?;
        tx.execute_batch("SAVEPOINT legacy_engine_config")?;
        match carry(tx, &deployment, revision, &text, source.as_deref())? {
            Ok(carried) => {
                tx.execute_batch("RELEASE legacy_engine_config")?;
                tx.execute(
                    "INSERT INTO engine_config_migrations VALUES(?1,?2,'migrated',?3,?4,?5,?6,?7,?8)",
                    params![
                        deployment,
                        revision,
                        carried.legacy_fingerprint,
                        carried.fingerprint,
                        carried.legacy_command_fingerprint,
                        text,
                        source,
                        "pre-E1 launch_settings carried into the deployment's engine_config; \
                         bindings keep the identity recorded before the upgrade",
                    ],
                )?;
            }
            Err(diagnostic) => {
                tx.execute_batch("ROLLBACK TO legacy_engine_config; RELEASE legacy_engine_config")?;
                let legacy_fingerprint = value["recipe_fingerprint"]
                    .as_str()
                    .unwrap_or(&stored_fingerprint)
                    .to_owned();
                tx.execute(
                    "INSERT INTO engine_config_migrations VALUES(?1,?2,'refused',?3,NULL,NULL,?4,?5,?6)",
                    params![deployment, revision, legacy_fingerprint, text, source, diagnostic],
                )?;
                journal(
                    tx,
                    None,
                    "engine_config_migration_refused",
                    &format!(
                        "deployment {deployment} revision {revision}: its pre-E1 engine \
                         settings could not be carried forward: {diagnostic}. Its bindings, \
                         reservations and owners are retained; operator action: stop the \
                         deployment and replace its configuration with an engine_config \
                         (ADR 0014 §1)"
                    ),
                )?;
            }
        }
    }
    let publications = tx
        .prepare("SELECT host_id,config_json,fingerprint FROM approved_host_publications ORDER BY host_id")?
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (host, text, legacy_fingerprint) in publications {
        let Some((stripped, _)) = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|document| strip_legacy_launch_settings(&document))
        else {
            continue;
        };
        match capyctl_config::remote_roles::HostConfig::parse(&stripped.to_string()) {
            Ok(config) => {
                let fingerprint =
                    capyctl_config::remote_resources::policy_fingerprint(&config.document);
                tx.execute(
                    "UPDATE approved_host_publications SET config_json=?2,fingerprint=?3 WHERE host_id=?1",
                    params![host, config.document.to_string(), fingerprint],
                )?;
                tx.execute(
                    "INSERT INTO host_publication_migrations VALUES(?1,?2,?3,?4)",
                    params![host, legacy_fingerprint, fingerprint, text],
                )?;
            }
            Err(error) => journal(
                tx,
                Some(&host),
                "host_publication_migration_refused",
                &format!(
                    "host {host}: its stored publication still names launch_settings and does \
                     not parse without them ({error}); it stays refused until the host \
                     republishes an edited document"
                ),
            )?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
