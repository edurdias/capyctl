//! ADR 0018 §1–§4: engine detection, built-in engine add and engine remove.
//! Detection reads metadata only; an installation runs only after the
//! operator named or picked it, or (ADR 0040 §10) after a remote add named a
//! detected candidate or confirmed a previewed digest.
use super::{
    closed, confirmation_digest, custom_engines, declared_by_operator, error, host_document,
    publish, refuse_existing, registered_in, registered_or_declared, service_owner, start_command,
    write_without, HostAdminError, RoleChannel, Target, ADD_REPLY, CONFIRMATION_REQUIRED,
    CONFIRMATION_STALE, REMOVE_REPLY,
};
use crate::control_socket::{ClientError, ControlRequest};
use crate::engines::{
    check_version, detect, resolve, Candidate, Resolved, ScanBounds, ScanRoots,
    VERSION_CHECK_TIMEOUT,
};
use crate::installation::{InstallationFingerprint, InstallationMeasurer, UNMEASURED};
use capyctl_config::effective::InstallationDrift;
use capyctl_config::engine_policy::Engine;
use capyctl_config::registration::{
    check_profile, lock_engines_for, profile_document, valid_profile_name, write_engines,
    EnginesFile, ProfileSpec, ENVIRONMENT_PROFILES,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// ADR 0040 §10: the domain of a built-in add's confirmation digest.
const BUILTIN_DOMAIN: &str = "capyctl/engine-add-confirmation/v1";

/// ADR 0018 §1: the installations detection finds under `roots`; ADR 0040
/// §10: `roots.extra` are the extra search directories (`engine detect
/// --path`).
pub fn candidates(roots: &ScanRoots) -> Vec<Candidate> {
    detect(roots, &ScanBounds::default())
}

/// `capyctl engine detect`'s result: `{"candidates": [...]}`.
pub fn detected(roots: &ScanRoots) -> Value {
    let rows: Vec<Value> = candidates(roots)
        .into_iter()
        .map(|c| {
            json!({"engine": c.engine.name(), "version": c.version, "custom": c.custom,
            "env": c.env, "entry": c.entry, "source": c.source})
        })
        .collect();
    json!({"candidates": rows})
}

/// What a built-in `engine add` registers: the installation, the profile
/// name, and what it writes beside the installation: host-fixed args
/// (`--arg`) and, ADR 0014 §8, what it approves (`--approve-option`,
/// `--approve-path`, `--allow-remote-code`).
#[derive(Debug, Clone)]
pub struct BuiltinAdd {
    /// ADR 0040 §11: the host the confirmation is bound to.
    pub host_id: String,
    pub path: PathBuf,
    pub name: Option<String>,
    /// `Some(true)` asks for deep park, `Some(false)` refuses it, `None`
    /// follows the probe (owner decision 2026-09-25).
    pub deep_park: Option<bool>,
    pub drift: InstallationDrift,
    pub args: Vec<String>,
    pub approved_options: Vec<String>,
    pub approved_paths: Vec<PathBuf>,
    /// ADR 0028 §2.1: the profile's `env` and `approved_env`.
    pub env: BTreeMap<String, String>,
    pub approved_env: Vec<String>,
    /// SPEC §3: `security.trust_remote_code`.
    pub remote_code: bool,
}

/// Where `engine add` looks for TensorFold's build toolchain.
pub struct Toolchain<'a> {
    pub cuda_home_env: Option<String>,
    pub search: &'a capyctl_config::toolchain::ToolchainSearch,
}

/// ADR 0018 §1, ADR 0040 §10: who approved running the named installation.
pub enum BuiltinApproval<'a> {
    /// The operator at the CLI on the box named or picked it.
    Operator,
    /// A remote add naming a candidate detection lists under these roots,
    /// without remote code.
    Detected(&'a ScanRoots),
    /// A remote add confirming [`preview_builtin`]'s digest with the profile
    /// name typed by the owner.
    Confirmed {
        digest: &'a str,
        typed_name: &'a str,
    },
}

/// ADR 0040 §10, §11: what a remote built-in add shows before it runs code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltinPreview {
    pub command: Vec<String>,
    pub summary: String,
    pub digest: String,
}

