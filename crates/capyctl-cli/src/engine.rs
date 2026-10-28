//! ADR 0018: `capyctl engine detect|add|list|remove`, the same on a host and in
//! standalone. Detection reads metadata only; an installation runs only
//! after the operator named or picked it. ADR 0040 §10: the operations live
//! in `capyctl_agent::host_admin`, shared with the management routes; this
//! module keeps the terminal: picking a candidate and confirming a custom
//! engine.
mod target;
pub use target::{absolute, named_role_document, resolve_target, role_engines, RoleKind, Target};

use crate::grammar::{Command, DeepParkChoice, DriftChoice};
use crate::output::StructuredError;
use capyctl_agent::control_socket::{request, ControlRequest};
use capyctl_agent::engines::ScanRoots;
pub use capyctl_agent::host_admin::REMOVE_REPLY;
use capyctl_agent::host_admin::{
    self, custom_engines, declared_by_operator, engine_key, role_document, BuiltinAdd,
    BuiltinApproval, ControlSocket, CustomEngine, CustomOperation, HostAdminError, ManifestInput,
};
use capyctl_agent::installation::InstallationMeasurer;
use capyctl_config::effective::InstallationDrift;
use capyctl_config::engine_env::{is_owned, ApprovedEnv, EnvRefusal};
use capyctl_config::engine_policy::Engine;
use capyctl_config::registration::{EnginesFile, ENGINES_FILE};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const LIST_REPLY: Duration = Duration::from_secs(5);

/// ADR 0040 §11: the host a custom engine's confirmation digest is bound to
/// when the CLI previews and confirms on the box; the digest never leaves
/// this process.
const CLI_HOST: &str = "local";

fn error(code: &'static str, message: impl Into<String>) -> StructuredError {
    StructuredError {
        code,
        message: message.into(),
    }
}

impl From<HostAdminError> for StructuredError {
    fn from(refusal: HostAdminError) -> Self {
        StructuredError {
            code: refusal.code,
            message: refusal.message,
        }
    }
}

fn drift_policy(choice: DriftChoice) -> InstallationDrift {
    match choice {
        DriftChoice::Warn => InstallationDrift::Warn,
        DriftChoice::Refuse => InstallationDrift::Refuse,
    }
}

fn deep_park_asked(choice: Option<DeepParkChoice>) -> Option<bool> {
    choice.map(|choice| choice == DeepParkChoice::Enabled)
}

pub fn is_engine_command(command: &Command) -> bool {
    matches!(
        command,
        Command::EngineDetect { .. }
            | Command::EngineAdd { .. }
            | Command::EngineAddCustom { .. }
            | Command::EngineList
            | Command::EngineRemove { .. }
    )
}

/// ADR 0021, ADR 0014 §3, §8, ADR 0028 §2.1: `capyctl engine flags`, the
/// ownership table of `engine`'s options and of the engine environment,
/// taken from the tables the checks read. The policy is the same for every
/// version of a family this build supports; `version` is recorded so a saved
/// table names the installation it was read for.
pub fn flags_table(engine: Engine, version: Option<&str>) -> Value {
    json!({
        "engine": engine.name(),
        "version": version,
        "flags": capyctl_config::engine_policy::flag_table(engine),
        "env": capyctl_config::engine_env::env_table(),
        "unlisted": {
            "flags": "classed by the name's shape (code, listener or egress, path, \
                      JSON configuration need approval), otherwise ordinary",
            "env": "a deployment needs a matching security.approved_env entry; \
                    a profile may set it",
        },
    })
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
    execute_with(command, config, state_dir, env, &Default::default()).await
}

