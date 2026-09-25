//! ADR 0018: `mllm engine detect|add|list|remove`, the same on a host and in
//! standalone. Detection reads metadata only; an installation runs only
//! after the operator named or picked it.
mod target;
pub use target::{resolve_target, RoleKind, Target};

use crate::grammar::{Command, DeepParkChoice, DriftChoice};
use crate::output::StructuredError;
use mllm_agent::control_socket::{request, ControlRequest};
use mllm_agent::engines::{
    check_version, detect, resolve, Resolved, ScanBounds, ScanRoots, VERSION_CHECK_TIMEOUT,
};
use mllm_config::effective::InstallationDrift;
use mllm_config::engine_policy::Engine;
use mllm_config::registration::{
    check_profile, lock_engines, profile_document, valid_profile_name, write_engines, EnginesFile,
    ProfileSpec, ENVIRONMENT_PROFILES,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

const ADD_REPLY: Duration = Duration::from_secs(60);
const REMOVE_REPLY: Duration = Duration::from_secs(990);
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

fn engine_name(engine: Engine) -> &'static str {
    match engine {
        Engine::Vllm => "vllm",
        Engine::Sglang => "sglang",
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

/// As [`execute`], reading `HOME`, `XDG_CONFIG_HOME` and `MLLM_CONFIG` through
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

fn candidates(paths: &[PathBuf]) -> Vec<mllm_agent::engines::Candidate> {
    detect(&ScanRoots::from_env(paths.to_vec()), &ScanBounds::default())
}

fn detected(paths: &[PathBuf]) -> Value {
    let rows: Vec<Value> = candidates(paths)
        .into_iter()
        .map(|c| {
            json!({"engine": engine_name(c.engine), "version": c.version, "custom": c.custom,
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
            engine_name(c.engine),
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
    fingerprint: Option<mllm_agent::installation::InstallationFingerprint>,
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
    let fingerprint = mllm_agent::installation::InstallationMeasurer::new()
        .measure(resolved.engine, &resolved.executable)
        .ok();
    // Controller ruling 2026-09-25: `engine add` before any role has started
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
    let report = mllm_agent::installation::probe_capabilities(
        resolved.engine,
        &resolved.executable,
        &runtime,
        mllm_agent::installation::PROBE_TIMEOUT,
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
        RoleKind::Host => mllm_config::ConfigKind::Host,
        RoleKind::Standalone => mllm_config::ConfigKind::Standalone,
    };
    mllm_config::parse_strict(kind, &text)
        .map_err(|e| error("invalid_config", format!("{}: {}", e.path, e.detail)))
}

/// Profiles the operator declared in the role document itself.
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
    let lock = lock_engines(&target.engines).map_err(|e| error("internal", e.detail))?;
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
    let name = name.unwrap_or(engine_name(resolved.engine)).to_owned();
    if !valid_profile_name(&name) {
        return Err(error(
            "invalid_config",
            format!("profile name {name:?} must be lowercase letters, digits, '-' or '_'"),
        ));
    }
    if target.kind == RoleKind::Standalone && ENVIRONMENT_PROFILES.contains(&name.as_str()) {
        return Err(error("profile_exists", format!("{name} is reserved for the MLLM_VLLM_BIN / MLLM_SGLANG_BIN installation; use --name")));
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
        None => registration.deep_park_missing != Some(true),
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
    };
    let revision = write_profile(target, &name, &spec)?;
    let mut out = json!({
        "profile": name, "engine": engine_name(resolved.engine), "version": registration.version,
        "custom": resolved.custom(), "executable": resolved.executable,
        "fingerprint": registration.fingerprint.map(|f| json!({"version": f.version, "digest": f.digest})),
        "deep_park": if deep { "enabled" } else { "disabled" }, "deep_park_probe": probe,
        "engines_file": target.engines, "revision": revision,
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
    let rows: Vec<Value> = all
        .into_iter()
        .map(|(name, profile, source)| {
            let engine = match profile["engine"].as_str() { Some("sglang") => Engine::Sglang, _ => Engine::Vllm };
            let accepted = role.as_ref().map(|r| r["accepted"].get(&name).cloned());
            let version = accepted
                .clone()
                .flatten()
                .and_then(|a| a["installation"]["version"].as_str().map(str::to_owned))
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| profile["build_fingerprint"].as_str().unwrap_or("unknown").to_owned());
            json!({
                "profile": name, "source": source, "engine": profile["engine"], "version": version,
                "custom": !mllm_config::registration::is_verified(engine, &version),
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

async fn remove(target: &Target, name: &str, drain: bool) -> Result<Value, StructuredError> {
    if target.kind == RoleKind::Standalone && ENVIRONMENT_PROFILES.contains(&name) {
        return Err(error("invalid_config", format!(
            "{name} comes from MLLM_VLLM_BIN / MLLM_SGLANG_BIN; unset the variable and restart the role instead"
        )));
    }
    let engines =
        EnginesFile::load(&target.engines).map_err(|e| error("invalid_config", e.detail))?;
    if !engines.profiles.contains_key(name) {
        // ADR 0018 §2: a profile declared in the role document is the operator's.
        return Err(error(
            "invalid_config",
            if declared_by_operator(target)?.contains_key(name) {
                format!(
                    "{name} is declared in {}; edit that file and restart the role",
                    target.role_document.display()
                )
            } else {
                format!("{} has no profile named {name}", target.engines.display())
            },
        ));
    }
    match request(
        &target.socket,
        &ControlRequest::Remove {
            profile: name.into(),
            drain,
        },
        REMOVE_REPLY,
    )
    .await
    {
        Ok(reply) if reply["ok"] == true => Ok(reply),
        Ok(reply) => {
            let mut message = reply["message"].as_str().unwrap_or("refused").to_owned();
            if let Some(names) = reply["deployments"].as_array().filter(|n| !n.is_empty()) {
                let names: Vec<&str> = names.iter().filter_map(Value::as_str).collect();
                message = format!("{message}: {}", names.join(", "));
            }
            Err(error(closed(reply["code"].as_str().unwrap_or("")), message))
        }
        // Owner decision 2026-09-25: a published profile is never removed unconfirmed.
        Err(e) => Err(error(
            "agent_unreachable",
            format!("{e}; nothing was removed; start the role and retry"),
        )),
    }
}
