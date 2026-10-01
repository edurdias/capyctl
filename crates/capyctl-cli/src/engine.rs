//! ADR 0018: `capyctl engine detect|add|list|remove`, the same on a host and in
//! standalone. Detection reads metadata only; an installation runs only
//! after the operator named or picked it.
mod target;
pub use target::{absolute, named_role_document, resolve_target, role_engines, RoleKind, Target};

use crate::grammar::{Command, DeepParkChoice, DriftChoice};
use crate::output::StructuredError;
use capyctl_agent::control_socket::{request, ClientError, ControlRequest};
use capyctl_agent::engines::{
    check_version, detect, resolve, Resolved, ScanBounds, ScanRoots, VERSION_CHECK_TIMEOUT,
};
use capyctl_config::effective::InstallationDrift;
use capyctl_config::engine_policy::Engine;
use capyctl_config::registration::{
    check_profile, lock_engines_for, profile_document, valid_profile_name, write_engines,
    EnginesFile, ProfileSpec, ENVIRONMENT_PROFILES,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

const ADD_REPLY: Duration = Duration::from_secs(60);
/// review decision I4: how long `remove` waits for the role. The role
/// answers a retirement within its own bound (`RETIRE_BOUND`, 960 s, which
/// covers the server's 900 s drain window); this adds a clear margin for the
/// role's measuring and the socket, so the CLI does not give up on a role
/// that is still about to answer.
pub const REMOVE_REPLY: Duration =
    Duration::from_secs(capyctl_agent::host_control::RETIRE_BOUND.as_secs() + 240);
const LIST_REPLY: Duration = Duration::from_secs(5);

fn error(code: &'static str, message: impl Into<String>) -> StructuredError {
    StructuredError {
        code,
        message: message.into(),
    }
}

/// A role's reply code as a closed CLI code.
fn closed(code: &str) -> &'static str {
    match code {
        "publish_rejected" => "publish_rejected",
        "profile_in_use" => "profile_in_use",
        "agent_unreachable" => "agent_unreachable",
        "profile_exists" => "profile_exists",
        "invalid_config" => "invalid_config",
        _ => "internal",
    }
}

pub fn is_engine_command(command: &Command) -> bool {
    matches!(
        command,
        Command::EngineDetect { .. }
            | Command::EngineAdd { .. }
            | Command::EngineList
            | Command::EngineRemove { .. }
    )
}

pub async fn execute(
    command: &Command,
    config: Option<&Path>,
    state_dir: &Path,
) -> Result<Value, StructuredError> {
    let env = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
    execute_in(command, config, state_dir, &env).await
}

/// As [`execute`], reading `HOME`, `XDG_CONFIG_HOME` and `CAPYCTL_CONFIG` through
/// `env` (a test states them instead of changing the process environment).
pub async fn execute_in(
    command: &Command,
    config: Option<&Path>,
    state_dir: &Path,
    env: &(dyn Fn(&str) -> Option<String> + Sync),
) -> Result<Value, StructuredError> {
    match command {
        Command::EngineDetect { paths } => Ok(detected(paths)),
        Command::EngineAdd {
            path,
            name,
            deep_park,
            drift,
            args,
        } => {
            let target = resolve_target(config, state_dir, env)?;
            add(
                &target,
                path.as_deref(),
                name.as_deref(),
                *deep_park,
                *drift,
                args,
            )
            .await
        }
        Command::EngineList => list(&resolve_target(config, state_dir, env)?).await,
        Command::EngineRemove { name, drain } => {
            remove(&resolve_target(config, state_dir, env)?, name, *drain).await
        }
        _ => Err(error("invalid_config", "not an engine command")),
    }
}

fn candidates(paths: &[PathBuf]) -> Vec<capyctl_agent::engines::Candidate> {
    detect(&ScanRoots::from_env(paths.to_vec()), &ScanBounds::default())
}

fn detected(paths: &[PathBuf]) -> Value {
    let rows: Vec<Value> = candidates(paths)
        .into_iter()
        .map(|c| {
            json!({"engine": c.engine.name(), "version": c.version, "custom": c.custom,
            "env": c.env, "entry": c.entry, "source": c.source})
        })
        .collect();
    json!({"candidates": rows})
}