/// As [`execute_in`], also reading `CUDA_HOME` through `env`, and looking for
/// TensorFold's build toolchain in `search` (a test's own directories).
pub async fn execute_with(
    command: &Command,
    config: Option<&Path>,
    state_dir: &Path,
    env: &(dyn Fn(&str) -> Option<String> + Sync),
    search: &capyctl_config::toolchain::ToolchainSearch,
) -> Result<Value, StructuredError> {
    match command {
        Command::EngineDetect { paths } => {
            Ok(host_admin::detected(&ScanRoots::from_env(paths.clone())))
        }
        Command::EngineAdd {
            path,
            name,
            deep_park,
            drift,
            args,
            approved_options,
            approved_paths,
            env: env_flags,
            approved_env,
            allow_remote_code,
        } => {
            // ADR 0028 §2.1: refused before anything runs or is written.
            let (profile_env, approved_env) = engine_add_env(env_flags, approved_env, env)
                .map_err(|r| error("invalid_config", r.code()))?;
            let target = resolve_target(config, state_dir, env)?;
            check_socket(&target)?;
            let path = match path {
                Some(path) => path.clone(),
                None => pick()?,
            };
            let request = BuiltinAdd {
                host_id: CLI_HOST.to_owned(),
                path,
                name: name.clone(),
                deep_park: deep_park_asked(*deep_park),
                drift: drift_policy(*drift),
                args: args.clone(),
                approved_options: approved_options.clone(),
                approved_paths: approved_paths.clone(),
                env: profile_env,
                approved_env,
                remote_code: *allow_remote_code,
            };
            let toolchain = host_admin::Toolchain {
                cuda_home_env: env("CUDA_HOME"),
                search,
            };
            // ADR 0018 §1: the operator at this terminal named or picked it.
            Ok(host_admin::add_builtin(
                &target,
                &request,
                &toolchain,
                BuiltinApproval::Operator,
                &Arc::new(InstallationMeasurer::new()),
                &ControlSocket(&target.socket),
            )
            .await?)
        }
        Command::EngineAddCustom {
            executable,
            name,
            manifest,
            deep_park,
            drift,
            env: env_flags,
            approved_env,
            yes,
        } => {
            // ADR 0028 §2.1, ADR 0031 §1: refused before anything is read or
            // written.
            let (profile_env, approved_env) = engine_add_env(env_flags, approved_env, env)
                .map_err(|r| error("invalid_config", r.code()))?;
            let target = resolve_target(config, state_dir, env)?;
            check_socket(&target)?;
            let request = CustomEngine {
                host_id: CLI_HOST.to_owned(),
                operation: CustomOperation::Add,
                name: name.clone(),
                executable: executable.clone(),
                manifest: ManifestInput::read_file(manifest),
                deep_park: deep_park_asked(*deep_park),
                drift: drift_policy(*drift),
                env: profile_env,
                approved_env,
            };
            add_custom(&target, &request, *yes).await
        }
        Command::EngineList => list(&resolve_target(config, state_dir, env)?).await,
        Command::EngineRemove { name, drain } => {
            let target = resolve_target(config, state_dir, env)?;
            Ok(host_admin::remove(&target, name, *drain, &ControlSocket(&target.socket)).await?)
        }
        _ => Err(error("invalid_config", "not an engine command")),
    }
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
    let found = host_admin::candidates(&ScanRoots::from_env(vec![]));
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

/// ADR 0028 §2.1 and R9: `--env` with `CAPYCTL_ENGINE_ADD_ENV`, `--approve-env`
/// with `CAPYCTL_APPROVE_ENV`. A flag beats the variable for one name; a name
/// given twice by flag is a conflict. An owned name or a malformed approval is
/// refused here, before anything runs.
pub fn engine_add_env(
    flags: &[(String, String)],
    approvals: &[String],
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<(BTreeMap<String, String>, Vec<String>), EnvRefusal> {
    let entries =
        crate::client::engine_env_with_channels(flags, env("CAPYCTL_ENGINE_ADD_ENV").as_deref())?;
    let mut profile_env = BTreeMap::new();
    for (name, value) in entries {
        if is_owned(&name) {
            return Err(EnvRefusal::Reserved(name));
        }
        if profile_env.insert(name.clone(), value).is_some() {
            return Err(EnvRefusal::Conflict(name));
        }
    }
    // R9: approvals follow flag > env; any --approve-env replaces the variable.
    let approved: Vec<String> = if approvals.is_empty() {
        let mut from_variable: Vec<String> = Vec::new();
        for glob in env("CAPYCTL_APPROVE_ENV")
            .unwrap_or_default()
            .split(';')
            .map(str::trim)
            .filter(|glob| !glob.is_empty())
        {
            if !from_variable.iter().any(|seen| seen == glob) {
                from_variable.push(glob.to_owned());
            }
        }
        from_variable
    } else {
        approvals.to_vec()
    };
    ApprovedEnv::parse(&approved)?;
    Ok((profile_env, approved))
}


/// Found live 2026-10-03: a role cannot listen on a control socket whose
/// path a Unix socket cannot hold, so nothing is saved for one it could
/// never publish to.
fn check_socket(target: &Target) -> Result<(), StructuredError> {
    if target.socket.as_os_str().len() > capyctl_agent::control_socket::MAX_PATH {
        return Err(error(
            "invalid_config",
            format!(
                "{} is longer than the {}-byte socket path limit, so capyctl cannot run with this state directory; use a shorter --state-dir (or CAPYCTL_STATE_DIR); nothing was written",
                target.socket.display(),
                capyctl_agent::control_socket::MAX_PATH
            ),
        ));
    }
    Ok(())
}

/// ADR 0031 §1, §3, §7, §8, ADR 0040 §11: `capyctl engine add --custom`.
/// Executes nothing. The preview's summary is printed, the operator
/// confirms on a terminal (or passed `--yes`), and the confirmation, made
/// with the previewed digest, measures again and writes custom-engines.yaml
/// only if nothing changed meanwhile; then the running role is asked to
/// publish it.
async fn add_custom(
    target: &Target,
    request: &CustomEngine,
    yes: bool,
) -> Result<Value, StructuredError> {
    let measurer = Arc::new(InstallationMeasurer::new());
    let preview = host_admin::preview_custom(target, request, &measurer).await?;
    {
        use std::io::Write;
        let _ = std::io::stderr().write_all(preview.summary.as_bytes());
    }
    confirm_custom(yes)?;
    Ok(host_admin::confirm_custom(
        target,
        request,
        &preview.digest,
        &request.name,
        &measurer,
        &ControlSocket(&target.socket),
    )
    .await?)
}

/// ADR 0031 §8: `engine add --custom` asks on a terminal (stdin and stderr,
/// as [`pick`] checks); without one it needs `--yes`, else `not_interactive`.
fn confirm_custom(yes: bool) -> Result<(), StructuredError> {
    use std::io::{BufRead, IsTerminal, Write};
    if yes {
        return Ok(());
    }
    if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
        return Err(error(
            "not_interactive",
            "engine add --custom asks for confirmation on a terminal; review the summary \
             above and pass --yes to register it without one; nothing was written",
        ));
    }
    let mut stderr = std::io::stderr();
    let _ = write!(stderr, "register this custom engine? [y/N] ");
    let _ = stderr.flush();
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|_| error("not_interactive", "no answer; nothing was written"))?;
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Ok(()),
        _ => Err(error("declined", "not registered; nothing was written")),
    }
}

