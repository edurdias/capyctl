//! `mllm validate config --file <FILE> [--host <HOST>]`.
//!
//! SPEC §14 / §15.3: validate syntax and schema before side effects. The checks
//! are the ones the product runs on the same documents, so a file this command
//! accepts is not refused later for a reason it could have named:
//!
//! - server: `ServerConfig::parse`;
//! - host: `HostConfig::parse`, then the resource policy normalized exactly as
//!   host publication does (`local_host_document` + `normalize_host_policy`);
//! - standalone: the strict standalone schema and its `shutdown.drain_timeout`;
//! - deployment: the strict deployment schema, the instance declaration
//!   (ADR 0013 §2), the declared `timeouts` (ADR 0014 amendment A1) and a
//!   declared startup peak (owner decision 2026-09-23), then,
//!   when `--host` names a host document, the effective resolution the host
//!   agent performs before launch.
//!
//! Nothing is written, created, or contacted: the command reads the named files
//! and reports.

use std::path::Path;

use mllm_config::{parse_strict, ConfigError, ConfigErrorCode, ConfigKind};
use serde_json::{json, Value};

use crate::output::StructuredError;

/// Bound on a configuration file read, matching `deploy model --file`.
const MAX_BYTES: u64 = 1024 * 1024;

pub fn validate_config(file: &Path, host: Option<&Path>) -> Result<Value, StructuredError> {
    let text = read(file)?;
    let kind = detect_kind(&text).map_err(|e| named(file, None, &e))?;
    if host.is_some() && kind != ConfigKind::Deployment {
        return Err(invalid(format!(
            "{}: --host applies only to a deployment document, not a {} document",
            file.display(),
            kind.as_str()
        )));
    }
    let resolved_against = match kind {
        ConfigKind::Server => {
            mllm_config::remote_roles::ServerConfig::parse(&text)
                .map_err(|e| named(file, Some(kind), &e))?;
            Value::Null
        }
        ConfigKind::Host => {
            host_policy_document(&text).map_err(|e| named(file, Some(kind), &e))?;
            Value::Null
        }
        ConfigKind::Standalone => {
            let document = parse_strict(kind, &text).map_err(|e| named(file, Some(kind), &e))?;
            mllm_config::remote_roles::drain_timeout(&document)
                .map_err(|e| named(file, Some(kind), &e))?;
            Value::Null
        }
        // ADR 0018 §2: `engines.yaml` validates like any other document kind.
        ConfigKind::Engines => {
            mllm_config::registration::EnginesFile::parse(file, &text)
                .map_err(|e| named(file, Some(kind), &e))?;
            Value::Null
        }
        ConfigKind::Deployment => {
            let deployment = parse_strict(kind, &text).map_err(|e| named(file, Some(kind), &e))?;
            let instances = mllm_config::instances::parse_instance_spec(&deployment)
                .map_err(|e| named(file, Some(kind), &e))?;
            // ADR 0014 amendment A1: declared timeouts, their floors, and the
            // deployment's own request deadline; the host's default is checked
            // when `--host` resolves it.
            mllm_config::effective::validate_declared_timeouts(&deployment)
                .map_err(|e| named(file, Some(kind), &e))?;
            // Owner decision 2026-09-23: a declared startup peak.
            mllm_config::effective::validate_declared_startup(&deployment)
                .map_err(|e| named(file, Some(kind), &e))?;
            match host {
                None => Value::Null,
                Some(host_file) => {
                    let host_text = read(host_file)?;
                    let (name, host_document) = host_policy_document(&host_text)
                        .map_err(|e| named(host_file, Some(ConfigKind::Host), &e))?;
                    // ADR 0013 §2, §3: as the server resolves each allowed
                    // host: a host outside the allowed set is no candidate,
                    // unnamed device claims take this host's devices, and the
                    // per-host recipe carries no deployment-level field.
                    if !instances.placement.allows(&name) {
                        return Err(invalid(format!(
                            "{}: deployment document: its placement does not allow host `{name}`",
                            file.display()
                        )));
                    }
                    let mut source =
                        mllm_config::instances::assign_devices(&deployment, &host_document)
                            .map_err(|e| named(file, Some(kind), &e))?;
                    if let Some(object) = source.as_object_mut() {
                        for field in ["instances", "placement", "host"] {
                            object.remove(field);
                        }
                    }
                    let (effective, provisional) = resolve_for_acceptance(&source, &host_document)
                        .map_err(|e| named(file, Some(kind), &e))?;
                    return Ok(json!({
                        "valid": true,
                        "kind": kind.as_str(),
                        "file": file.display().to_string(),
                        "resolved_against": name,
                        "provisional": provisional,
                        "effective": {
                            "name": effective.name,
                            "residency": effective.residency,
                            "recipe": effective.recipe,
                            "recipe_fingerprint": effective.recipe_fingerprint,
                            "resources": effective.resources,
                            "request_deadline_ms": effective.request_deadline_ms,
                            "timeouts": effective.timeouts,
                            // Owner decision 2026-09-23: what admission
                            // reserves from arm until Ready, before any
                            // measurement on the host.
                            "startup": mllm_config::effective::startup_budget(&effective),
                            // ADR 0014 §5 (owner decision 2026-09-25): the
                            // context the launch passes, fitted to the KV
                            // grant when undeclared, read from the checkpoint
                            // as this machine sees it (a remote host fits it
                            // again from its own copy at launch).
                            "context": mllm_config::context_fit::fit_for_effective(&effective),
                        },
                    }));
                }
            }
        }
    };
    Ok(json!({
        "valid": true,
        "kind": kind.as_str(),
        "file": file.display().to_string(),
        "resolved_against": resolved_against,
    }))
}