/// ADR 0018 §1: with no path, an operator at a terminal picks a candidate.
fn pick() -> Result<PathBuf, StructuredError> {
    use std::io::{BufRead, IsTerminal, Write};
    if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
        return Err(error(
            "not_interactive",
            "engine add without a path needs a terminal; name the installation",
        ));
    }
    let found = candidates(&[]);
    if found.is_empty() {
        return Err(error(
            "engine_not_found",
            "no installation found; name it, or use engine detect --path DIR",
        ));
    }
    let mut stderr = std::io::stderr();
    for (i, c) in found.iter().enumerate() {
        let _ = writeln!(
            stderr,
            "{:>3}  {} {}{}  {}",
            i + 1,
            c.engine.name(),
            c.version,
            if c.custom { " (custom)" } else { "" },
            c.env.display()
        );
    }
    let _ = write!(stderr, "register which? ");
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|_| error("not_interactive", "no answer"))?;
    let index: usize = line
        .trim()
        .parse()
        .map_err(|_| error("invalid_config", "not a number from the list"))?;
    found
        .get(index.wrapping_sub(1))
        .map(|c| c.entry.clone())
        .ok_or_else(|| error("invalid_config", "not a number from the list"))
}

struct Registration {
    version: String,
    fingerprint: Option<capyctl_agent::installation::InstallationFingerprint>,
    deep_park_missing: Option<bool>,
}

/// ADR 0018 §1 step 3: executed only now, because the operator named it.
fn register(resolved: &Resolved, state_dir: &Path) -> Result<Registration, StructuredError> {
    let version = check_version(resolved, VERSION_CHECK_TIMEOUT).map_err(|e| {
        error(
            "engine_version_failed",
            format!(
                "{}: {e}; nothing was written",
                resolved.executable.display()
            ),
        )
    })?;
    let fingerprint = capyctl_agent::installation::InstallationMeasurer::new()
        .measure(resolved.engine, &resolved.executable)
        .ok();
    // review decision 2026-09-25: `engine add` before any role has started
    // is the first run. The state root the role will use is created
    // owner-only, as the role creates it, so the probe has somewhere private
    // to work; engines.yaml is written and the role picks it up at start.
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(state_dir)
            .map_err(|e| {
                error(
                    "internal",
                    format!("state directory {}: {e}", state_dir.display()),
                )
            })?;
    }
    let scratch = tempfile::tempdir_in(state_dir)
        .map_err(|e| error("internal", format!("probe directory: {e}")))?;
    let runtime = scratch.path().join("runtime");
    crate::managed_runtime::prepare_for_role(&runtime)?;
    let report = capyctl_agent::installation::probe_capabilities(
        resolved.engine,
        &resolved.executable,
        &runtime,
        capyctl_agent::installation::PROBE_TIMEOUT,
    );
    Ok(Registration {
        version,
        fingerprint,
        deep_park_missing: report.and_then(|r| r.available("deep_park")).map(|a| !a),
    })
}

/// The role document, strictly parsed (never written). A standalone document
/// that does not exist yet (the first run; start generates it) declares
/// nothing.
fn role_document(target: &Target) -> Result<Value, StructuredError> {
    if target.kind == RoleKind::Standalone && !target.role_document.exists() {
        return Ok(serde_json::json!({}));
    }
    let text = std::fs::read_to_string(&target.role_document).map_err(|e| {
        error(
            "invalid_config",
            format!("{}: {e}", target.role_document.display()),
        )
    })?;
    let kind = match target.kind {
        RoleKind::Host => capyctl_config::ConfigKind::Host,
        RoleKind::Standalone => capyctl_config::ConfigKind::Standalone,
    };
    capyctl_config::parse_strict(kind, &text)
        .map_err(|e| error("invalid_config", format!("{}: {}", e.path, e.detail)))
}

/// Profiles the operator declared in the role document itself.
/// Whether `name` is registered in engines.yaml or declared in the role
/// document (an older host may have registered a profile named `local`).
fn registered_or_declared(target: &Target, name: &str) -> Result<bool, StructuredError> {
    let engines =
        EnginesFile::load(&target.engines).map_err(|e| error("invalid_config", e.detail))?;
    Ok(engines.profiles.contains_key(name) || declared_by_operator(target)?.contains_key(name))
}