async fn list(target: &Target) -> Result<Value, StructuredError> {
    let engines =
        EnginesFile::load(&target.engines).map_err(|e| error("invalid_config", e.detail))?;
    // ADR 0031 amendment A1: custom engines are registered in their own file.
    let custom = EnginesFile::load(&custom_engines(target))
        .map_err(|e| error("invalid_config", e.detail))?;
    let role = request(&target.socket, &ControlRequest::List, LIST_REPLY)
        .await
        .ok()
        .filter(|r| r["ok"] == true);
    // ADR 0018 §2: registered profiles, then the ones the operator declared.
    let mut all: Vec<(String, Value, &'static str)> = engines
        .profiles
        .clone()
        .into_iter()
        .map(|(n, p)| (n, p, ENGINES_FILE))
        .chain(
            custom
                .profiles
                .clone()
                .into_iter()
                .map(|(n, p)| (n, p, capyctl_config::registration::CUSTOM_ENGINES_FILE)),
        )
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
    let profiles: Vec<Value> = all.iter().map(|(_, profile, _)| profile.clone()).collect();
    let mut rows: Vec<Value> = all
        .into_iter()
        .map(|(name, profile, source)| {
            let kind = profile["engine"].as_str().and_then(Engine::from_name);
            let engine = kind.unwrap_or(Engine::Vllm);
            let accepted = role.as_ref().map(|r| r["accepted"].get(&name).cloned());
            let reported = accepted
                .clone()
                .flatten()
                .and_then(|a| a["installation"]["version"].as_str().map(str::to_owned));
            let version = capyctl_config::registration::listed_version(
                kind,
                reported.as_deref(),
                profile["build_fingerprint"].as_str(),
            );
            // ADR 0031 §6, §7: a custom engine's idle signal (or `none`) and
            // whether its launches carry a per-launch engine key.
            let manifest = capyctl_agent::installation::custom_manifest(&profile);
            json!({
                "profile": name, "source": source, "engine": profile["engine"], "version": version,
                "custom": !capyctl_config::registration::is_verified(engine, &version),
                "idle_signal": manifest.as_ref().map(|m| m.idle.as_ref().map_or_else(|| "none".to_owned(), |idle| idle.describe())),
                "engine_key": manifest.as_ref().map(engine_key),
                "api": manifest.as_ref().map(|m| m.api),
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
    // ADR 0033 §9: the host's `lmcache` and `retention` as the running role
    // reports them (what it was started with, overrides included), else as
    // its role document says.
    let host = role
        .as_ref()
        .and_then(running_host_settings)
        .unwrap_or_else(|| {
            role_document(target)
                .ok()
                .map(|document| match target.kind {
                    RoleKind::Host => capyctl_config::lmcache::host_settings(&document),
                    RoleKind::Standalone => {
                        capyctl_config::lmcache::host_settings(&document["host"])
                    }
                })
                .unwrap_or_default()
        });
    let inputs: Vec<(String, Value)> = rows
        .iter()
        .map(|row| row["version"].as_str().unwrap_or_default().to_owned())
        .zip(profiles)
        .collect();
    let views = tokio::task::spawn_blocking(move || {
        let measurer = capyctl_agent::installation::InstallationMeasurer::new();
        inputs
            .iter()
            .map(|(version, profile)| listed_lmcache(&measurer, host, version, profile))
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|_| error("internal", "the LMCache check failed"))?;
    for (row, view) in rows.iter_mut().zip(views) {
        row["lmcache"] = view;
    }
    Ok(
        json!({"engines_file": target.engines, "revision": engines.revision,
        "custom_engines_file": custom.path, "custom_revision": custom.revision,
        "agent": if role.is_some() { "reachable" } else { "unreachable" }, "engines": rows}),
    )
}

/// ADR 0033 §9: the `lmcache` and `retention` a running role's `list` reply
/// reports it runs under; `None` from a role that does not report them.
fn running_host_settings(
    reply: &Value,
) -> Option<(
    capyctl_config::lmcache::HostLmcache,
    capyctl_config::lmcache::Retention,
)> {
    reply["host"]
        .is_object()
        .then(|| capyctl_config::lmcache::host_settings(&reply["host"]))
}

/// ADR 0033 §9: a local profile's `lmcache` field: off with the first reason
/// of the host's settings and the engine gate, else the launch-time check
/// run on the profile's environment (files only, nothing runs), showing the
/// registered version and whether the files drifted from it.
fn listed_lmcache(
    measurer: &capyctl_agent::installation::InstallationMeasurer,
    host: (
        capyctl_config::lmcache::HostLmcache,
        capyctl_config::lmcache::Retention,
    ),
    version: &str,
    profile: &Value,
) -> Value {
    use capyctl_config::lmcache::{listed_view, profile_gate, LmcacheOff, LmcacheOffReason};
    let off =
        |reason: LmcacheOffReason, detail: String| listed_view(&Err(LmcacheOff { reason, detail }));
    let Some(engine) = profile["engine"].as_str().and_then(Engine::from_name) else {
        let reason = LmcacheOffReason::EngineUnsupported;
        return off(reason, reason.sentence().to_owned());
    };
    if let Some(gated) = profile_gate(host, engine, version) {
        return listed_view(&Err(gated));
    }
    let registration: Option<capyctl_config::lmcache::LmcacheRegistration> =
        serde_json::from_value(profile["lmcache"].clone()).ok();
    let Some(executable) = profile["executable"].as_str() else {
        return listed_view(&capyctl_config::lmcache::listed(
            host,
            engine,
            version,
            registration.as_ref(),
        ));
    };
    match capyctl_agent::lmcache_install::check_lmcache(
        measurer,
        engine,
        Path::new(executable),
        registration.as_ref(),
    ) {
        Ok(install) => {
            let registered = registration.map_or(install.version, |r| r.version);
            let mut view = listed_view(&Ok(registered));
            view["drift"] = install.drifted.into();
            view
        }
        Err(failure) => off(failure.reason, failure.detail),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // T45 (ADR 0033 §§2, 9, 10): before any file is read, a profile is off
    // for the host's zero retention, then the host's `lmcache: off`, then
    // its engine and version, in that order; nothing of the environment is
    // read for them (the executable does not exist).
    #[test]
    fn lmcache_listing_follows_the_host_settings_and_the_engine() {
        use capyctl_config::lmcache::{HostLmcache, Retention};
        let measurer = capyctl_agent::installation::InstallationMeasurer::new();
        let vllm = json!({"engine": "vllm", "executable": "/nonexistent/bin/vllm",
            "lmcache": {"version": "0.5.6", "digest": "sha256:00"}});
        let reason = |host, version: &str, profile: &Value| {
            listed_lmcache(&measurer, host, version, profile)["reason"].clone()
        };
        let defaults = (HostLmcache::Auto, Retention::Standard);
        assert_eq!(
            reason((HostLmcache::Off, Retention::Zero), "0.32.0", &vllm),
            "retention_zero"
        );
        assert_eq!(
            reason((HostLmcache::Off, Retention::Standard), "0.32.0", &vllm),
            "host_off"
        );
        assert_eq!(
            reason((HostLmcache::Auto, Retention::Zero), "0.31.0", &vllm),
            "retention_zero"
        );
        assert_eq!(
            reason(defaults, "0.31.0", &vllm),
            "engine_version_unsupported"
        );
        let sglang = json!({"engine": "sglang", "executable": "/nonexistent/bin/python3"});
        assert_eq!(
            reason(defaults, "0.5.21", &sglang),
            "engine_version_unsupported"
        );
        assert_eq!(
            listed_lmcache(&measurer, defaults, "0.5.21", &sglang)["detail"],
            "LMCache needs SGLang 0.5.22 or newer"
        );
        for engine in ["llamacpp", "tensorfold", "custom", "unknown"] {
            let profile = json!({"engine": engine, "executable": "/nonexistent/x"});
            assert_eq!(reason(defaults, "1.0.0", &profile), "engine_unsupported");
        }
        // The environment is read only past those gates.
        assert_eq!(reason(defaults, "0.32.0", &vllm), "not_installed");
        assert_eq!(
            capyctl_config::lmcache::host_settings(&json!({"lmcache": "off", "retention": "zero"})),
            (HostLmcache::Off, Retention::Zero)
        );
        assert_eq!(
            capyctl_config::lmcache::host_settings(&json!({"lmcache": "sometimes"})),
            defaults
        );
    }

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

    // T45 (ADR 0033 §9; finding 9, live 2026-10-22): `engine list` shows
    // LMCache off with its reason, not the registered version, under the
    // `lmcache` and `retention` the running role reports it runs under; a
    // reply that reports none falls back to the role document.
    #[test]
    fn lmcache_listing_follows_the_running_roles_settings() {
        use capyctl_config::lmcache::{HostLmcache, Retention};
        let measurer = capyctl_agent::installation::InstallationMeasurer::new();
        let vllm = json!({"engine": "vllm", "executable": "/nonexistent/bin/vllm",
            "lmcache": {"version": "0.5.6", "digest": "sha256:00"}});
        for (reported, reason) in [
            (
                json!({"lmcache": "off", "retention": "standard"}),
                "host_off",
            ),
            (
                json!({"lmcache": "auto", "retention": "zero"}),
                "retention_zero",
            ),
        ] {
            let reply = json!({"ok": true, "accepted": {}, "host": reported});
            let host = running_host_settings(&reply).expect("reported");
            let view = listed_lmcache(&measurer, host, "0.32.0", &vllm);
            assert_eq!(view["state"], "off", "{view}");
            assert_eq!(view["reason"], reason, "{view}");
            assert!(view.get("version").is_none(), "{view}");
        }
        assert_eq!(
            running_host_settings(&json!({"ok": true, "host": {}})),
            Some((HostLmcache::Auto, Retention::Standard))
        );
        assert_eq!(running_host_settings(&json!({"ok": true})), None);
    }
}