/// ADR 0014 §5, §7: acceptance resolves a deployment whose memory is derived
/// from weights not yet measured as `provisional`, exactly as the management
/// API does; any other resolution failure is a refusal.
fn resolve_for_acceptance(
    deployment: &Value,
    host: &Value,
) -> Result<(mllm_config::effective::EffectiveDeployment, bool), ConfigError> {
    match mllm_config::effective::resolve_effective(deployment, host) {
        Ok(effective) => Ok((effective, false)),
        Err(error)
            if error.code == ConfigErrorCode::NotMaterializable
                && error.path.starts_with("engine_config.memory") =>
        {
            let placeholder = mllm_config::effective::CheckpointFacts {
                weights_bytes: Some(0),
                ..Default::default()
            };
            mllm_config::effective::resolve_effective_with_checkpoint(deployment, host, placeholder)
                .map(|effective| (effective, true))
        }
        Err(error) => Err(error),
    }
}

/// The host document as host publication and the agent resolve against it:
/// the role-local settings removed and the resource policy normalized.
fn host_policy_document(text: &str) -> Result<(String, Value), ConfigError> {
    let config = mllm_config::remote_roles::HostConfig::parse(text)?;
    let local = mllm_config::remote_resources::local_host_document(&config.document)?;
    mllm_config::effective::normalize_host_policy(&local)?;
    Ok((config.name, local))
}

/// The document kind, taken from the strict parse of each known kind. A kind
/// that parses settles it; otherwise the kind is the one whose failure is not
/// a kind mismatch. A failure every kind shares (a duplicate key, a missing
/// `kind`) is reported without a kind, since the document never said one.
fn detect_kind(text: &str) -> Result<ConfigKind, ConfigError> {
    let kinds = [
        ConfigKind::Server,
        ConfigKind::Host,
        ConfigKind::Deployment,
        ConfigKind::Standalone,
        ConfigKind::Engines,
    ];
    let mut failures = Vec::new();
    for kind in kinds {
        match parse_strict(kind, text) {
            Ok(_) => return Ok(kind),
            Err(e) if e.code == ConfigErrorCode::SchemaVersion && e.path == "kind" => {}
            Err(e) => failures.push((kind, e)),
        }
    }
    match failures.as_slice() {
        [] => Err(ConfigError::new(
            ConfigErrorCode::SchemaVersion,
            "kind",
            "document kind is not one of server, host, deployment, standalone, engines",
        )),
        [(kind, _)] => Ok(*kind),
        [(_, first), ..] => Err(first.clone()),
    }
}

fn read(file: &Path) -> Result<String, StructuredError> {
    use std::io::Read;
    let source = std::fs::File::open(file).map_err(|_| {
        invalid(format!(
            "{}: cannot read configuration file",
            file.display()
        ))
    })?;
    let mut text = String::new();
    source
        .take(MAX_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|_| {
            invalid(format!(
                "{}: cannot read configuration file",
                file.display()
            ))
        })?;
    if text.len() as u64 > MAX_BYTES {
        return Err(invalid(format!(
            "{}: configuration file is too large",
            file.display()
        )));
    }
    Ok(text)
}

/// The stable name of a configuration error, as reported to the operator.
fn error_name(code: ConfigErrorCode) -> &'static str {
    match code {
        ConfigErrorCode::UnknownField => "unknown_field",
        ConfigErrorCode::DuplicateKey => "duplicate_key",
        ConfigErrorCode::InvalidUnit => "invalid_unit",
        ConfigErrorCode::MissingRequired => "missing_required",
        ConfigErrorCode::UnsupportedCombination => "unsupported_combination",
        ConfigErrorCode::ConflictingArgs => "conflicting_args",
        ConfigErrorCode::InvalidCacheRef => "invalid_cache_ref",
        ConfigErrorCode::ContradictoryConnection => "contradictory_connection",
        ConfigErrorCode::SchemaVersion => "schema_version",
        ConfigErrorCode::NotMaterializable => "not_materializable",
        ConfigErrorCode::ModelSourceDenied => "model_source_denied",
        ConfigErrorCode::Io => "io",
    }
}

fn named(file: &Path, kind: Option<ConfigKind>, error: &ConfigError) -> StructuredError {
    let kind = kind.map(|k| format!("{} ", k.as_str())).unwrap_or_default();
    invalid(format!(
        "{}: {kind}document: {} at `{}`: {}",
        file.display(),
        error_name(error.code),
        error.path,
        error.detail
    ))
}

fn invalid(message: String) -> StructuredError {
    StructuredError {
        code: "invalid_config",
        message,
    }
}