fn declared_by_operator(
    target: &Target,
) -> Result<serde_json::Map<String, Value>, StructuredError> {
    let document = role_document(target)?;
    let profiles = match target.kind {
        RoleKind::Host => &document["runtime_profiles"],
        RoleKind::Standalone => &document["host"]["runtime_profiles"],
    };
    Ok(profiles.as_object().cloned().unwrap_or_default())
}

/// ADR 0018 §2: lock engines.yaml, re-check the name against both files,
/// validate, write. The role document is never written. The revision written.
fn write_profile(target: &Target, name: &str, spec: &ProfileSpec) -> Result<u64, StructuredError> {
    let lock = lock_engines_for(&target.engines, service_owner(target))
        .map_err(|e| error("internal", format!("{}: {}", e.path, e.detail)))?;
    let mut engines =
        EnginesFile::load(&target.engines).map_err(|e| error("invalid_config", e.detail))?;
    if engines.profiles.contains_key(name) || declared_by_operator(target)?.contains_key(name) {
        return Err(error(
            "profile_exists",
            format!("profile {name} exists; use --name, or remove it first"),
        ));
    }
    let profile = profile_document(spec);
    check_profile(name, &profile)
        .map_err(|e| error("invalid_config", format!("{}: {}", e.path, e.detail)))?;
    engines.profiles.insert(name.to_owned(), profile);
    let host = match target.kind {
        RoleKind::Host => Some(role_document(target)?),
        RoleKind::Standalone => None,
    };
    write_engines(&engines, &lock, host.as_ref()).map_err(|e| error("invalid_config", e.detail))
}

