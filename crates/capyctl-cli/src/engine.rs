//! ADR 0018: `capyctl engine detect|add|list|remove`, the same on a host and in
//! standalone. Detection reads metadata only; an installation runs only
//! after the operator named or picked it.
mod target;
pub use target::{absolute, named_role_document, resolve_target, role_engines, RoleKind, Target};

use crate::grammar::{Command, DeepParkChoice, DriftChoice};
use crate::output::StructuredError;
use capyctl_agent::control_socket::{request, ClientError, ControlRequest};
use capyctl_agent::engines::{
    check_version, detect, resolve, resolve_custom, Resolved, ScanBounds, ScanRoots,
    VERSION_CHECK_TIMEOUT,
};
use capyctl_config::custom::{CustomManifest, CustomProfile, MANIFEST_INVALID, MAX_MANIFEST_BYTES};
use capyctl_config::effective::InstallationDrift;
use capyctl_config::engine_env::{is_owned, ApprovedEnv, EnvRefusal};
use capyctl_config::engine_policy::Engine;
use capyctl_config::registration::{
    check_profile, custom_engines_beside, custom_profile_document, lock_engines_for,
    profile_document, put_companions, valid_profile_name, write_engines, CustomProfileSpec,
    EnginesFile, ProfileSpec, ENGINES_FILE, ENVIRONMENT_PROFILES,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
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
        Command::EngineDetect { paths } => Ok(detected(paths)),
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
            add(
                &target,
                path.as_deref(),
                name.as_deref(),
                *deep_park,
                *drift,
                Extras {
                    args,
                    options: approved_options,
                    paths: approved_paths,
                    env: profile_env,
                    approved_env,
                    remote_code: *allow_remote_code,
                },
                Toolchain {
                    cuda_home_env: env("CUDA_HOME"),
                    search,
                },
            )
            .await
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
            add_custom(
                &target,
                CustomAdd {
                    executable,
                    name,
                    manifest,
                    deep_park: *deep_park,
                    drift: *drift,
                    env: profile_env,
                    approved_env,
                    yes: *yes,
                },
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
    reported: capyctl_agent::engines::ReportedVersion,
    fingerprint: Option<capyctl_agent::installation::InstallationFingerprint>,
    deep_park_missing: Option<bool>,
    /// ADR 0033 §1: the LMCache of a vLLM or SGLang environment.
    lmcache: Option<capyctl_agent::lmcache_install::LmcacheMeasurement>,
}

/// ADR 0018 §1 step 3: executed only now, because the operator named it.
fn register(resolved: &Resolved, state_dir: &Path) -> Result<Registration, StructuredError> {
    let reported = check_version(resolved, VERSION_CHECK_TIMEOUT).map_err(|e| {
        error(
            e.code(),
            format!(
                "{}: {e}; nothing was written",
                resolved.executable.display()
            ),
        )
    })?;
    let measurer = capyctl_agent::installation::InstallationMeasurer::new();
    let fingerprint = measurer.measure(resolved.engine, &resolved.executable).ok();
    // ADR 0033 §1: LMCache's version and digest, read from the environment's
    // files (nothing runs); recorded only when found and measurable, and
    // never installed by capyctl.
    let lmcache = match resolved.engine {
        Engine::Vllm | Engine::Sglang => {
            capyctl_agent::lmcache_install::measure_lmcache(&measurer, &resolved.executable)
                .ok()
                .flatten()
        }
        Engine::Tensorfold | Engine::Llamacpp | Engine::TensorrtLlm | Engine::Custom => None,
    };
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
        reported,
        fingerprint,
        deep_park_missing: report.and_then(|r| r.available("deep_park")).map(|a| !a),
        lmcache,
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

/// ADR 0031 amendment A1: custom-engines.yaml beside the target's engines.yaml.
fn custom_engines(target: &Target) -> std::path::PathBuf {
    custom_engines_beside(&target.engines)
}

/// The capyctl-owned file `name` is registered in (engines.yaml or, ADR 0031
/// amendment A1, custom-engines.yaml), if any.
fn registered_in(target: &Target, name: &str) -> Result<Option<EnginesFile>, StructuredError> {
    for path in [target.engines.clone(), custom_engines(target)] {
        let file = EnginesFile::load(&path).map_err(|e| error("invalid_config", e.detail))?;
        if file.profiles.contains_key(name) {
            return Ok(Some(file));
        }
    }
    Ok(None)
}

/// Whether `name` is registered in engines.yaml or custom-engines.yaml, or
/// declared in the role document (an older host may have registered a
/// profile named `local`).
fn registered_or_declared(target: &Target, name: &str) -> Result<bool, StructuredError> {
    Ok(registered_in(target, name)?.is_some() || declared_by_operator(target)?.contains_key(name))
}

/// ADR 0018 §1, ADR 0031 §1: `profile_exists`, across both capyctl-owned
/// files and the role document.
fn refuse_existing(target: &Target, name: &str) -> Result<(), StructuredError> {
    if registered_or_declared(target, name)? {
        return Err(error(
            "profile_exists",
            format!("profile {name} exists; use --name, or remove it first"),
        ));
    }
    Ok(())
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
/// ADR 0038 §2: `companions` are the versions TensorRT-LLM's check read.
fn write_profile(
    target: &Target,
    name: &str,
    spec: &ProfileSpec,
    companions: &BTreeMap<String, String>,
) -> Result<u64, StructuredError> {
    let lock = lock_engines_for(&target.engines, service_owner(target))
        .map_err(|e| error("internal", format!("{}: {}", e.path, e.detail)))?;
    let mut engines =
        EnginesFile::load(&target.engines).map_err(|e| error("invalid_config", e.detail))?;
    refuse_existing(target, name)?;
    let mut profile = profile_document(spec);
    put_companions(&mut profile, companions);
    check_profile(name, &profile)
        .map_err(|e| error("invalid_config", format!("{}: {}", e.path, e.detail)))?;
    engines.profiles.insert(name.to_owned(), profile);
    let host = match target.kind {
        RoleKind::Host => Some(role_document(target)?),
        RoleKind::Standalone => None,
    };
    write_engines(&engines, &lock, host.as_ref()).map_err(|e| error("invalid_config", e.detail))
}

/// What `engine add` writes beside the installation: host-fixed args
/// (`--arg`) and, ADR 0014 §8, what it approves (`--approve-option`,
/// `--approve-path`, `--allow-remote-code`).
struct Extras<'a> {
    args: &'a [String],
    options: &'a [String],
    paths: &'a [PathBuf],
    /// ADR 0028 §2.1: `--env` and `--approve-env`, merged with their variables.
    env: BTreeMap<String, String>,
    approved_env: Vec<String>,
    /// Spec §3: `security.trust_remote_code`.
    remote_code: bool,
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

/// Where `engine add` looks for TensorFold's build toolchain.
struct Toolchain<'a> {
    cuda_home_env: Option<String>,
    search: &'a capyctl_config::toolchain::ToolchainSearch,
}

async fn add(
    target: &Target,
    path: Option<&Path>,
    name: Option<&str>,
    deep_park: Option<DeepParkChoice>,
    drift: DriftChoice,
    extras: Extras<'_>,
    toolchain: Toolchain<'_>,
) -> Result<Value, StructuredError> {
    check_socket(target)?;
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
        return Err(error("profile_exists", format!("{name} is reserved for the role's own installation (--vllm-bin / --sglang-bin / --tensorfold-bin / --llamacpp-bin / --tensorrt-llm-bin, CAPYCTL_VLLM_BIN / CAPYCTL_SGLANG_BIN / CAPYCTL_TENSORFOLD_BIN / CAPYCTL_LLAMACPP_BIN / CAPYCTL_TENSORRT_LLM_BIN or local_engine); use --name")));
    }
    // Checked before anything runs, and again under the lock when writing.
    refuse_existing(target, &name)?;
    // SPEC §13.3 amendment (owner decision 2026-09-25). ADR 0029 §2: nothing
    // compiles at run time for llama.cpp, so no toolkit is recorded for it.
    // ADR 0038 §2: TensorRT-LLM records one as the other Python engines do;
    // nothing about it is checked here.
    let cuda_home = match resolved.engine {
        // ADR 0031 §1: `--custom` registers through its own path.
        Engine::Llamacpp | Engine::Custom => None,
        Engine::Vllm | Engine::Sglang | Engine::Tensorfold | Engine::TensorrtLlm => {
            capyctl_config::registration::detect_cuda_home_in(
                toolchain.cuda_home_env.as_deref(),
                &toolchain.search.default_cuda_home,
                |nvcc| nvcc.is_file(),
            )
        }
    };
    // ADR 0023 §4, ADR 0029 §5: TensorFold and llama.cpp have no remote-code
    // option, so a deployment on them may never set trust_remote_code: true;
    // allowing it would approve nothing.
    if extras.remote_code {
        if let Some(family) = match resolved.engine {
            Engine::Tensorfold => Some("TensorFold"),
            Engine::Llamacpp => Some("llama.cpp"),
            Engine::Vllm | Engine::Sglang | Engine::TensorrtLlm => None,
            // ADR 0031 §1: `--custom` registers through its own path.
            Engine::Custom => Some("a custom engine"),
        } {
            return Err(error(
                "capability_missing",
                format!(
                    "{family} runs no code shipped with a checkpoint, so --allow-remote-code \
                     is refused and nothing was written"
                ),
            ));
        }
    }
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
        capyctl_config::toolchain::check(bin, cuda_home.as_deref(), &toolchain.search.system)
            .map_err(|missing| {
                error(
                    "toolchain_missing",
                    format!("{}; nothing was written", missing.for_engine_add()),
                )
            })?;
    }
    if resolved.engine == Engine::Llamacpp {
        // ADR 0029 §2: llama.cpp has no deep-park capability, and a
        // machine-wide config.ini would set options CapyCTL cannot see; both
        // are refused before anything runs or is written.
        if deep_park == Some(DeepParkChoice::Enabled) {
            return Err(error(
                "capability_missing",
                "llama.cpp has no on-demand sleep or wake; it runs restart_only, so \
                 --deep-park enabled is refused and nothing was written",
            ));
        }
        if let Some(refusal) =
            capyctl_config::llamacpp::system_config_refusal(&toolchain.search.system_root)
        {
            return Err(error(
                "engine_unsupported",
                format!("{refusal}; nothing was written"),
            ));
        }
    }
    // ADR 0038 §2, §8: TensorRT-LLM is restart-only; a request to park is
    // refused before anything runs or is written. Nothing about its toolchain
    // is checked: the first start's kernel builds are ADR 0014 A12's.
    if resolved.engine == Engine::TensorrtLlm && deep_park == Some(DeepParkChoice::Enabled) {
        return Err(error(
            "capability_missing",
            "TensorRT-LLM's sleep mode is a prototype its server does not expose; it runs \
             restart_only, so --deep-park enabled is refused and nothing was written",
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
        // ADR 0023 §2, ADR 0029 §2, ADR 0038 §2: TensorFold, llama.cpp and
        // TensorRT-LLM are disabled even when the probe could not run.
        None => {
            !matches!(
                resolved.engine,
                Engine::Tensorfold | Engine::Llamacpp | Engine::TensorrtLlm
            ) && registration.deep_park_missing != Some(true)
        }
    };
    let spec = ProfileSpec {
        engine: resolved.engine,
        executable: resolved.executable.clone(),
        // ADR 0029 §2: llama.cpp's is `<version>+<commit>`.
        build_fingerprint: registration.reported.build_fingerprint.clone(),
        deep_park: deep,
        installation_drift: match drift {
            DriftChoice::Warn => InstallationDrift::Warn,
            DriftChoice::Refuse => InstallationDrift::Refuse,
        },
        args: extras.args.to_vec(),
        env: extras.env.clone(),
        approved_env: extras.approved_env.clone(),
        cuda_home,
        approved_options: extras.options.to_vec(),
        approved_paths: extras
            .paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
        trust_remote_code: extras.remote_code,
        lmcache: registration
            .lmcache
            .as_ref()
            .map(capyctl_agent::lmcache_install::LmcacheMeasurement::registration),
    };
    let companions = &registration.reported.companions;
    let revision = write_profile(target, &name, &spec, companions)?;
    // ADR 0038 §2: TensorRT-LLM's version counts beside its torch build;
    // anything else is an uncertified custom setup, written all the same.
    let custom = !capyctl_config::registration::is_verified_with_torch(
        resolved.engine,
        &spec.build_fingerprint,
        companions
            .get(capyctl_config::tensorrt_llm::TORCH)
            .map(String::as_str),
    );
    let mut out = json!({
        "profile": name, "engine": resolved.engine.name(),
        "version": registration.reported.version,
        "build_fingerprint": spec.build_fingerprint,
        "custom": custom,
        "executable": resolved.executable,
        "fingerprint": registration.fingerprint.map(|f| json!({"version": f.version, "digest": f.digest})),
        "deep_park": if deep { "enabled" } else { "disabled" }, "deep_park_probe": probe,
        "engines_file": target.engines, "revision": revision,
        "cuda_home": spec.cuda_home,
        "trust_remote_code": spec.trust_remote_code,
        // ADR 0033 §1: what was recorded, or null when LMCache was not found.
        "lmcache": spec.lmcache,
    });
    // ADR 0038 §2: the companion versions recorded beside TensorRT-LLM's.
    if resolved.engine == Engine::TensorrtLlm {
        out["companions"] = json!(companions);
    }
    publish(target, out, revision, &target.engines).await
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

/// ADR 0018 §3: ask the running role to publish what `engine add` wrote to
/// `written` at `revision`; `out` is the command's result.
async fn publish(
    target: &Target,
    mut out: Value,
    revision: u64,
    written: &Path,
) -> Result<Value, StructuredError> {
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
            // Release live check 0.1.1: a role started with another
            // --state-dir or --config listens on another socket, so it looks
            // absent here. Say which socket was tried and how to reach it.
            out["notice"] = json!(format!(
                "saved to {} (revision {revision}); start capyctl (`{}`) to use it\n\
                 no capyctl answered on {}; if it is already running with another \
                 --state-dir or --config, restart it to publish this profile, and pass \
                 the same option to `capyctl engine`",
                written.display(),
                match target.kind {
                    RoleKind::Standalone => "capyctl start standalone",
                    RoleKind::Host => "capyctl start host",
                },
                target.socket.display()
            ));
            Ok(out)
        }
        // A role is there but did not take or answer the request: that is a
        // fault the operator must look at.
        Err(e) => Err(error(
            "agent_unreachable",
            format!(
                "{e}; {} is written (revision {revision}); it takes effect when the role starts",
                written.display()
            ),
        )),
    }
}

/// ADR 0031 §1: what `engine add --custom` was asked to register.
struct CustomAdd<'a> {
    executable: &'a Path,
    name: &'a str,
    manifest: &'a Path,
    deep_park: Option<DeepParkChoice>,
    drift: DriftChoice,
    /// ADR 0028 §2.1: `--env` and `--approve-env`, merged with their variables.
    env: BTreeMap<String, String>,
    approved_env: Vec<String>,
    /// ADR 0031 §8: `--yes`.
    yes: bool,
}