/// The installation and profile a built-in add checked, before anything ran.
struct Checked {
    resolved: Resolved,
    name: String,
    cuda_home: Option<PathBuf>,
}

/// ADR 0018 §1, ADR 0023, ADR 0029: every refusal of a built-in add that
/// comes before anything runs or is written.
fn check_builtin(
    target: &Target,
    request: &BuiltinAdd,
    toolchain: &Toolchain<'_>,
) -> Result<Checked, HostAdminError> {
    let resolved = resolve(&request.path).map_err(|e| error(e.code(), e.to_string()))?;
    let name = request
        .name
        .clone()
        .unwrap_or_else(|| resolved.engine.name().to_owned());
    if !valid_profile_name(&name) {
        return Err(error(
            "invalid_config",
            format!("profile name {name:?} must be lowercase letters, digits, '-' or '_'"),
        ));
    }
    // Owner rule 2026-09-25: on a host as in standalone, these names are the
    // role's own installation (`local_engine`, `--vllm-bin`, `CAPYCTL_VLLM_BIN`).
    if ENVIRONMENT_PROFILES.contains(&name.as_str()) {
        return Err(error("profile_exists", format!("{name} is reserved for the role's own installation (--vllm-bin / --sglang-bin / --tensorfold-bin / --llamacpp-bin, CAPYCTL_VLLM_BIN / CAPYCTL_SGLANG_BIN / CAPYCTL_TENSORFOLD_BIN / CAPYCTL_LLAMACPP_BIN or local_engine); use --name")));
    }
    // Checked before anything runs, and again under the lock when writing.
    refuse_existing(target, &name)?;
    // #198, ADR 0008 amendment 2026-10-30: an installation the engine's
    // protected entry can never import (an editable SGLang, reached only
    // through a .pth file the entry never runs) is refused before anything
    // runs or is written; every launch refuses it too.
    if let Some(refusal) =
        crate::installation::unsupported_installation(resolved.engine, &resolved.executable)
    {
        return Err(error(
            refusal,
            format!(
                "{}: SGLang is installed in editable mode (pip install -e, as in the \
                 lmsysorg/sglang image): its package is outside the environment's \
                 site-packages, reachable only through a .pth file CapyCTL never runs. \
                 Reinstall it without -e (pip install --no-deps --force-reinstall <the \
                 source checkout's python directory>), then add it again; nothing was written",
                resolved.env.display()
            ),
        ));
    }
    // SPEC §13.3 amendment (owner decision 2026-09-25). ADR 0029 §2: nothing
    // compiles at run time for llama.cpp, so no toolkit is recorded for it.
    let cuda_home = match resolved.engine {
        // ADR 0031 §1: `--custom` registers through its own path.
        Engine::Llamacpp | Engine::Custom => None,
        Engine::Vllm | Engine::Sglang | Engine::Tensorfold => {
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
    if request.remote_code {
        if let Some(family) = match resolved.engine {
            Engine::Tensorfold => Some("TensorFold"),
            Engine::Llamacpp => Some("llama.cpp"),
            Engine::Vllm | Engine::Sglang => None,
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
        if request.deep_park == Some(true) {
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
        if request.deep_park == Some(true) {
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
    Ok(Checked {
        resolved,
        name,
        cuda_home,
    })
}

/// The installation measured from its files (nothing runs).
async fn measure_files(
    measurer: &Arc<InstallationMeasurer>,
    resolved: &Resolved,
) -> Result<Result<InstallationFingerprint, ()>, HostAdminError> {
    let (measurer, engine, executable) = (
        measurer.clone(),
        resolved.engine,
        resolved.executable.clone(),
    );
    tokio::task::spawn_blocking(move || measurer.measure(engine, &executable).map_err(|_| ()))
        .await
        .map_err(|_| error("internal", "measuring the installation failed"))
}

/// ADR 0040 §10, §11: what a built-in add's confirmation covers.
fn builtin_fields(
    request: &BuiltinAdd,
    checked: &Checked,
    measured: &Result<InstallationFingerprint, ()>,
) -> Value {
    let executable = std::fs::canonicalize(&checked.resolved.executable)
        .unwrap_or_else(|_| checked.resolved.executable.clone());
    json!({
        "operation": "add",
        "host_id": request.host_id,
        "profile": checked.name,
        "engine": checked.resolved.engine.name(),
        "version": checked.resolved.version,
        "executable": executable,
        "args": request.args,
        "env": request.env,
        "approved_env": request.approved_env,
        "approved_options": request.approved_options,
        "approved_paths": request.approved_paths,
        "trust_remote_code": request.remote_code,
        "installation_drift": drift_name(request.drift),
        "deep_park": request.deep_park,
        "cuda_home": checked.cuda_home,
        "installation": match measured {
            Ok(f) => json!({"digest": f.digest, "files": f.files, "bytes": f.bytes}),
            Err(()) => json!(UNMEASURED),
        },
    })
}

pub(super) fn drift_name(drift: InstallationDrift) -> &'static str {
    match drift {
        InstallationDrift::Warn => "warn",
        InstallationDrift::Refuse => "refuse",
    }
}

/// ADR 0040 §10, §11: the command a built-in add would register, the
/// summary the owner approves, and the digest the add must carry. Executes
/// nothing; refuses as the add would.
pub async fn preview_builtin(
    target: &Target,
    request: &BuiltinAdd,
    toolchain: &Toolchain<'_>,
    measurer: &Arc<InstallationMeasurer>,
) -> Result<BuiltinPreview, HostAdminError> {
    let checked = check_builtin(target, request, toolchain)?;
    let measured = measure_files(measurer, &checked.resolved).await?;
    let digest = confirmation_digest(
        BUILTIN_DOMAIN,
        &builtin_fields(request, &checked, &measured),
    );
    let resolved = &checked.resolved;
    let command: Vec<String> = std::iter::once(resolved.executable.display().to_string())
        .chain(request.args.iter().cloned())
        .collect();
    let list = |items: &[String]| {
        if items.is_empty() {
            "none".to_owned()
        } else {
            items.join(", ")
        }
    };
    let summary = format!(
        "engine {}\n  engine:        {} {}\n  executable:    {}\n  arguments:     {}\n  environment:   {}\n  approved env:  {}\n  options:       {}\n  paths:         {}\n  remote code:   {}\n  installation:  {}\n  drift:         {}\n",
        checked.name,
        resolved.engine.name(),
        resolved.version,
        resolved.executable.display(),
        if request.args.is_empty() { "none".into() } else { request.args.join(" ") },
        list(&request.env.keys().cloned().collect::<Vec<_>>()),
        list(&request.approved_env),
        list(&request.approved_options),
        list(
            &request
                .approved_paths
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
        ),
        if request.remote_code { "allowed" } else { "refused" },
        match &measured {
            Ok(f) => format!("{} ({} files, {} bytes)", f.digest, f.files, f.bytes),
            Err(()) => UNMEASURED.to_owned(),
        },
        drift_name(request.drift),
    );
    Ok(BuiltinPreview {
        command,
        summary,
        digest,
    })
}

/// ADR 0040 §10: a remote add runs code only on a detected candidate
/// without remote code, or against the digest it previewed.
async fn check_approval(
    approval: &BuiltinApproval<'_>,
    request: &BuiltinAdd,
    checked: &Checked,
    measurer: &Arc<InstallationMeasurer>,
) -> Result<(), HostAdminError> {
    match approval {
        BuiltinApproval::Operator => Ok(()),
        BuiltinApproval::Detected(roots) => {
            let named =
                |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
            let wanted = named(&request.path);
            let listed = candidates(roots)
                .iter()
                .any(|c| named(&c.env) == wanted || named(&c.entry) == wanted);
            if listed && !request.remote_code {
                return Ok(());
            }
            Err(error(
                CONFIRMATION_REQUIRED,
                format!(
                    "{} {}; preview it and confirm the digest with the profile name; nothing was written",
                    request.path.display(),
                    if listed {
                        "is added with remote code allowed, which runs code a checkpoint ships"
                    } else {
                        "is not a detected installation"
                    }
                ),
            ))
        }
        BuiltinApproval::Confirmed { digest, typed_name } => {
            if *typed_name != checked.name {
                return Err(stale_name(&checked.name));
            }
            let measured = measure_files(measurer, &checked.resolved).await?;
            let now =
                confirmation_digest(BUILTIN_DOMAIN, &builtin_fields(request, checked, &measured));
            if now != *digest {
                return Err(error(
                    CONFIRMATION_STALE,
                    "the installation or the request changed since the preview; preview it \
                     again; nothing was written",
                ));
            }
            Ok(())
        }
    }
}

pub(super) fn stale_name(name: &str) -> HostAdminError {
    error(
        CONFIRMATION_STALE,
        format!("the confirmation must be the profile name, {name}; nothing was written"),
    )
}

struct Registration {
    reported: crate::engines::ReportedVersion,
    fingerprint: Option<InstallationFingerprint>,
    deep_park_missing: Option<bool>,
    /// ADR 0033 §1: the LMCache of a vLLM or SGLang environment.
    lmcache: Option<crate::lmcache_install::LmcacheMeasurement>,
}

/// ADR 0018 §1 step 3: executed only now, because the operator named it.
fn register(resolved: &Resolved, state_dir: &Path) -> Result<Registration, HostAdminError> {
    let reported = check_version(resolved, VERSION_CHECK_TIMEOUT).map_err(|e| {
        error(
            e.code(),
            format!(
                "{}: {e}; nothing was written",
                resolved.executable.display()
            ),
        )
    })?;
    let measurer = InstallationMeasurer::new();
    let fingerprint = measurer.measure(resolved.engine, &resolved.executable).ok();
    // ADR 0033 §1: LMCache's version and digest, read from the environment's
    // files (nothing runs); recorded only when found and measurable, and
    // never installed by capyctl.
    let lmcache = match resolved.engine {
        Engine::Vllm | Engine::Sglang => {
            crate::lmcache_install::measure_lmcache(&measurer, &resolved.executable)
                .ok()
                .flatten()
        }
        Engine::Tensorfold | Engine::Llamacpp | Engine::Custom => None,
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
    // SPEC §3.3 / ADR 0001: the probe runs from the runtime helpers compiled
    // into this binary; a refresh or restore is reported, never file contents.
    let materialized = crate::embedded_runtime::materialize(&runtime)
        .map_err(|e| error("invalid_config", format!("managed runtime directory: {e}")))?;
    if let Some(notice) = materialized.notice(&runtime) {
        capyctl_domain::role_log::notice(capyctl_domain::role_log::Level::Warning, &notice);
    }
    let report = crate::installation::probe_capabilities(
        resolved.engine,
        &resolved.executable,
        &runtime,
        crate::installation::PROBE_TIMEOUT,
    );
    Ok(Registration {
        reported,
        fingerprint,
        deep_park_missing: report.and_then(|r| r.available("deep_park")).map(|a| !a),
        lmcache,
    })
}

/// ADR 0018 §2: lock engines.yaml, re-check the name against both files,
/// validate, write. The role document is never written. The revision written.
fn write_profile(target: &Target, name: &str, spec: &ProfileSpec) -> Result<u64, HostAdminError> {
    let lock = lock_engines_for(&target.engines, service_owner(target))
        .map_err(|e| error("internal", format!("{}: {}", e.path, e.detail)))?;
    let mut engines =
        EnginesFile::load(&target.engines).map_err(|e| error("invalid_config", e.detail))?;
    refuse_existing(target, name)?;
    let profile = profile_document(spec);
    check_profile(name, &profile)
        .map_err(|e| error("invalid_config", format!("{}: {}", e.path, e.detail)))?;
    engines.profiles.insert(name.to_owned(), profile);
    let host = host_document(target)?;
    write_engines(&engines, &lock, host.as_ref()).map_err(|e| error("invalid_config", e.detail))
}

/// ADR 0018 §1–§3: `capyctl engine add <path>`. Refuses before anything
/// runs; once `approval` holds, checks the installation's version and
/// capabilities (this runs it), writes the profile into engines.yaml and
/// asks the role to publish it through `channel`.
pub async fn add_builtin(
    target: &Target,
    request: &BuiltinAdd,
    toolchain: &Toolchain<'_>,
    approval: BuiltinApproval<'_>,
    measurer: &Arc<InstallationMeasurer>,
    channel: &impl RoleChannel,
) -> Result<Value, HostAdminError> {
    let checked = check_builtin(target, request, toolchain)?;
    check_approval(&approval, request, &checked, measurer).await?;
    let Checked {
        resolved,
        name,
        cuda_home,
    } = checked;
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
    let deep = match request.deep_park {
        Some(asked) => asked,
        // ADR 0023 §2, ADR 0029 §2: TensorFold and llama.cpp are disabled even
        // when the probe could not run.
        None => {
            !matches!(resolved.engine, Engine::Tensorfold | Engine::Llamacpp)
                && registration.deep_park_missing != Some(true)
        }
    };
    let spec = ProfileSpec {
        engine: resolved.engine,
        executable: resolved.executable.clone(),
        // ADR 0029 §2: llama.cpp's is `<version>+<commit>`.
        build_fingerprint: registration.reported.build_fingerprint.clone(),
        deep_park: deep,
        installation_drift: request.drift,
        args: request.args.clone(),
        env: request.env.clone(),
        approved_env: request.approved_env.clone(),
        cuda_home,
        approved_options: request.approved_options.clone(),
        approved_paths: request
            .approved_paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
        trust_remote_code: request.remote_code,
        lmcache: registration
            .lmcache
            .as_ref()
            .map(crate::lmcache_install::LmcacheMeasurement::registration),
    };
    let revision = write_profile(target, &name, &spec)?;
    let out = json!({
        "profile": name, "engine": resolved.engine.name(),
        "version": registration.reported.version,
        "build_fingerprint": spec.build_fingerprint,
        "custom": !capyctl_config::registration::is_verified(resolved.engine, &spec.build_fingerprint),
        "executable": resolved.executable,
        "fingerprint": registration.fingerprint.map(|f| json!({"version": f.version, "digest": f.digest})),
        "deep_park": if deep { "enabled" } else { "disabled" }, "deep_park_probe": probe,
        "engines_file": target.engines, "revision": revision,
        "cuda_home": spec.cuda_home,
        "trust_remote_code": spec.trust_remote_code,
        // ADR 0033 §1: what was recorded, or null when LMCache was not found.
        "lmcache": spec.lmcache,
    });
    publish(channel, target, out, revision, &target.engines).await
}

/// ADR 0018 §4 (review decision C1): the CLI is the only writer of
/// engines.yaml. With no role running it edits the file (amendment A3).
/// Otherwise remove asks the running role to retire the profile, waits
/// for the confirmation, then writes engines.yaml without it and asks the
/// role to reload, which publishes the removal. A retry after any failure
/// resumes the same retirement (review decision I1), so a crash between the
/// confirmation and the write is finished by running remove again.
pub async fn remove(
    target: &Target,
    name: &str,
    drain: bool,
    channel: &impl RoleChannel,
) -> Result<Value, HostAdminError> {
    if ENVIRONMENT_PROFILES.contains(&name) && !registered_or_declared(target, name)? {
        return Err(error("invalid_config", format!(
            "{name} comes from the role's own installation (--vllm-bin / --sglang-bin / --tensorfold-bin / --llamacpp-bin, CAPYCTL_VLLM_BIN / CAPYCTL_SGLANG_BIN / CAPYCTL_TENSORFOLD_BIN / CAPYCTL_LLAMACPP_BIN or local_engine); unset it and restart the role instead"
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
    let retired = match channel
        .ask(
            ControlRequest::Remove {
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
    match channel.ask(ControlRequest::Add, ADD_REPLY).await {
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
fn remove_offline(target: &Target, file: &Path, name: &str) -> Result<Value, HostAdminError> {
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
            start_command(target),
        ),
    }))
}

/// review decision I4: a remove the role took but never answered.
fn unknown_outcome(cause: &str, name: &str) -> HostAdminError {
    error(
        "agent_unreachable",
        format!(
            "{cause}; the outcome is unknown: the role may still be retiring {name}. \
             Run `capyctl engine list` to see whether it is still published, and run \
             `capyctl engine remove {name}` again to finish; a retry resumes the same removal"
        ),
    )
}