async fn add(
    target: &Target,
    path: Option<&Path>,
    name: Option<&str>,
    deep_park: Option<DeepParkChoice>,
    drift: DriftChoice,
    args: &[String],
) -> Result<Value, StructuredError> {
    let path = match path {
        Some(path) => path.to_path_buf(),
        None => pick()?,
    };
    let resolved = resolve(&path).map_err(|e| error(e.code(), e.to_string()))?;
    let name = name.unwrap_or(resolved.engine.name()).to_owned();
    if !valid_profile_name(&name) {
        return Err(error(
            "invalid_config",
            format!("profile name {name:?} must be lowercase letters, digits, '-' or '_'"),
        ));
    }
    // Owner rule 2026-09-25: on a host as in standalone, these names are the
    // role's own installation (`local_engine`, `--vllm-bin`, `CAPYCTL_VLLM_BIN`).
    if ENVIRONMENT_PROFILES.contains(&name.as_str()) {
        return Err(error("profile_exists", format!("{name} is reserved for the role's own installation (--vllm-bin / --sglang-bin, CAPYCTL_VLLM_BIN / CAPYCTL_SGLANG_BIN or local_engine); use --name")));
    }
    // Checked before anything runs, and again under the lock when writing.
    let existing =
        EnginesFile::load(&target.engines).map_err(|e| error("invalid_config", e.detail))?;
    if existing.profiles.contains_key(&name) || declared_by_operator(target)?.contains_key(&name) {
        return Err(error(
            "profile_exists",
            format!("profile {name} exists; use --name, or remove it first"),
        ));
    }
    // SPEC §13.3 amendment (owner decision 2026-09-25).
    let cuda_home = capyctl_config::registration::detect_cuda_home(
        std::env::var("CUDA_HOME").ok().as_deref(),
        |nvcc| nvcc.is_file(),
    );
    if resolved.engine == Engine::Tensorfold {
        // ADR 0023 §2: TensorFold has no park path; refuse a request for one
        // before anything runs or is written.
        if deep_park == Some(DeepParkChoice::Enabled) {
            return Err(error(
                "capability_missing",
                "TensorFold has no sleep or release API; it runs restart_only, so \
                 --deep-park enabled is refused and nothing was written",
            ));
        }
        let bin = resolved.executable.parent().unwrap_or(&resolved.env);
        capyctl_config::toolchain::check(
            bin,
            cuda_home.as_deref(),
            capyctl_config::toolchain::SYSTEM_PATH,
        )
        .map_err(|missing| {
            error(
                "toolchain_missing",
                format!("{missing}; nothing was written"),
            )
        })?;
    }
    let (r, state) = (resolved.clone(), target.state_dir.clone());
    let registration = tokio::task::spawn_blocking(move || register(&r, &state))
        .await
        .map_err(|_| error("internal", "registration failed"))??;
    let probe = match registration.deep_park_missing {
        Some(true) => "capability_missing",
        Some(false) => "available",
        None => "unknown",
    };
    // Owner decision 2026-09-25: missing deep park is recorded as disabled unless asked.
    let deep = match deep_park {
        Some(DeepParkChoice::Enabled) => true,
        Some(DeepParkChoice::Disabled) => false,
        // ADR 0023 §2: TensorFold is disabled even when the probe could not run.
        None => {
            resolved.engine != Engine::Tensorfold && registration.deep_park_missing != Some(true)
        }
    };
    let spec = ProfileSpec {
        engine: resolved.engine,
        executable: resolved.executable.clone(),
        build_fingerprint: registration.version.clone(),
        deep_park: deep,
        installation_drift: match drift {
            DriftChoice::Warn => InstallationDrift::Warn,
            DriftChoice::Refuse => InstallationDrift::Refuse,
        },
        args: args.to_vec(),
        cuda_home,
    };
    let revision = write_profile(target, &name, &spec)?;
    let mut out = json!({
        "profile": name, "engine": resolved.engine.name(), "version": registration.version,
        "custom": resolved.custom(), "executable": resolved.executable,
        "fingerprint": registration.fingerprint.map(|f| json!({"version": f.version, "digest": f.digest})),
        "deep_park": if deep { "enabled" } else { "disabled" }, "deep_park_probe": probe,
        "engines_file": target.engines, "revision": revision,
        "cuda_home": spec.cuda_home,
    });
    match request(&target.socket, &ControlRequest::Add, ADD_REPLY).await {
        Ok(reply) if reply["ok"] == true => {
            out["published"] = reply["published"].clone();
            Ok(out)
        }
        Ok(reply) => Err(error(
            closed(reply["code"].as_str().unwrap_or("")),
            format!(
                "{} (the profile is written at revision {revision} and shows as not published)",
                reply["message"].as_str().unwrap_or("refused")
            ),
        )),
        // Owner decision 2026-09-25 (first-run walk): no role is running,
        // which is the normal first run (standalone refuses to start with no
        // engine). The profile is saved and the role publishes it when it
        // starts, so this is a success with a notice, not an error. SPEC §8,
        // ADR 0018 §3.
        Err(ClientError::NotRunning(_)) => {
            out["published"] = json!("role_not_running");
            out["notice"] = json!(format!(
                "saved to {} (revision {revision}); start capyctl (`{}`) to use it",
                target.engines.display(),
                match target.kind {
                    RoleKind::Standalone => "capyctl start standalone",
                    RoleKind::Host => "capyctl start host",
                }
            ));
            Ok(out)
        }
        // A role is there but did not take or answer the request: that is a
        // fault the operator must look at.
        Err(e) => Err(error(
            "agent_unreachable",
            format!(
                "{e}; {} is written (revision {revision}); it takes effect when the role starts",
                target.engines.display()
            ),
        )),
    }
}