/// ADR 0031 §7: what `engine key` reads for a custom engine.
fn engine_key(manifest: &CustomManifest) -> &'static str {
    if manifest.uses_api_key() {
        "per-launch"
    } else {
        "none"
    }
}

/// ADR 0031 §7: the known limitation of an engine without a key.
const NO_KEY_CAVEAT: &str =
    "the engine listens unauthenticated on loopback; any local process can reach its port";

/// ADR 0031 §2, §10: the manifest file, read once: at most 64 KiB of UTF-8,
/// validated. `custom_manifest_invalid` (exit 27), or
/// `engine_env_reserved:<NAME>` for an `env` name CapyCTL owns.
fn read_manifest(path: &Path) -> Result<CustomManifest, StructuredError> {
    use std::io::Read;
    let invalid = |detail: String| {
        error(
            MANIFEST_INVALID,
            format!("{}: {detail}; nothing was written", path.display()),
        )
    };
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| {
            file.take(MAX_MANIFEST_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
        })
        .map_err(|e| invalid(format!("the manifest cannot be read: {e}")))?;
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(invalid(format!(
            "a manifest is at most 64 KiB ({MAX_MANIFEST_BYTES} bytes)"
        )));
    }
    let text =
        String::from_utf8(bytes).map_err(|_| invalid("the manifest is not UTF-8 text".into()))?;
    let manifest = CustomManifest::parse(&text).map_err(|e| {
        if e.code == MANIFEST_INVALID {
            invalid(e.detail.clone())
        } else {
            error(
                "invalid_config",
                format!("{}: {e}; nothing was written", path.display()),
            )
        }
    })?;
    // ADR 0031 §3, §10: `environment` names an existing absolute directory.
    if let Some(root) = &manifest.environment {
        if !std::fs::metadata(root).is_ok_and(|metadata| metadata.is_dir()) {
            return Err(invalid(format!(
                "`environment` {root} is not an existing directory"
            )));
        }
    }
    Ok(manifest)
}

