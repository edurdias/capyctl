//! ADR 0031 §1, §3, §7, §8 and ADR 0040 §11: adding or changing a custom
//! engine. Executes nothing: the executable is resolved and measured from
//! its files, and the manifest is read once and copied with its digest into
//! custom-engines.yaml (amendment A1). A preview returns the exact command,
//! the approval summary and a confirmation digest; the confirmation measures
//! again and writes only when the digest and the typed profile name still
//! match, pinning the previewed installation digest.
use super::engines::{drift_name, stale_name};
use super::{
    confirmation_digest, custom_engines, error, host_document, publish, refuse_existing,
    service_owner, HostAdminError, RoleChannel, Target, CONFIRMATION_STALE,
};
use crate::engines::resolve_custom;
use crate::installation::{
    custom_installation, CustomInstallation, InstallationFingerprint, InstallationMeasurer,
    UNMEASURED,
};
use capyctl_config::custom::{CustomManifest, CustomProfile, MANIFEST_INVALID, MAX_MANIFEST_BYTES};
use capyctl_config::effective::InstallationDrift;
use capyctl_config::engine_policy::Engine;
use capyctl_config::registration::{
    check_profile, custom_profile_document, lock_engines_for, valid_profile_name, write_engines,
    CustomProfileSpec, EnginesFile, ENVIRONMENT_PROFILES,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// ADR 0040 §11: the domain of a custom engine's confirmation digest.
const CUSTOM_DOMAIN: &str = "capyctl/custom-engine-confirmation/v1";

/// ADR 0040 §11: a manifest sent through the management API is capped below
/// the 64 KiB server-to-host message limit, leaving room for the rest of the
/// request (executable, name, environment, digest).
pub const MAX_REMOTE_MANIFEST_BYTES: usize = 48 * 1024;
const _: () = assert!(MAX_REMOTE_MANIFEST_BYTES < capyctl_protocol::SERVER_MESSAGE_BYTES);

/// ADR 0031 §7: the known limitation of an engine without a key.
const NO_KEY_CAVEAT: &str =
    "the engine listens unauthenticated on loopback; any local process can reach its port";

/// ADR 0040 §11: adding a new profile, or changing the custom profile of
/// that name in custom-engines.yaml.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustomOperation {
    Add,
    Change,
}

/// Where the manifest comes from, read once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestInput {
    /// `engine add --custom --manifest <path>`: the file's first 64 KiB + 1
    /// bytes, or why it could not be read (refused in its turn).
    File {
        path: PathBuf,
        read: Result<Vec<u8>, String>,
    },
    /// The manifest content of a management request body.
    Remote(String),
}

impl ManifestInput {
    /// ADR 0031 §2: the manifest file, read once and bounded.
    pub fn read_file(path: &Path) -> Self {
        use std::io::Read;
        let mut bytes = Vec::new();
        let read = std::fs::File::open(path)
            .and_then(|file| {
                file.take(MAX_MANIFEST_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)
            })
            .map(|_| bytes)
            .map_err(|e| e.to_string());
        Self::File {
            path: path.to_path_buf(),
            read,
        }
    }
}

/// ADR 0031 §1, ADR 0040 §11: what is asked to be registered.
#[derive(Debug, Clone)]
pub struct CustomEngine {
    /// The host the confirmation is bound to.
    pub host_id: String,
    pub operation: CustomOperation,
    pub name: String,
    pub executable: PathBuf,
    pub manifest: ManifestInput,
    /// `Some(true)` asks for deep park, which a custom engine refuses.
    pub deep_park: Option<bool>,
    pub drift: InstallationDrift,
    /// ADR 0028 §2.1: the profile's `env` and `approved_env`.
    pub env: BTreeMap<String, String>,
    pub approved_env: Vec<String>,
}

/// ADR 0040 §11: what the owner approves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomPreview {
    /// The executable and the argument template, as the launch runs them
    /// (placeholders unrendered).
    pub command: Vec<String>,
    /// The summary `engine add --custom` prints before it asks.
    pub summary: String,
    pub digest: String,
}

