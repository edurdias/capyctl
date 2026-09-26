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
    // Owner decision 2026-09-25: a `~/` model path means this user's home, as
    // `deploy model --file` reads it.
    let text = crate::deployment_file::with_home_expanded(&read(file)?);
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
            host_policy_document(file).map_err(|e| named(file, Some(kind), &e))?;
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
                // Owner decision 2026-09-25: the document with the defaults a
                // minimal file leaves out, as deploy sends it.
                // SPEC §15.3: say plainly what was not checked.
                None => {
                    let mut out = json!({
                        "valid": true,
                        "kind": kind.as_str(),
                        "file": file.display().to_string(),
                        "resolved_against": Value::Null,
                        "document": deployment,
                    });
                    let mut unchecked = vec![
                        "resolution against a host: pass --host <host.yaml> to check the runtime profile, placement, devices, resources and timeouts, and to see the host's defaults",
                    ];
                    unchecked.extend_from_slice(REQUIRES_SERVER);
                    out["requires_server"] = json!(unchecked);
                    return Ok(out);
                }
                Some(host_file) => {
                    read(host_file)?;
                    let (name, host_document) = host_policy_document(host_file)
                        .map_err(|e| named(host_file, Some(ConfigKind::Host), &e))?;
                    // ADR 0013 §2, §3, ADR 0018 §7: the checks the server's
                    // deploy runs against this host's publication, in its
                    // order (`registry_targets`), so a file validate accepts
                    // is not refused by deploy for a reason it could name.
                    // A host outside the allowed set is no candidate.
                    if !instances.placement.allows(&name) {
                        return Err(invalid(format!(
                            "{}: deployment document: its placement does not allow host `{name}`",
                            file.display()
                        )));
                    }
                    let labels = mllm_config::instances::host_labels(&host_document)
                        .map_err(|e| named(host_file, Some(ConfigKind::Host), &e))?;
                    if !instances.placement.selector_matches(&labels) {
                        return Err(invalid(format!(
                            "{}: deployment document: its placement.selector does not match the labels of host `{name}` (deploy refuses it: selector_mismatch)",
                            file.display()
                        )));
                    }
                    let profiles = host_document["runtime_profiles"]
                        .as_object()
                        .cloned()
                        .unwrap_or_default();
                    if profiles.is_empty() {
                        return Err(invalid(format!(
                            "{}: host `{name}` declares no runtime profile, in its document or in {}; register one with `mllm engine add` (deploy refuses it: no_runtime_profiles)",
                            host_file.display(),
                            mllm_config::registration::engines_beside(host_file).display()
                        )));
                    }
                    let profile = deployment["runtime_profile"].as_str().unwrap_or_default();
                    // Owner decision 2026-09-25: an engine family names the
                    // host's one profile of that family.
                    if mllm_config::deployment_defaults::profile_on_host(profile, &host_document)
                        .is_none()
                    {
                        let names: Vec<&str> = profiles.keys().map(String::as_str).collect();
                        return Err(StructuredError {
                            code: "profile_not_published",
                            message: format!(
                                "{}: deployment document: runtime profile `{profile}` is not declared by host `{name}` (it declares {}); register it with `mllm engine add`, or name one of those",
                                file.display(),
                                names.join(", ")
                            ),
                        });
                    }
                    // Unnamed device claims take this host's devices, and the
                    // per-host recipe carries no deployment-level field.
                    let strip = |mut source: Value| {
                        if let Some(object) = source.as_object_mut() {
                            for field in ["instances", "placement", "host"] {
                                object.remove(field);
                            }
                        }
                        source
                    };
                    // As the server resolves it: both documents scoped to the
                    // host's ledger keys, and the host's resource policy
                    // composed as the current controls (its first publication
                    // stores exactly these).
                    let scoped = scoped_resolution(&name, &deployment, &host_document)
                        .map_err(|e| named(file, Some(kind), &e))?;
                    resolve_for_acceptance(&strip(scoped.0), &scoped.1)
                        .map_err(|e| named(file, Some(kind), &e))?;
                    // The host-local view the operator reads (device and
                    // domain names as the host document writes them).
                    let source = strip(
                        mllm_config::instances::assign_devices(&deployment, &host_document)
                            .map_err(|e| named(file, Some(kind), &e))?,
                    );
                    let (effective, provisional) = resolve_for_acceptance(&source, &host_document)
                        .map_err(|e| named(file, Some(kind), &e))?;
                    return Ok(json!({
                        "valid": true,
                        "kind": kind.as_str(),
                        "file": file.display().to_string(),
                        "resolved_against": name,
                        "requires_server": REQUIRES_SERVER,
                        "provisional": provisional,
                        // Owner decision 2026-09-25: the document as this host
                        // runs it, with every default filled.
                        "document": source,
                        "effective": {
                            "name": effective.name,
                            "routes": effective.routes,
                            "runtime_profile": source["runtime_profile"],
                            "runtime_profile_revision": effective.profile.revision,
                            "selected_devices": effective.selected_devices,
                            "memory": effective.engine_config.memory(),
                            "provenance": serde_json::to_value(&effective.engine_config)
                                .ok()
                                .map(|config| config["provenance"].clone()),
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
    let out = json!({
        "valid": true,
        "kind": kind.as_str(),
        "file": file.display().to_string(),
        "resolved_against": resolved_against,
    });
    Ok(out)
}

/// SPEC §15.3: the deploy checks that need the server's live state, which an
/// offline validation cannot run. `deploy model` runs them and names any that
/// refuses.
const REQUIRES_SERVER: &[&str] = &[
    "whether each allowed host is enrolled, online and has published (host_unpublished)",
    "which runtime profiles the host's role accepted and published after measuring each installation (profile_not_published)",
    "the host's current resource policy as the server stores it (resource_policy_unavailable)",
    "route and deployment-name conflicts with existing deployments (route_conflict)",
    "a new checkpoint's digest, measured on the host after acceptance (checkpoint_digest_pending)",
];

/// ADR 0013 §3: the deployment and host documents exactly as the server
/// resolves them for a registry host: scoped to the host's ledger keys, with
/// the host's resource policy composed as the current controls, and unnamed
/// device claims assigned from the scoped host.
fn scoped_resolution(
    name: &str,
    deployment: &Value,
    host: &Value,
) -> Result<(Value, Value), ConfigError> {
    use mllm_config::effective::{compose_current_resource_controls, normalize_host_policy};
    use mllm_config::remote_resources::{scope_deployment_document, scope_host_document};
    let trusted = scope_host_document(name, host)?;
    let policy = normalize_host_policy(&trusted)?;
    let host = compose_current_resource_controls(
        &trusted,
        &mllm_config::resource_controls::ResourceContext::from_host(&policy),
        &mllm_config::resource_controls::ResourceControls::from_host(&policy),
    )?;
    let command = scope_deployment_document(name, deployment)?;
    let source = mllm_config::instances::assign_devices(&command, &host)?;
    Ok((source, host))
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
/// merged with the `engines.yaml` beside it (ADR 0018 §2, as the host role
/// loads it), the role-local settings removed and the resource policy
/// normalized.
fn host_policy_document(path: &Path) -> Result<(String, Value), ConfigError> {
    let engines = mllm_config::registration::engines_beside(path);
    // Owner decision 2026-09-25: the models directory and the model-source
    // policy as the host role resolves them (without its run's flags).
    let config = mllm_config::remote_roles::HostConfig::load_with_engines(path, &engines)?
        .with_models(
            &Default::default(),
            &mllm_config::model_settings::ModelOverrides::from_process_env()?,
            std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .as_deref(),
        )?;
    let local = mllm_config::remote_resources::local_host_document(&config.document)?;
    mllm_config::effective::normalize_host_policy(&local)?;
    Ok((config.name, local))
}

/// The document kind, taken from the strict parse of each known kind. A kind
/// that parses settles it; otherwise the kind is the one whose failure is not
/// a kind mismatch. A failure every kind shares (a duplicate key, a missing
/// `kind`) is reported without a kind, since the document never said one.
fn detect_kind(text: &str) -> Result<ConfigKind, ConfigError> {
    // Owner decision 2026-09-25: a deployment is the one kind a document may
    // leave implied, so a document without `kind` is a deployment.
    if mllm_config::parse_document(text)?
        .as_object()
        .is_some_and(|document| !document.contains_key("kind"))
    {
        return Ok(ConfigKind::Deployment);
    }
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