async fn list(target: &Target) -> Result<Value, StructuredError> {
    let engines =
        EnginesFile::load(&target.engines).map_err(|e| error("invalid_config", e.detail))?;
    let role = request(&target.socket, &ControlRequest::List, LIST_REPLY)
        .await
        .ok()
        .filter(|r| r["ok"] == true);
    // ADR 0018 §2: registered profiles, then the ones the operator declared.
    let mut all: Vec<(String, Value, &'static str)> = engines
        .profiles
        .clone()
        .into_iter()
        .map(|(n, p)| (n, p, "engines.yaml"))
        .collect();
    all.extend(
        declared_by_operator(target)?
            .into_iter()
            .map(|(n, p)| (n, p, "role document")),
    );
    // ADR 0018 §5: a standalone role's environment profiles (`local`,
    // `local-vllm`, `local-sglang`) live in neither file; the role publishes
    // them, so they are listed from what it accepted.
    if let Some(accepted) = role.as_ref().and_then(|r| r["accepted"].as_object()) {
        let known: std::collections::BTreeSet<String> =
            all.iter().map(|(n, _, _)| n.clone()).collect();
        for (name, entry) in accepted {
            if known.contains(name) {
                continue;
            }
            let profile = json!({
                "engine": entry["engine"], "executable": entry["executable"],
                "build_fingerprint": entry["build_fingerprint"],
                "security": {"deep_park": entry["deep_park"]},
            });
            all.push((name.clone(), profile, "environment"));
        }
    }
    let rows: Vec<Value> = all
        .into_iter()
        .map(|(name, profile, source)| {
            let engine = profile["engine"].as_str().and_then(Engine::from_name).unwrap_or(Engine::Vllm);
            let accepted = role.as_ref().map(|r| r["accepted"].get(&name).cloned());
            let version = accepted
                .clone()
                .flatten()
                .and_then(|a| a["installation"]["version"].as_str().map(str::to_owned))
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| profile["build_fingerprint"].as_str().unwrap_or("unknown").to_owned());
            json!({
                "profile": name, "source": source, "engine": profile["engine"], "version": version,
                "custom": !capyctl_config::registration::is_verified(engine, &version),
                "executable": profile["executable"],
                "fingerprint": accepted.clone().flatten().map(|a| a["installation"].clone()),
                "deep_park": profile["security"]["deep_park"],
                "deep_park_probe": accepted.clone().flatten().map(|a| a["deep_park_probe"].clone()).unwrap_or(json!("unknown")),
                "published": match &accepted { None => "unknown", Some(Some(_)) => "published", Some(None) => "not published" },
                "deployments": role
                    .as_ref()
                    .map(|r| r["users"][&name].clone())
                    .filter(|users| !users.is_null())
                    .unwrap_or(json!([])),
            })
        })
        .collect();
    Ok(
        json!({"engines_file": target.engines, "revision": engines.revision,
        "agent": if role.is_some() { "reachable" } else { "unreachable" }, "engines": rows}),
    )
}