/// ADR 0031 §2, §10: the manifest, at most 64 KiB of UTF-8 from a file (ADR
/// 0040 §11: [`MAX_REMOTE_MANIFEST_BYTES`] through the API), validated.
/// `custom_manifest_invalid` (exit 27), or `engine_env_reserved:<NAME>` for
/// an `env` name CapyCTL owns.
fn read_manifest(input: &ManifestInput) -> Result<CustomManifest, HostAdminError> {
    let label = match input {
        ManifestInput::File { path, .. } => path.display().to_string(),
        ManifestInput::Remote(_) => "the manifest".to_owned(),
    };
    let invalid = |detail: String| {
        error(
            MANIFEST_INVALID,
            format!("{label}: {detail}; nothing was written"),
        )
    };
    let text = match input {
        ManifestInput::File { read, .. } => {
            let bytes = read
                .as_ref()
                .map_err(|e| invalid(format!("the manifest cannot be read: {e}")))?;
            if bytes.len() > MAX_MANIFEST_BYTES {
                return Err(invalid(format!(
                    "a manifest is at most 64 KiB ({MAX_MANIFEST_BYTES} bytes)"
                )));
            }
            String::from_utf8(bytes.clone())
                .map_err(|_| invalid("the manifest is not UTF-8 text".into()))?
        }
        ManifestInput::Remote(text) => {
            if text.len() > MAX_REMOTE_MANIFEST_BYTES {
                return Err(invalid(format!(
                    "a manifest sent to a host is at most {} KiB ({MAX_REMOTE_MANIFEST_BYTES} \
                     bytes), below the 64 KiB server-to-host message limit",
                    MAX_REMOTE_MANIFEST_BYTES / 1024
                )));
            }
            text.clone()
        }
    };
    let manifest = CustomManifest::parse(&text).map_err(|e| {
        if e.code == MANIFEST_INVALID {
            invalid(e.detail.clone())
        } else {
            error(
                "invalid_config",
                format!("{label}: {e}; nothing was written"),
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

/// Everything a preview shows and a confirmation re-checks.
struct Prepared {
    name: String,
    executable: PathBuf,
    manifest: CustomManifest,
    spec: CustomProfileSpec,
    profile: Value,
    layout: CustomInstallation,
    measured: Option<InstallationFingerprint>,
    out: Value,
    summary: String,
}

/// ADR 0031 §1, §10: every refusal before anything is asked or written, then
/// the installation measured from its files only (§3).
async fn prepare(
    target: &Target,
    request: &CustomEngine,
    measurer: &Arc<InstallationMeasurer>,
) -> Result<Prepared, HostAdminError> {
    // ADR 0031 §4: a custom engine never parks.
    if request.deep_park == Some(true) {
        return Err(error(
            "capability_missing",
            "a custom engine has no sleep or wake CapyCTL can drive; it runs restart_only, \
             so --deep-park enabled is refused and nothing was written",
        ));
    }
    let name = request.name.as_str();
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
    let executable = resolve_custom(&request.executable)
        .map_err(|e| error(e.code(), format!("{e}; nothing was written")))?;
    let manifest = read_manifest(&request.manifest)?;
    match request.operation {
        CustomOperation::Add => refuse_existing(target, name)?,
        CustomOperation::Change => {
            current_custom(target, name)?;
        }
    }
    // ADR 0031 §3: the installation's identity, from its files only.
    let (measured, layout) = {
        let (executable, manifest, measurer) =
            (executable.clone(), manifest.clone(), measurer.clone());
        tokio::task::spawn_blocking(move || {
            let layout = custom_installation(&executable, Some(&manifest));
            let measured = measurer
                .measure_profile(Engine::Custom, &executable, Some(&manifest))
                .ok();
            (measured, layout)
        })
        .await
        .map_err(|_| error("internal", "measuring the installation failed"))?
    };
    // ADR 0031 §8: the digest the summary shows is the one pinned; the role
    // registers against it and never adopts a different measurement.
    let mut custom = CustomProfile::new(manifest.clone());
    custom.installation_digest = measured.as_ref().map(|f| f.digest.clone());
    let spec = CustomProfileSpec {
        executable: executable.clone(),
        custom,
        installation_drift: request.drift,
        env: request.env.clone(),
        approved_env: request.approved_env.clone(),
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
        Some(f) => format!("{} ({} files, {} bytes)", f.digest, f.files, f.bytes),
        None => UNMEASURED.to_owned(),
    };
    let key = super::engine_key(&manifest);
    let idle = manifest.idle.as_ref().map(|idle| idle.describe());
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
        "fingerprint": measured.as_ref().map(|f| json!({"version": f.version, "digest": f.digest, "files": f.files, "bytes": f.bytes})),
        "manifest_digest": spec.custom.manifest_digest,
        "idle_signal": idle.clone().unwrap_or_else(|| "none".into()),
        "engine_key": key,
        "installation_drift": drift_name(spec.installation_drift),
        "deep_park": "disabled",
        "engines_file": custom_engines(target),
    });
    // ADR 0031 §8: the approval is this summary.
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
    Ok(Prepared {
        name: name.to_owned(),
        executable,
        manifest,
        spec,
        profile,
        layout,
        measured,
        out,
        summary,
    })
}

/// ADR 0040 §11: custom-engines.yaml holding `name`, which a change needs.
fn current_custom(target: &Target, name: &str) -> Result<EnginesFile, HostAdminError> {
    let path = custom_engines(target);
    let file = EnginesFile::load(&path).map_err(|e| error("invalid_config", e.detail))?;
    if !file.profiles.contains_key(name) {
        return Err(error(
            "invalid_config",
            format!(
                "{} has no custom engine named {name}; add it instead",
                path.display()
            ),
        ));
    }
    Ok(file)
}

/// ADR 0040 §11: exactly what the confirmation digest covers. `revision` is
/// custom-engines.yaml's for a change.
fn digest_of(request: &CustomEngine, prepared: &Prepared, revision: Option<u64>) -> String {
    let manifest = &prepared.manifest;
    let executable =
        std::fs::canonicalize(&prepared.executable).unwrap_or_else(|_| prepared.executable.clone());
    let fields = json!({
        "operation": match request.operation {
            CustomOperation::Add => json!({"kind": "add"}),
            CustomOperation::Change => json!({"kind": "change", "revision": revision}),
        },
        "host_id": request.host_id,
        "profile": prepared.name,
        "executable": executable,
        "manifest_digest": prepared.spec.custom.manifest_digest,
        "args": manifest.args,
        "env": prepared.spec.env,
        "approved_env": prepared.spec.approved_env,
        "installation_drift": drift_name(prepared.spec.installation_drift),
        "deep_park": "disabled",
        "interpreter": prepared.layout.interpreter,
        "root": prepared.layout.root.as_ref().map(|root| {
            json!({"path": root, "declared": prepared.layout.declared_root})
        }),
        "idle_signal": manifest.idle,
        "api": manifest.api,
        "engine_key": super::engine_key(manifest),
        "installation": match &prepared.measured {
            Some(f) => json!({"digest": f.digest, "files": f.files, "bytes": f.bytes}),
            None => json!(UNMEASURED),
        },
    });
    confirmation_digest(CUSTOM_DOMAIN, &fields)
}

/// ADR 0040 §11: the preview. Refuses as the add or change would, measures
/// the installation, and returns the command, the approval summary and the
/// digest a confirmation must carry. Nothing runs and nothing is written.
pub async fn preview_custom(
    target: &Target,
    request: &CustomEngine,
    measurer: &Arc<InstallationMeasurer>,
) -> Result<CustomPreview, HostAdminError> {
    let prepared = prepare(target, request, measurer).await?;
    let revision = match request.operation {
        CustomOperation::Add => None,
        CustomOperation::Change => Some(current_custom(target, &prepared.name)?.revision),
    };
    Ok(CustomPreview {
        command: std::iter::once(prepared.executable.display().to_string())
            .chain(prepared.manifest.args.iter().cloned())
            .collect(),
        digest: digest_of(request, &prepared, revision),
        summary: prepared.summary,
    })
}

/// ADR 0040 §11, ADR 0031 §8: the confirmation. `typed_name` must be the
/// profile name; the installation is measured again and, under the engines
/// lock, the digest is recomputed with custom-engines.yaml's revision; any
/// difference from `digest` is refused `confirmation_stale` and nothing is
/// written. Otherwise the profile, pinning the previewed installation
/// digest, is written to custom-engines.yaml (amendment A1) and the role is
/// asked to publish it through `channel`.
pub async fn confirm_custom(
    target: &Target,
    request: &CustomEngine,
    digest: &str,
    typed_name: &str,
    measurer: &Arc<InstallationMeasurer>,
    channel: &impl RoleChannel,
) -> Result<Value, HostAdminError> {
    if typed_name != request.name {
        return Err(stale_name(&request.name));
    }
    let prepared = prepare(target, request, measurer).await?;
    let path = custom_engines(target);
    // ADR 0031 amendment A1: the one lock both engines files share, then the
    // name re-checked against both files and the role document.
    let lock = lock_engines_for(&path, service_owner(target))
        .map_err(|e| error("internal", format!("{}: {}", e.path, e.detail)))?;
    let mut custom = EnginesFile::load(&path).map_err(|e| error("invalid_config", e.detail))?;
    let revision = match request.operation {
        CustomOperation::Add => None,
        CustomOperation::Change => Some(custom.revision),
    };
    if digest_of(request, &prepared, revision) != digest {
        return Err(error(
            CONFIRMATION_STALE,
            "the installation, the manifest, the request or custom-engines.yaml changed since \
             the preview; preview it again; nothing was written",
        ));
    }
    match request.operation {
        CustomOperation::Add => refuse_existing(target, &prepared.name)?,
        CustomOperation::Change => {
            if !custom.profiles.contains_key(&prepared.name) {
                return Err(error(
                    CONFIRMATION_STALE,
                    format!(
                        "{} no longer has {}; nothing was written",
                        path.display(),
                        prepared.name
                    ),
                ));
            }
        }
    }
    custom
        .profiles
        .insert(prepared.name.clone(), prepared.profile);
    let host = host_document(target)?;
    let revision = write_engines(&custom, &lock, host.as_ref())
        .map_err(|e| error("invalid_config", e.detail))?;
    drop(lock);
    let mut out = prepared.out;
    out["revision"] = json!(revision);
    publish(channel, target, out, revision, &path).await
}