/// ADR 0031 §1, §3, §7, §8: `capyctl engine add --custom`. Executes nothing:
/// the executable is resolved and measured from its files, the manifest is
/// read once and copied with its digest into custom-engines.yaml (amendment
/// A1) once the operator confirms (or passed `--yes`), and the running role
/// is asked to publish it.
async fn add_custom(target: &Target, request: CustomAdd<'_>) -> Result<Value, StructuredError> {
    check_socket(target)?;
    // ADR 0031 §4: a custom engine never parks.
    if request.deep_park == Some(DeepParkChoice::Enabled) {
        return Err(error(
            "capability_missing",
            "a custom engine has no sleep or wake CapyCTL can drive; it runs restart_only, \
             so --deep-park enabled is refused and nothing was written",
        ));
    }
    let name = request.name;
    if !valid_profile_name(name) {
        return Err(error(
            "invalid_config",
            format!("profile name {name:?} must be lowercase letters, digits, '-' or '_'"),
        ));
    }
    if ENVIRONMENT_PROFILES.contains(&name) {
        return Err(error(
            "profile_exists",
            format!("{name} is reserved for the role's own installation; use another --name"),
        ));
    }
    let executable = resolve_custom(request.executable)
        .map_err(|e| error(e.code(), format!("{e}; nothing was written")))?;
    let manifest = read_manifest(request.manifest)?;
    refuse_existing(target, name)?;
    // ADR 0031 §3: the installation's identity, from its files only.
    let (measured, layout) = {
        let (executable, manifest) = (executable.clone(), manifest.clone());
        tokio::task::spawn_blocking(move || {
            let layout =
                capyctl_agent::installation::custom_installation(&executable, Some(&manifest));
            let measured = capyctl_agent::installation::InstallationMeasurer::new()
                .measure_profile(Engine::Custom, &executable, Some(&manifest));
            (measured, layout)
        })
        .await
        .map_err(|_| error("internal", "measuring the installation failed"))?
    };
    // ADR 0031 §8: the digest the summary shows is the one pinned; the role
    // registers against it and never adopts a different measurement.
    let mut custom = CustomProfile::new(manifest.clone());
    custom.installation_digest = measured.as_ref().ok().map(|f| f.digest.clone());
    let spec = CustomProfileSpec {
        executable: executable.clone(),
        custom,
        installation_drift: match request.drift {
            DriftChoice::Warn => InstallationDrift::Warn,
            DriftChoice::Refuse => InstallationDrift::Refuse,
        },
        env: request.env,
        approved_env: request.approved_env,
    };
    let profile = custom_profile_document(&spec);
    // SPEC §15.3: validated as resolution reads it, before anything is asked
    // or written.
    check_profile(name, &profile).map_err(|e| {
        error(
            "invalid_config",
            format!("{}: {}; nothing was written", e.path, e.detail),
        )
    })?;
    let environment: Vec<String> = manifest
        .env
        .keys()
        .map(|key| format!("{key} (manifest)"))
        .chain(spec.env.keys().map(|key| format!("{key} (profile)")))
        .collect();
    let root = layout.root.as_ref().map(|root| {
        format!(
            "{} ({})",
            root.display(),
            if layout.declared_root {
                "the manifest's environment"
            } else {
                "virtual environment"
            }
        )
    });
    let installation = match &measured {
        Ok(f) => format!("{} ({} files, {} bytes)", f.digest, f.files, f.bytes),
        Err(_) => capyctl_agent::installation::UNMEASURED.to_owned(),
    };
    let key = engine_key(&manifest);
    let idle = manifest.idle.as_ref().map(|idle| idle.describe());
    let written = custom_engines(target);
    let mut out = json!({
        "profile": name, "engine": Engine::Custom.name(),
        "version": manifest.build_fingerprint(),
        "build_fingerprint": manifest.build_fingerprint(),
        "custom": true,
        "executable": executable,
        "args": manifest.args,
        "api": manifest.api,
        "environment": environment,
        "approved_env": spec.approved_env,
        "root": root,
        "interpreter": layout.interpreter,
        "installation": installation,
        "fingerprint": measured.as_ref().ok().map(|f| json!({"version": f.version, "digest": f.digest, "files": f.files, "bytes": f.bytes})),
        "manifest_digest": spec.custom.manifest_digest,
        "idle_signal": idle.clone().unwrap_or_else(|| "none".into()),
        "engine_key": key,
        "installation_drift": match spec.installation_drift {
            InstallationDrift::Warn => "warn",
            InstallationDrift::Refuse => "refuse",
        },
        "deep_park": "disabled",
        "engines_file": written,
    });
    // ADR 0031 §8: the approval is this summary, confirmed on a terminal.
    let mut summary = format!(
        "custom engine {name}\n  executable:   {}\n  arguments:    {}\n  api:          {}\n  version:      {}\n  environment:  {}\n  approved env: {}\n  pinned root:  {}\n  interpreter:  {}\n  installation: {installation}\n  idle signal:  {}\n  engine key:   {}\n  drift:        {}\n",
        executable.display(),
        manifest.args.join(" "),
        out["api"].as_str().unwrap_or_default(),
        manifest.build_fingerprint(),
        if environment.is_empty() { "none".into() } else { environment.join(", ") },
        if spec.approved_env.is_empty() { "none".into() } else { spec.approved_env.join(", ") },
        root.as_deref().unwrap_or("the executable alone"),
        layout
            .interpreter
            .as_ref()
            .map_or_else(|| "none".into(), |path| path.display().to_string()),
        idle.as_deref().unwrap_or("none"),
        if manifest.uses_api_key() { key.to_owned() } else { format!("none ({NO_KEY_CAVEAT})") },
        out["installation_drift"].as_str().unwrap_or_default(),
    );
    // ADR 0031 §7: a key in the argument vector is readable by every local user.
    if manifest.api_key_in_args() {
        let warning = "the engine key is in the argument list, visible to every local user in \
                       /proc/<pid>/cmdline; prefer an env value such as env: {SOME_KEY: \"{api_key}\"} \
                       where the engine reads one";
        summary.push_str(&format!("warning: {warning}\n"));
        out["warnings"] = json!([warning]);
    }
    {
        use std::io::Write;
        let _ = std::io::stderr().write_all(summary.as_bytes());
    }
    confirm_custom(request.yes)?;
    let revision = write_custom_profile(target, name, profile)?;
    out["revision"] = json!(revision);
    publish(target, out, revision, &written).await
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

/// ADR 0031 amendment A1: take the one lock both engines files share, then
/// re-check the name against both files and the role document, and write.
/// engines.yaml is never touched. The revision written.
fn write_custom_profile(
    target: &Target,
    name: &str,
    profile: Value,
) -> Result<u64, StructuredError> {
    let path = custom_engines(target);
    let lock = lock_engines_for(&path, service_owner(target))
        .map_err(|e| error("internal", format!("{}: {}", e.path, e.detail)))?;
    let mut custom = EnginesFile::load(&path).map_err(|e| error("invalid_config", e.detail))?;
    refuse_existing(target, name)?;
    custom.profiles.insert(name.to_owned(), profile);
    let host = match target.kind {
        RoleKind::Host => Some(role_document(target)?),
        RoleKind::Standalone => None,
    };
    write_engines(&custom, &lock, host.as_ref()).map_err(|e| error("invalid_config", e.detail))
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
            // ADR 0038 §2: a role's own TensorRT-LLM lists its companions.
            let profile = json!({
                "engine": entry["engine"], "executable": entry["executable"],
                "build_fingerprint": entry["build_fingerprint"],
                "security": {"deep_park": entry["deep_park"]},
                "companions": entry["companions"],
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
            // whether its launches carry a per-launch engine key. ADR 0038
            // §2, §7: TensorRT-LLM reports no idle signal and takes no engine
            // key.
            let manifest = capyctl_agent::installation::custom_manifest(&profile);
            let tensorrt_llm = kind == Some(Engine::TensorrtLlm);
            let companions = &profile["companions"];
            json!({
                "profile": name, "source": source, "engine": profile["engine"], "version": version,
                // ADR 0038 §2: TensorRT-LLM's version counts beside its torch build.
                "custom": !capyctl_config::registration::is_verified_with_torch(
                    engine,
                    &version,
                    companions[capyctl_config::tensorrt_llm::TORCH].as_str(),
                ),
                "idle_signal": manifest
                    .as_ref()
                    .map(|m| m.idle.as_ref().map_or_else(|| "none".to_owned(), |idle| idle.describe()))
                    .or_else(|| tensorrt_llm.then(|| "none".to_owned())),
                "engine_key": manifest.as_ref().map(engine_key).or(tensorrt_llm.then_some("none")),
                "companions": tensorrt_llm.then(|| companions.clone()),
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

/// ADR 0018 §4 (review decision C1): the CLI is the only writer of
/// engines.yaml. With no role running it edits the file (amendment A3).
/// Otherwise remove asks the running role to retire the profile, waits
/// for the confirmation, then writes engines.yaml without it and asks the
/// role to reload, which publishes the removal. A retry after any failure
/// resumes the same retirement (review decision I1), so a crash between the
/// confirmation and the write is finished by running remove again.
async fn remove(target: &Target, name: &str, drain: bool) -> Result<Value, StructuredError> {
    if ENVIRONMENT_PROFILES.contains(&name) && !registered_or_declared(target, name)? {
        return Err(error("invalid_config", format!(
            "{name} comes from the role's own installation (--vllm-bin / --sglang-bin / --tensorfold-bin / --llamacpp-bin / --tensorrt-llm-bin, CAPYCTL_VLLM_BIN / CAPYCTL_SGLANG_BIN / CAPYCTL_TENSORFOLD_BIN / CAPYCTL_LLAMACPP_BIN / CAPYCTL_TENSORRT_LLM_BIN or local_engine); unset it and restart the role instead"
        )));
    }
    // ADR 0031 amendment A1: the profile is in engines.yaml or in
    // custom-engines.yaml; the one that holds it is rewritten.
    let holder = registered_in(target, name)?;
    let registered = holder.is_some();
    let (file, file_revision) = match &holder {
        Some(file) => (file.path.clone(), file.revision),
        None => (
            target.engines.clone(),
            EnginesFile::load(&target.engines)
                .map_err(|e| error("invalid_config", e.detail))?
                .revision,
        ),
    };
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
            format!(
                "neither {} nor {} has a profile named {name}",
                target.engines.display(),
                custom_engines(target).display()
            ),
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
        // ADR 0018 amendment A3 (owner decision 2026-10-02): no role is
        // running, so nothing is published from this file and nothing runs
        // on it. The file is edited under its lock, as `engine add` does
        // offline; the role publishes the removal when it starts.
        Err(ClientError::NotRunning(_)) => return remove_offline(target, &file, name),
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
        write_without(target, &file, name).map_err(|e| {
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
        file_revision
    };
    let mut out = json!({"removed": name, "engines_file": file, "revision": revision});
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
                file.display()
            ),
        )),
        Err(e) => Err(error(
            "agent_unreachable",
            format!(
                "{e}; {name} is out of {} (revision {revision}) but its removal may not be published; run `capyctl engine list`, and `capyctl engine remove {name}` again to finish",
                file.display()
            ),
        )),
    }
}

/// ADR 0018 amendment A3: `engine remove` with no role listening. The
/// profile leaves `file` (engines.yaml or custom-engines.yaml); a deployment
/// that names it is refused as an unpublished profile once the role starts.
fn remove_offline(target: &Target, file: &Path, name: &str) -> Result<Value, StructuredError> {
    let revision = write_without(target, file, name).map_err(|e| {
        error(
            "internal",
            format!("{}: {}; nothing was removed", e.path, e.detail),
        )
    })?;
    Ok(json!({
        "removed": name, "engines_file": file, "revision": revision,
        "published": "role_not_running",
        "notice": format!(
            "removed from {} (revision {revision}); no capyctl answered on {}, so the \
             change takes effect when it starts (`{}`). If it is already running with \
             another --state-dir or --config, restart it, and pass the same option to \
             `capyctl engine`",
            file.display(),
            target.socket.display(),
            match target.kind {
                RoleKind::Standalone => "capyctl start standalone",
                RoleKind::Host => "capyctl start host",
            },
        ),
    }))
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

/// `file` (engines.yaml or custom-engines.yaml) without `name`, under its
/// lock, keeping its owner and mode.
fn write_without(
    target: &Target,
    file: &Path,
    name: &str,
) -> Result<u64, capyctl_config::ConfigError> {
    let lock = lock_engines_for(file, service_owner(target))?;
    let mut engines = EnginesFile::load(file)?;
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