/// ADR 0018 §4 (review decision C1): the CLI is the only writer of
/// engines.yaml. Remove asks the running role to retire the profile, waits
/// for the confirmation, then writes engines.yaml without it and asks the
/// role to reload, which publishes the removal. A retry after any failure
/// resumes the same retirement (review decision I1), so a crash between the
/// confirmation and the write is finished by running remove again.
async fn remove(target: &Target, name: &str, drain: bool) -> Result<Value, StructuredError> {
    if ENVIRONMENT_PROFILES.contains(&name) && !registered_or_declared(target, name)? {
        return Err(error("invalid_config", format!(
            "{name} comes from the role's own installation (--vllm-bin / --sglang-bin, CAPYCTL_VLLM_BIN / CAPYCTL_SGLANG_BIN or local_engine); unset it and restart the role instead"
        )));
    }
    let engines =
        EnginesFile::load(&target.engines).map_err(|e| error("invalid_config", e.detail))?;
    let registered = engines.profiles.contains_key(name);
    // ADR 0018 §2: a profile declared in the role document is the operator's.
    if !registered && declared_by_operator(target)?.contains_key(name) {
        return Err(error(
            "invalid_config",
            format!(
                "{name} is declared in {}; edit that file and restart the role",
                target.role_document.display()
            ),
        ));
    }
    let not_registered = || {
        error(
            "invalid_config",
            format!("{} has no profile named {name}", target.engines.display()),
        )
    };
    let retired = match request(
        &target.socket,
        &ControlRequest::Remove {
            profile: name.into(),
            drain,
        },
        REMOVE_REPLY,
    )
    .await
    {
        Ok(reply) if reply["ok"] == true => reply["retired"] == true,
        Ok(reply) => {
            let mut message = reply["message"].as_str().unwrap_or("refused").to_owned();
            if let Some(names) = reply["deployments"].as_array().filter(|n| !n.is_empty()) {
                let names: Vec<&str> = names.iter().filter_map(Value::as_str).collect();
                message = format!("{message}: {}", names.join(", "));
            }
            return Err(error(closed(reply["code"].as_str().unwrap_or("")), message));
        }
        // review decision I4: the role took the request and may have
        // acted on it; only `engine list` can say what happened.
        Err(ClientError::Unanswered(e)) => return Err(unknown_outcome(&e, name)),
        Err(_) if !registered => return Err(not_registered()),
        // Owner decision 2026-09-25: a published profile is never removed unconfirmed.
        Err(e) => {
            return Err(error(
                "agent_unreachable",
                format!("{e}; nothing was removed; start the role and retry"),
            ))
        }
    };
    // Neither registered nor published by the role: there is nothing to remove.
    if !registered && !retired {
        return Err(not_registered());
    }
    let revision = if registered {
        write_without(target, name).map_err(|e| {
            error(
                "internal",
                format!(
                    "{}: {}; {}. Run `capyctl engine remove {name}` again to finish",
                    e.path,
                    e.detail,
                    if retired {
                        format!("{name} is retired, so nothing new is placed on it")
                    } else {
                        "nothing was removed".into()
                    }
                ),
            )
        })?
    } else {
        engines.revision
    };
    let mut out = json!({"removed": name, "engines_file": target.engines, "revision": revision});
    match request(&target.socket, &ControlRequest::Add, ADD_REPLY).await {
        Ok(reply) if reply["ok"] == true => {
            out["published"] = reply["published"].clone();
            Ok(out)
        }
        Ok(reply) => Err(error(
            closed(reply["code"].as_str().unwrap_or("")),
            format!(
                "{}; {name} is out of {} (revision {revision}) but the role has not published its removal; run `capyctl engine remove {name}` again to finish",
                reply["message"].as_str().unwrap_or("refused"),
                target.engines.display()
            ),
        )),
        Err(e) => Err(error(
            "agent_unreachable",
            format!(
                "{e}; {name} is out of {} (revision {revision}) but its removal may not be published; run `capyctl engine list`, and `capyctl engine remove {name}` again to finish",
                target.engines.display()
            ),
        )),
    }
}

/// review decision C1: who the role runs as, when this CLI is root and the
/// role's state directory belongs to another user: a new engines.yaml goes to
/// that user so the role can read it. `None` otherwise (the writer owns it).
fn service_owner(target: &Target) -> Option<(u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid has no preconditions and cannot fail.
    if unsafe { libc::geteuid() } != 0 {
        return None;
    }
    let meta = std::fs::symlink_metadata(&target.state_dir).ok()?;
    (meta.file_type().is_dir() && meta.uid() != 0).then(|| (meta.uid(), meta.gid()))
}

/// engines.yaml without `name`, under the lock, keeping its owner and mode.
fn write_without(target: &Target, name: &str) -> Result<u64, capyctl_config::ConfigError> {
    let lock = lock_engines_for(&target.engines, service_owner(target))?;
    let mut engines = EnginesFile::load(&target.engines)?;
    engines.profiles.remove(name);
    write_engines(&engines, &lock, None)
}

/// review decision I4: a remove the role took but never answered.
fn unknown_outcome(cause: &str, name: &str) -> StructuredError {
    error(
        "agent_unreachable",
        format!(
            "{cause}; the outcome is unknown: the role may still be retiring {name}. \
             Run `capyctl engine list` to see whether it is still published, and run \
             `capyctl engine remove {name}` again to finish; a retry resumes the same removal"
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // T37 (review decision I4): the CLI's bound on `remove` outlasts the
    // role's own bound on a retirement by a clear margin, so a drain that runs
    // close to its bound is answered, not reported as lost.
    #[test]
    fn remove_waits_longer_than_the_role_retires() {
        let role = capyctl_agent::host_control::RETIRE_BOUND;
        assert!(
            role >= Duration::from_secs(900 + 60),
            "covers the drain window"
        );
        assert!(
            REMOVE_REPLY >= role + Duration::from_secs(120),
            "{REMOVE_REPLY:?}"
        );
    }
}
