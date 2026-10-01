//! Owner rule 2026-09-25 (SPEC §15.2: run-time role flags override ordinary
//! settings under one documented precedence): every engine-installation
//! setting of a role is stated three ways, a YAML field, a CLI flag on
//! `start host` / `start standalone`, and an environment variable, with one
//! precedence for all of them: CLI flag > environment > YAML > default.
//! Implemented once here and used by both roles that run engines (standalone
//! behaves as a server plus one host).
//!
//! | Setting | YAML (host document; standalone `host:` block) | Flag | Variable |
//! |---|---|---|---|
//! | vLLM executable | `local_engine.vllm` | `--vllm-bin` | `CAPYCTL_VLLM_BIN` |
//! | SGLang executable | `local_engine.sglang` | `--sglang-bin` | `CAPYCTL_SGLANG_BIN` |
//! | TensorFold executable | `local_engine.tensorfold` | `--tensorfold-bin` | `CAPYCTL_TENSORFOLD_BIN` |
//! | build fingerprint | `local_engine.build_fingerprint` | `--engine-fingerprint` | `CAPYCTL_ENGINE_FINGERPRINT` |
//! | host-fixed vLLM args | `local_engine.args` | `--engine-args` | `CAPYCTL_ENGINE_ARGS` |
//! | KV cache (standalone) | `local_engine.kv_cache` | `--kv-cache` | `CAPYCTL_KV_CACHE_BYTES` |
//! | deep parking | `local_engine.deep_park` | `--deep-park` | `CAPYCTL_DEEP_PARK` |
//! | trust_remote_code | `local_engine.trust_remote_code` | `--trust-remote-code` | `CAPYCTL_TRUST_REMOTE_CODE` |
//! | installation drift | `local_engine.installation_drift` | `--installation-drift` | `CAPYCTL_INSTALLATION_DRIFT` |
//! | runtime directory | `runtime_dir` | `--runtime-dir` | `CAPYCTL_RUNTIME_DIR` |
//! | engine port range | `resource_policy.endpoint_port_range` | `--engine-ports` | `CAPYCTL_ENGINE_PORTS` |
//! | CUDA toolkit | `local_engine.cuda_home` | `--cuda-home` | `CAPYCTL_CUDA_HOME` |
//!
//! The `local_engine` executables declare the role's unnamed installation:
//! one of them is the runtime profile `local`; several are `local-vllm`,
//! `local-sglang` and `local-tensorfold` (ADR 0018 §5, ADR 0023 §2). The switches in `local_engine` apply to
//! those profiles only; a profile declared in `runtime_profiles` or
//! registered with `capyctl engine add` states its own. `CAPYCTL_ENGINE_PORTS`
//! replaces the standalone-only `CAPYCTL_STANDALONE_ENGINE_PORTS`, which is
//! still read after it with a deprecation warning.

use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::effective::InstallationDrift;
use crate::engine_policy::Engine;
use crate::{ConfigError, ConfigErrorCode};

pub const VLLM_BIN_ENV: &str = "CAPYCTL_VLLM_BIN";
pub const SGLANG_BIN_ENV: &str = "CAPYCTL_SGLANG_BIN";
pub const TENSORFOLD_BIN_ENV: &str = "CAPYCTL_TENSORFOLD_BIN";
pub const ENGINE_FINGERPRINT_ENV: &str = "CAPYCTL_ENGINE_FINGERPRINT";
pub const ENGINE_ARGS_ENV: &str = "CAPYCTL_ENGINE_ARGS";
pub const KV_CACHE_ENV: &str = "CAPYCTL_KV_CACHE_BYTES";
pub const DEEP_PARK_ENV: &str = "CAPYCTL_DEEP_PARK";
pub const TRUST_REMOTE_CODE_ENV: &str = "CAPYCTL_TRUST_REMOTE_CODE";
pub const INSTALLATION_DRIFT_ENV: &str = "CAPYCTL_INSTALLATION_DRIFT";
pub const RUNTIME_DIR_ENV: &str = "CAPYCTL_RUNTIME_DIR";
/// SPEC §13.3 amendment (owner decision 2026-09-25): the CUDA toolkit root of
/// the role's own installation, published as its profile's `cuda_home`.
pub const CUDA_HOME_ENV: &str = "CAPYCTL_CUDA_HOME";
/// The engines' loopback port range, `start-end`, for either role.
pub const ENGINE_PORTS_ENV: &str = "CAPYCTL_ENGINE_PORTS";
/// The standalone-only name [`ENGINE_PORTS_ENV`] replaces. Still read, after
/// it, with a deprecation warning ([`deprecation_warnings`]).
pub const DEPRECATED_ENGINE_PORTS_ENV: &str = "CAPYCTL_STANDALONE_ENGINE_PORTS";
/// SPEC §16.5: the default engine port range.
pub const DEFAULT_ENGINE_PORTS: (u16, u16) = (8100, 8199);
/// The YAML block naming the role's own engine installation.
pub const LOCAL_ENGINE: &str = "local_engine";

fn refuse(path: &str, detail: impl Into<String>) -> ConfigError {
    ConfigError::new(ConfigErrorCode::UnsupportedCombination, path, detail)
}

/// One layer of engine settings: the CLI flags, the environment, or the YAML
/// document. `None` is "this layer does not state it".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EngineOverrides {
    pub vllm: Option<PathBuf>,
    pub sglang: Option<PathBuf>,
    pub tensorfold: Option<PathBuf>,
    pub build_fingerprint: Option<String>,
    pub args: Option<Vec<String>>,
    pub kv_cache: Option<String>,
    pub deep_park: Option<bool>,
    pub trust_remote_code: Option<bool>,
    pub installation_drift: Option<InstallationDrift>,
    pub runtime_dir: Option<PathBuf>,
    pub engine_ports: Option<(u16, u16)>,
    pub cuda_home: Option<PathBuf>,
}

impl EngineOverrides {
    /// `self`, with every setting it leaves unstated taken from `lower`.
    pub fn or(self, lower: Self) -> Self {
        Self {
            vllm: self.vllm.or(lower.vllm),
            sglang: self.sglang.or(lower.sglang),
            tensorfold: self.tensorfold.or(lower.tensorfold),
            build_fingerprint: self.build_fingerprint.or(lower.build_fingerprint),
            args: self.args.or(lower.args),
            kv_cache: self.kv_cache.or(lower.kv_cache),
            deep_park: self.deep_park.or(lower.deep_park),
            trust_remote_code: self.trust_remote_code.or(lower.trust_remote_code),
            installation_drift: self.installation_drift.or(lower.installation_drift),
            runtime_dir: self.runtime_dir.or(lower.runtime_dir),
            engine_ports: self.engine_ports.or(lower.engine_ports),
            cuda_home: self.cuda_home.or(lower.cuda_home),
        }
    }

    /// Whether this layer names an engine executable.
    pub fn names_an_engine(&self) -> bool {
        self.vllm.is_some() || self.sglang.is_some() || self.tensorfold.is_some()
    }

    /// The environment's layer, read through `get` (the raw value; `None`
    /// when unset). An empty value is unset for the paths, the fingerprint,
    /// the arguments and the KV cache (the shape a mistyped export leaves);
    /// SPEC §15.3 (T03): for the switches and the port range it is refused,
    /// because a mistyped opt-out silently left on is the failure those
    /// switches exist to prevent. A malformed value is refused with the
    /// variable's name.
    pub fn from_env(get: &dyn Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let text = |key: &str| get(key).filter(|value| !value.is_empty());
        let ports = match get(ENGINE_PORTS_ENV) {
            Some(value) => Some(port_range(ENGINE_PORTS_ENV, &value)?),
            None => get(DEPRECATED_ENGINE_PORTS_ENV)
                .map(|value| port_range(DEPRECATED_ENGINE_PORTS_ENV, &value))
                .transpose()?,
        };
        Ok(Self {
            vllm: text(VLLM_BIN_ENV).map(PathBuf::from),
            sglang: text(SGLANG_BIN_ENV).map(PathBuf::from),
            tensorfold: text(TENSORFOLD_BIN_ENV).map(PathBuf::from),
            build_fingerprint: text(ENGINE_FINGERPRINT_ENV),
            args: text(ENGINE_ARGS_ENV).map(|value| split_args(&value)),
            kv_cache: text(KV_CACHE_ENV)
                .map(|value| kv_cache(KV_CACHE_ENV, &value).map(|_| value))
                .transpose()?,
            deep_park: get(DEEP_PARK_ENV)
                .map(|value| deep_park(DEEP_PARK_ENV, &value))
                .transpose()?,
            trust_remote_code: text(TRUST_REMOTE_CODE_ENV)
                .map(|value| boolean(TRUST_REMOTE_CODE_ENV, &value))
                .transpose()?,
            installation_drift: get(INSTALLATION_DRIFT_ENV)
                .map(|value| drift(INSTALLATION_DRIFT_ENV, &value))
                .transpose()?,
            runtime_dir: text(RUNTIME_DIR_ENV).map(PathBuf::from),
            engine_ports: ports,
            cuda_home: text(CUDA_HOME_ENV)
                .map(|value| {
                    Some(PathBuf::from(&value))
                        .filter(|path| path.is_absolute())
                        .ok_or_else(|| {
                            refuse(
                                CUDA_HOME_ENV,
                                format!("must be an absolute directory; got {value:?}"),
                            )
                        })
                })
                .transpose()?,
        })
    }

    /// The process environment's layer.
    pub fn from_process_env() -> Result<Self, ConfigError> {
        Self::from_env(&|key| std::env::var(key).ok())
    }

    /// The YAML layer of `block`: a host document, or a standalone
    /// document's `host:` block. Paths in YAML are absolute.
    pub fn from_document(block: &Value) -> Result<Self, ConfigError> {
        let local = match block.get(LOCAL_ENGINE) {
            None | Some(Value::Null) => &Value::Null,
            Some(local) if local.is_object() => local,
            Some(_) => return Err(refuse(LOCAL_ENGINE, "must be a mapping")),
        };
        let path_of = |value: Option<&Value>, name: &str| -> Result<Option<PathBuf>, ConfigError> {
            value
                .map(|value| {
                    value
                        .as_str()
                        .map(PathBuf::from)
                        .filter(|path| path.is_absolute())
                        .ok_or_else(|| refuse(name, "must be an absolute path"))
                })
                .transpose()
        };
        let text_of = |key: &str| -> Result<Option<String>, ConfigError> {
            local
                .get(key)
                .map(|value| {
                    value
                        .as_str()
                        .filter(|text| !text.is_empty())
                        .map(str::to_owned)
                        .ok_or_else(|| refuse(&format!("{LOCAL_ENGINE}.{key}"), "must be text"))
                })
                .transpose()
        };
        let scalar_of = |key: &str| -> Option<String> {
            local.get(key).map(|value| match value {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            })
        };
        let args = local
            .get("args")
            .map(|value| match value {
                Value::Array(items) => items
                    .iter()
                    .map(|item| item.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| refuse("local_engine.args", "must be a list of strings")),
                Value::String(text) => Ok(split_args(text)),
                _ => Err(refuse("local_engine.args", "must be a list of strings")),
            })
            .transpose()?;
        let ports = match block
            .get("resource_policy")
            .and_then(|policy| policy.get("endpoint_port_range"))
        {
            None => None,
            Some(range) => {
                let port = |key: &str| {
                    range
                        .get(key)
                        .and_then(|value| match value {
                            Value::Number(number) => number.as_u64(),
                            Value::String(text) => text.parse().ok(),
                            _ => None,
                        })
                        .and_then(|port| u16::try_from(port).ok())
                };
                let path = "resource_policy.endpoint_port_range";
                let (Some(start), Some(end)) = (port("start"), port("end")) else {
                    return Err(refuse(path, "must state `start` and `end` ports"));
                };
                Some(checked_range(path, start, end)?)
            }
        };
        Ok(Self {
            vllm: path_of(local.get("vllm"), "local_engine.vllm")?,
            sglang: path_of(local.get("sglang"), "local_engine.sglang")?,
            tensorfold: path_of(local.get("tensorfold"), "local_engine.tensorfold")?,
            build_fingerprint: text_of("build_fingerprint")?,
            args,
            kv_cache: text_of("kv_cache")?
                .map(|value| kv_cache("local_engine.kv_cache", &value).map(|_| value))
                .transpose()?,
            deep_park: scalar_of("deep_park")
                .map(|value| deep_park("local_engine.deep_park", &value))
                .transpose()?,
            trust_remote_code: scalar_of("trust_remote_code")
                .map(|value| boolean("local_engine.trust_remote_code", &value))
                .transpose()?,
            installation_drift: scalar_of("installation_drift")
                .map(|value| drift("local_engine.installation_drift", &value))
                .transpose()?,
            runtime_dir: path_of(block.get("runtime_dir"), "runtime_dir")?,
            engine_ports: ports,
            cuda_home: path_of(local.get("cuda_home"), "local_engine.cuda_home")?,
        })
    }
}

/// `on` or `off`. SPEC §9.1 / ADR 0012: deep parking is on unless the host
/// opts out; any other spelling is refused, never guessed (SPEC §15.3).
pub fn deep_park(name: &str, text: &str) -> Result<bool, ConfigError> {
    match text {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err(refuse(
            name,
            format!(
                "must be `on` or `off` (unset means on; `off` opts this host out of deep \
                 parking); got {text:?}"
            ),
        )),
    }
}

/// `true`/`false` (also `1`/`0`, `on`/`off`).
pub fn boolean(name: &str, text: &str) -> Result<bool, ConfigError> {
    match text {
        "true" | "1" | "on" => Ok(true),
        "false" | "0" | "off" => Ok(false),
        _ => Err(refuse(
            name,
            format!("must be `true` or `false`; got {text:?}"),
        )),
    }
}

/// ADR 0008: `warn` (the default) or `refuse`.
pub fn drift(name: &str, text: &str) -> Result<InstallationDrift, ConfigError> {
    match text {
        "warn" => Ok(InstallationDrift::Warn),
        "refuse" => Ok(InstallationDrift::Refuse),
        _ => Err(refuse(
            name,
            format!("must be `warn` or `refuse` (unset means warn); got {text:?}"),
        )),
    }
}

/// A positive byte size such as `16GiB`.
pub fn kv_cache(name: &str, text: &str) -> Result<i64, ConfigError> {
    crate::effective::parse_bytes(text)
        .ok()
        .filter(|bytes| *bytes > 0)
        .ok_or_else(|| {
            refuse(
                name,
                format!("must be a positive byte size such as 16GiB; got {text:?}"),
            )
        })
}

fn checked_range(name: &str, start: u16, end: u16) -> Result<(u16, u16), ConfigError> {
    if start >= 1024 && start <= end {
        Ok((start, end))
    } else {
        Err(refuse(
            name,
            format!(
                "must be an inclusive port range with 1024 <= start <= end, e.g. 8100-8199; \
                 got {start}-{end}"
            ),
        ))
    }
}

/// SPEC §15.3: an inclusive port range `start-end` with `1024 <= start <= end`.
pub fn port_range(name: &str, text: &str) -> Result<(u16, u16), ConfigError> {
    let parsed = text
        .split_once('-')
        .and_then(|(start, end)| Some((start.parse::<u16>().ok()?, end.parse::<u16>().ok()?)));
    match parsed {
        Some((start, end)) => checked_range(name, start, end),
        None => Err(refuse(
            name,
            format!(
                "must be an inclusive port range `start-end` with 1024 <= start <= end, \
                 e.g. 8100-8199; got {text:?}"
            ),
        )),
    }
}

/// Host-fixed arguments written as one string, split on spaces.
pub fn split_args(text: &str) -> Vec<String> {
    text.split(' ')
        .filter(|argument| !argument.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The warnings a role prints once at start: the deprecated
/// `CAPYCTL_STANDALONE_ENGINE_PORTS` is set.
pub fn deprecation_warnings(get: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
    if get(DEPRECATED_ENGINE_PORTS_ENV).is_none() {
        return Vec::new();
    }
    vec![format!(
        "warning: {DEPRECATED_ENGINE_PORTS_ENV} is deprecated; use {ENGINE_PORTS_ENV}{}",
        if get(ENGINE_PORTS_ENV).is_some() {
            " (ignored for this run: CAPYCTL_ENGINE_PORTS is set)"
        } else {
            ""
        }
    )]
}

/// The resolved engine settings of one role, with the defaults applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineSettings {
    pub vllm: Option<PathBuf>,
    pub sglang: Option<PathBuf>,
    pub tensorfold: Option<PathBuf>,
    /// Stated, or `None`: the role asks the engine (`<engine> --version`).
    pub build_fingerprint: Option<String>,
    pub args: Vec<String>,
    /// Stated, or `None`: the role's own default.
    pub kv_cache: Option<String>,
    /// Default on (SPEC §9.1, ADR 0012).
    pub deep_park: bool,
    /// Default off (Spec §3).
    pub trust_remote_code: bool,
    /// Default `warn` (ADR 0008).
    pub installation_drift: InstallationDrift,
    /// Stated, or `None`: the managed `<state_dir>/runtime` (SPEC §3.3).
    pub runtime_dir: Option<PathBuf>,
    /// Stated, or `None`: [`DEFAULT_ENGINE_PORTS`].
    pub engine_ports: Option<(u16, u16)>,
    /// Stated, or `None`: the engine PATH stays minimal (nothing is detected
    /// for the role's own installation; `capyctl engine add` detects one).
    pub cuda_home: Option<PathBuf>,
}

/// Owner rule 2026-09-25: CLI flag > environment > YAML > default, setting by
/// setting.
pub fn resolve(
    flag: &EngineOverrides,
    env: &EngineOverrides,
    document: &EngineOverrides,
) -> EngineSettings {
    let merged = flag.clone().or(env.clone()).or(document.clone());
    EngineSettings {
        vllm: merged.vllm,
        sglang: merged.sglang,
        tensorfold: merged.tensorfold,
        build_fingerprint: merged.build_fingerprint,
        args: merged.args.unwrap_or_default(),
        kv_cache: merged.kv_cache,
        deep_park: merged.deep_park.unwrap_or(true),
        trust_remote_code: merged.trust_remote_code.unwrap_or(false),
        installation_drift: merged.installation_drift.unwrap_or_default(),
        runtime_dir: merged.runtime_dir,
        engine_ports: merged.engine_ports,
        cuda_home: merged.cuda_home,
    }
}

impl EngineSettings {
    /// ADR 0018 §5, ADR 0023 §2: the role's own installations and their
    /// profile names: one executable is `local`; several are `local-<engine>`.
    pub fn installations(&self) -> Vec<(&'static str, Engine, PathBuf)> {
        let named: Vec<(Engine, PathBuf)> = [
            (Engine::Vllm, &self.vllm),
            (Engine::Sglang, &self.sglang),
            (Engine::Tensorfold, &self.tensorfold),
        ]
        .into_iter()
        .filter_map(|(engine, path)| path.clone().map(|path| (engine, path)))
        .collect();
        if let [(engine, path)] = named.as_slice() {
            return vec![("local", *engine, path.clone())];
        }
        named
            .into_iter()
            .map(|(engine, path)| {
                let name = match engine {
                    Engine::Vllm => "local-vllm",
                    Engine::Sglang => "local-sglang",
                    Engine::Tensorfold => "local-tensorfold",
                };
                (name, engine, path)
            })
            .collect()
    }
}

/// Owner rule 2026-09-25 (standalone is a server plus one host): apply a
/// host's resolved engine settings to its document before it is published.
///
/// - `runtime_dir` and `resource_policy.endpoint_port_range` are stated when a
///   layer names them;
/// - the `local_engine` executables become the runtime profiles `local` (or
///   `local-vllm`, `local-sglang` and `local-tensorfold`), built exactly as `capyctl engine add`
///   builds a profile ([`crate::registration::profile_document`]), with the
///   fingerprint stated or read by `probe` from `<engine> --version`. A name
///   the document already declares is refused (`profile_exists`);
/// - the `local_engine` block itself is removed: what the host publishes is
///   the profiles it made from it.
///
/// A host generates no deployment, so a KV cache is a deployment's own
/// (`engine_config.memory.kv_cache`) and `kv_cache` is refused here.
pub fn apply_to_host(
    document: &mut Value,
    settings: &EngineSettings,
    probe: &dyn Fn(Engine, &Path) -> Result<String, String>,
) -> Result<(), ConfigError> {
    apply_to_host_with(document, settings, probe, &Default::default())
}

/// As [`apply_to_host`], looking for TensorFold's build toolchain in `search`.
pub fn apply_to_host_with(
    document: &mut Value,
    settings: &EngineSettings,
    probe: &dyn Fn(Engine, &Path) -> Result<String, String>,
    search: &crate::toolchain::ToolchainSearch,
) -> Result<(), ConfigError> {
    if settings.kv_cache.is_some() {
        return Err(refuse(
            "local_engine.kv_cache",
            "a host generates no deployment; state engine_config.memory.kv_cache in the \
             deployment instead",
        ));
    }
    let object = document
        .as_object_mut()
        .ok_or_else(|| refuse("", "the host document is not a mapping"))?;
    object.remove(LOCAL_ENGINE);
    if let Some(dir) = &settings.runtime_dir {
        object.insert("runtime_dir".into(), json!(dir));
    }
    if let Some((start, end)) = settings.engine_ports {
        let policy = object
            .entry("resource_policy")
            .or_insert_with(|| Value::Object(Map::new()));
        policy["endpoint_port_range"] = json!({"start": start, "end": end});
    }
    let installations = settings.installations();
    if installations.is_empty() {
        return Ok(());
    }
    let profiles = object
        .entry("runtime_profiles")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| refuse("runtime_profiles", "must be a mapping"))?;
    for (name, engine, executable) in installations {
        if profiles.contains_key(name) {
            return Err(ConfigError::new(
                ConfigErrorCode::UnsupportedCombination,
                format!("runtime_profiles.{name}"),
                format!(
                    "profile_exists: {name} is declared in runtime_profiles (or engines.yaml) \
                     and also named by local_engine, --vllm-bin/--sglang-bin/--tensorfold-bin or \
                     CAPYCTL_VLLM_BIN/CAPYCTL_SGLANG_BIN/CAPYCTL_TENSORFOLD_BIN; remove one"
                ),
            ));
        }
        let build_fingerprint = match &settings.build_fingerprint {
            Some(stated) => stated.clone(),
            None => probe(engine, &executable).map_err(|detail| {
                refuse(
                    &format!("runtime_profiles.{name}"),
                    format!("{detail}; state local_engine.build_fingerprint (or {ENGINE_FINGERPRINT_ENV}) to publish one explicitly"),
                )
            })?,
        };
        // ADR 0023 §2: the first start builds CUDA extensions, so the
        // toolchain is checked here as `engine add` checks it.
        if engine == Engine::Tensorfold {
            let bin = executable.parent().unwrap_or(Path::new(""));
            crate::toolchain::check(bin, settings.cuda_home.as_deref(), &search.system).map_err(
                |missing| {
                    ConfigError::new(
                        ConfigErrorCode::UnsupportedCombination,
                        format!("runtime_profiles.{name}"),
                        format!("toolchain_missing: {}", missing.for_local_engine()),
                    )
                },
            )?;
        }
        // ADR 0014 §1: SGLang's protected entry takes no argument vector.
        let args = match engine {
            Engine::Vllm | Engine::Tensorfold => settings.args.clone(),
            Engine::Sglang => Vec::new(),
        };
        let mut profile =
            crate::registration::profile_document(&crate::registration::ProfileSpec {
                engine,
                executable,
                build_fingerprint,
                // ADR 0023 §6: TensorFold never parks, whatever local_engine.deep_park says.
                deep_park: settings.deep_park && engine != Engine::Tensorfold,
                installation_drift: settings.installation_drift,
                args,
                cuda_home: settings.cuda_home.clone(),
            });
        profile["security"]["trust_remote_code"] = json!(settings.trust_remote_code);
        crate::registration::check_profile(name, &profile)?;
        profiles.insert(name.to_owned(), profile);
    }
    Ok(())
}

/// The settings a running host resolved at start (models directory, model
/// sources, runtime directory, engine port range and the `local_engine`
/// profiles) change only with a restart. A live reload of the document
/// (ADR 0018 §3) therefore carries the running values over into `reloaded`,
/// so only a change to `runtime_profiles` is published live.
pub fn carry_start_settings(running: &Value, reloaded: &mut Value) {
    let Some(object) = reloaded.as_object_mut() else {
        return;
    };
    object.remove(LOCAL_ENGINE);
    for field in ["model_store", "model_sources", "runtime_dir"] {
        match running.get(field) {
            Some(value) => {
                object.insert(field.into(), value.clone());
            }
            None => {
                object.remove(field);
            }
        }
    }
    let running_range = running
        .get("resource_policy")
        .and_then(|policy| policy.get("endpoint_port_range"));
    match (running_range, object.get_mut("resource_policy")) {
        (Some(range), Some(policy)) => policy["endpoint_port_range"] = range.clone(),
        (Some(range), None) => {
            object.insert(
                "resource_policy".into(),
                json!({"endpoint_port_range": range}),
            );
        }
        (None, Some(Value::Object(policy))) => {
            policy.remove("endpoint_port_range");
        }
        (None, _) => {}
    }
    // SPEC §3 / ADR 0018 §3: boot-observed placement facts are not written
    // into host.yaml. Keep them when adding an engine live. Explicit changes
    // remain visible to the profiles-only comparison and require a restart.
    if object
        .get("device_inventory_digest")
        .is_none_or(Value::is_null)
    {
        if let Some(digest) = running.get("device_inventory_digest") {
            object.insert("device_inventory_digest".into(), digest.clone());
        }
    }
    if let Some(devices) = object
        .get_mut("resource_policy")
        .and_then(|policy| policy.get_mut("devices"))
        .and_then(Value::as_object_mut)
    {
        for (id, device) in devices {
            if device["physical_gpu_uuid"].is_null() {
                if let Some(uuid) =
                    running["resource_policy"]["devices"][id].get("physical_gpu_uuid")
                {
                    device["physical_gpu_uuid"] = uuid.clone();
                }
            }
        }
    }
    // The start-time `local_engine` profiles, unless the document now
    // declares one of that name itself.
    let Some(running_profiles) = running["runtime_profiles"].as_object() else {
        return;
    };
    for name in crate::registration::ENVIRONMENT_PROFILES {
        let Some(profile) = running_profiles.get(*name) else {
            continue;
        };
        let profiles = object
            .entry("runtime_profiles")
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(profiles) = profiles.as_object_mut() {
            profiles
                .entry((*name).to_owned())
                .or_insert_with(|| profile.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_layer(pairs: &[(&str, &str)]) -> Result<EngineOverrides, ConfigError> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        EngineOverrides::from_env(&move |key| {
            pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
        })
    }

    // T03 T21 (owner rule 2026-09-25, SPEC §15.2): every engine setting follows
    // CLI flag > environment > YAML > default, one row per setting.
    #[test]
    fn every_engine_setting_follows_flag_env_document_default() {
        type Row = (
            &'static str,
            EngineOverrides,
            Vec<(&'static str, &'static str)>,
            Value,
            fn(&EngineSettings) -> String,
            [&'static str; 4],
        );
        let rows: Vec<Row> = vec![
            (
                "vllm",
                EngineOverrides {
                    vllm: Some("/flag/vllm".into()),
                    ..Default::default()
                },
                vec![(VLLM_BIN_ENV, "/env/vllm")],
                json!({"local_engine": {"vllm": "/yaml/vllm"}}),
                |s| format!("{:?}", s.vllm),
                [
                    "Some(\"/flag/vllm\")",
                    "Some(\"/env/vllm\")",
                    "Some(\"/yaml/vllm\")",
                    "None",
                ],
            ),
            (
                "tensorfold",
                EngineOverrides {
                    tensorfold: Some("/flag/tensorfold".into()),
                    ..Default::default()
                },
                vec![(TENSORFOLD_BIN_ENV, "/env/tensorfold")],
                json!({"local_engine": {"tensorfold": "/yaml/tensorfold"}}),
                |s| format!("{:?}", s.tensorfold),
                [
                    "Some(\"/flag/tensorfold\")",
                    "Some(\"/env/tensorfold\")",
                    "Some(\"/yaml/tensorfold\")",
                    "None",
                ],
            ),
            (
                "sglang",
                EngineOverrides {
                    sglang: Some("/flag/python".into()),
                    ..Default::default()
                },
                vec![(SGLANG_BIN_ENV, "/env/python")],
                json!({"local_engine": {"sglang": "/yaml/python"}}),
                |s| format!("{:?}", s.sglang),
                [
                    "Some(\"/flag/python\")",
                    "Some(\"/env/python\")",
                    "Some(\"/yaml/python\")",
                    "None",
                ],
            ),
            (
                "build_fingerprint",
                EngineOverrides {
                    build_fingerprint: Some("flag".into()),
                    ..Default::default()
                },
                vec![(ENGINE_FINGERPRINT_ENV, "env")],
                json!({"local_engine": {"build_fingerprint": "yaml"}}),
                |s| format!("{:?}", s.build_fingerprint),
                ["Some(\"flag\")", "Some(\"env\")", "Some(\"yaml\")", "None"],
            ),
            (
                "args",
                EngineOverrides {
                    args: Some(vec!["--flag".into()]),
                    ..Default::default()
                },
                vec![(ENGINE_ARGS_ENV, "--env  --twice")],
                json!({"local_engine": {"args": ["--yaml"]}}),
                |s| format!("{:?}", s.args),
                [
                    "[\"--flag\"]",
                    "[\"--env\", \"--twice\"]",
                    "[\"--yaml\"]",
                    "[]",
                ],
            ),
            (
                "kv_cache",
                EngineOverrides {
                    kv_cache: Some("3GiB".into()),
                    ..Default::default()
                },
                vec![(KV_CACHE_ENV, "2GiB")],
                json!({"local_engine": {"kv_cache": "1GiB"}}),
                |s| format!("{:?}", s.kv_cache),
                ["Some(\"3GiB\")", "Some(\"2GiB\")", "Some(\"1GiB\")", "None"],
            ),
            (
                "deep_park",
                EngineOverrides {
                    deep_park: Some(true),
                    ..Default::default()
                },
                vec![(DEEP_PARK_ENV, "off")],
                json!({"local_engine": {"deep_park": "on"}}),
                |s| s.deep_park.to_string(),
                ["true", "false", "true", "true"],
            ),
            (
                "trust_remote_code",
                EngineOverrides {
                    trust_remote_code: Some(false),
                    ..Default::default()
                },
                vec![(TRUST_REMOTE_CODE_ENV, "1")],
                json!({"local_engine": {"trust_remote_code": false}}),
                |s| s.trust_remote_code.to_string(),
                ["false", "true", "false", "false"],
            ),
            (
                "installation_drift",
                EngineOverrides {
                    installation_drift: Some(InstallationDrift::Warn),
                    ..Default::default()
                },
                vec![(INSTALLATION_DRIFT_ENV, "refuse")],
                json!({"local_engine": {"installation_drift": "warn"}}),
                |s| format!("{:?}", s.installation_drift),
                ["Warn", "Refuse", "Warn", "Warn"],
            ),
            (
                "runtime_dir",
                EngineOverrides {
                    runtime_dir: Some("/flag/runtime".into()),
                    ..Default::default()
                },
                vec![(RUNTIME_DIR_ENV, "/env/runtime")],
                json!({"runtime_dir": "/yaml/runtime"}),
                |s| format!("{:?}", s.runtime_dir),
                [
                    "Some(\"/flag/runtime\")",
                    "Some(\"/env/runtime\")",
                    "Some(\"/yaml/runtime\")",
                    "None",
                ],
            ),
            (
                "engine_ports",
                EngineOverrides {
                    engine_ports: Some((9000, 9009)),
                    ..Default::default()
                },
                vec![(ENGINE_PORTS_ENV, "9100-9109")],
                json!({"resource_policy": {"endpoint_port_range": {"start": 9200, "end": 9209}}}),
                |s| format!("{:?}", s.engine_ports),
                [
                    "Some((9000, 9009))",
                    "Some((9100, 9109))",
                    "Some((9200, 9209))",
                    "None",
                ],
            ),
            (
                "cuda_home",
                EngineOverrides {
                    cuda_home: Some("/flag/cuda".into()),
                    ..Default::default()
                },
                vec![(CUDA_HOME_ENV, "/env/cuda")],
                json!({"local_engine": {"cuda_home": "/yaml/cuda"}}),
                |s| format!("{:?}", s.cuda_home),
                [
                    "Some(\"/flag/cuda\")",
                    "Some(\"/env/cuda\")",
                    "Some(\"/yaml/cuda\")",
                    "None",
                ],
            ),
        ];
        for (name, flag, env, document, read, expected) in rows {
            let env = env_layer(&env).unwrap();
            let document = EngineOverrides::from_document(&document).unwrap();
            let none = EngineOverrides::default();
            let seen = [
                read(&resolve(&flag, &env, &document)),
                read(&resolve(&none, &env, &document)),
                read(&resolve(&none, &none, &document)),
                read(&resolve(&none, &none, &none)),
            ];
            assert_eq!(seen, expected.map(str::to_owned), "{name}");
        }
    }

    // T03: SPEC §15.3, a malformed value is refused with the name that
    // stated it; an empty switch export is refused, an empty path is unset.
    #[test]
    fn malformed_values_are_refused_with_their_names() {
        for (key, value) in [
            (DEEP_PARK_ENV, ""),
            (DEEP_PARK_ENV, "disabled"),
            (DEEP_PARK_ENV, "ON"),
            (INSTALLATION_DRIFT_ENV, ""),
            (INSTALLATION_DRIFT_ENV, "ignore"),
            (TRUST_REMOTE_CODE_ENV, "yes"),
            (KV_CACHE_ENV, "lots"),
            (ENGINE_PORTS_ENV, ""),
            (ENGINE_PORTS_ENV, "80-90"),
            (ENGINE_PORTS_ENV, "9000-8000"),
            (DEPRECATED_ENGINE_PORTS_ENV, "x"),
            (CUDA_HOME_ENV, "relative/cuda"),
        ] {
            let error = env_layer(&[(key, value)]).unwrap_err();
            assert_eq!(error.path, key, "{key}={value:?}");
        }
        assert_eq!(
            env_layer(&[(VLLM_BIN_ENV, ""), (KV_CACHE_ENV, "")]).unwrap(),
            EngineOverrides::default()
        );
        for (document, path) in [
            (
                json!({"local_engine": {"vllm": "relative"}}),
                "local_engine.vllm",
            ),
            (
                json!({"local_engine": {"deep_park": true}}),
                "local_engine.deep_park",
            ),
            (json!({"runtime_dir": "rel"}), "runtime_dir"),
            (
                json!({"local_engine": {"cuda_home": "cuda"}}),
                "local_engine.cuda_home",
            ),
            (
                json!({"resource_policy": {"endpoint_port_range": {"start": 80, "end": 90}}}),
                "resource_policy.endpoint_port_range",
            ),
        ] {
            let error = EngineOverrides::from_document(&document).unwrap_err();
            assert_eq!(error.path, path, "{document}");
        }
    }

    // T03: `CAPYCTL_ENGINE_PORTS` replaces the standalone-only name, which is
    // still read after it and warned about once.
    #[test]
    fn the_old_engine_ports_variable_is_a_deprecated_alias() {
        let old = env_layer(&[(DEPRECATED_ENGINE_PORTS_ENV, "9300-9309")]).unwrap();
        assert_eq!(old.engine_ports, Some((9300, 9309)));
        let both = env_layer(&[
            (DEPRECATED_ENGINE_PORTS_ENV, "9300-9309"),
            (ENGINE_PORTS_ENV, "9400-9409"),
        ])
        .unwrap();
        assert_eq!(both.engine_ports, Some((9400, 9409)));
        let get = |key: &str| (key == DEPRECATED_ENGINE_PORTS_ENV).then(|| "1-2".to_owned());
        let warnings = deprecation_warnings(&get);
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains("use CAPYCTL_ENGINE_PORTS"),
            "{warnings:?}"
        );
        assert!(deprecation_warnings(&|_| None).is_empty());
    }

    // T21 T03 (owner rule: standalone is a server plus one host): a host's
    // local_engine becomes the `local` profile built as `engine add` builds
    // one; the block is not published; a declared name is refused.
    #[test]
    fn a_host_publishes_its_local_engine_as_a_profile() {
        let mut document = json!({
            "name": "h",
            "local_engine": {"vllm": "/opt/vllm/bin/vllm"},
            "runtime_profiles": {},
        });
        let settings = resolve(
            &EngineOverrides {
                deep_park: Some(false),
                trust_remote_code: Some(true),
                engine_ports: Some((9000, 9099)),
                runtime_dir: Some("/opt/capyctl/runtime".into()),
                args: Some(vec!["--enforce-eager".into()]),
                cuda_home: Some("/usr/local/cuda-13.0".into()),
                ..Default::default()
            },
            &EngineOverrides::default(),
            &EngineOverrides::from_document(&document).unwrap(),
        );
        apply_to_host(&mut document, &settings, &|_, _| Ok("vllm 0.29.0".into())).unwrap();
        assert!(document.get("local_engine").is_none());
        let profile = &document["runtime_profiles"]["local"];
        assert_eq!(profile["executable"], "/opt/vllm/bin/vllm");
        assert_eq!(profile["build_fingerprint"], "vllm 0.29.0");
        assert_eq!(profile["security"]["deep_park"], "disabled");
        assert_eq!(profile["security"]["trust_remote_code"], true);
        assert_eq!(profile["args"], json!(["--enforce-eager"]));
        // SPEC §13.3 amendment: the stated CUDA toolkit is the profile's.
        assert_eq!(profile["cuda_home"], "/usr/local/cuda-13.0");
        assert_eq!(document["runtime_dir"], "/opt/capyctl/runtime");
        assert_eq!(
            document["resource_policy"]["endpoint_port_range"],
            json!({"start": 9000, "end": 9099})
        );
        // Both executables: two profiles; SGLang takes no argument vector.
        let mut both = json!({"name": "h"});
        let settings = resolve(
            &EngineOverrides {
                vllm: Some("/v".into()),
                sglang: Some("/s".into()),
                build_fingerprint: Some("fp".into()),
                args: Some(vec!["--x".into()]),
                ..Default::default()
            },
            &EngineOverrides::default(),
            &EngineOverrides::default(),
        );
        apply_to_host(&mut both, &settings, &|_, _| unreachable!("stated")).unwrap();
        assert_eq!(both["runtime_profiles"]["local-vllm"]["engine"], "vllm");
        assert_eq!(both["runtime_profiles"]["local-sglang"]["args"], json!([]));
        // A declared `local` is refused; a failed probe names the setting.
        let mut declared = json!({"runtime_profiles": {"local": {}}});
        let settings = resolve(
            &EngineOverrides {
                vllm: Some("/v".into()),
                ..Default::default()
            },
            &EngineOverrides::default(),
            &EngineOverrides::default(),
        );
        let error = apply_to_host(&mut declared, &settings, &|_, _| Ok("fp".into())).unwrap_err();
        assert!(error.detail.contains("profile_exists"), "{error}");
        let error =
            apply_to_host(&mut json!({}), &settings, &|_, _| Err("no version".into())).unwrap_err();
        assert!(error.detail.contains("build_fingerprint"), "{error}");
        // A host has no generated deployment to give a KV cache.
        let settings = resolve(
            &EngineOverrides {
                kv_cache: Some("1GiB".into()),
                ..Default::default()
            },
            &EngineOverrides::default(),
            &EngineOverrides::default(),
        );
        assert!(apply_to_host(&mut json!({}), &settings, &|_, _| Ok("fp".into())).is_err());
        // Nothing named: the document is unchanged apart from the block.
        let mut plain = json!({"name": "h", "runtime_profiles": {"p": {"engine": "vllm"}}});
        let before = plain.clone();
        apply_to_host(
            &mut plain,
            &resolve(
                &EngineOverrides::default(),
                &EngineOverrides::default(),
                &EngineOverrides::default(),
            ),
            &|_, _| unreachable!(),
        )
        .unwrap();
        assert_eq!(plain, before);
    }

    fn defaults() -> EngineSettings {
        resolve(
            &Default::default(),
            &Default::default(),
            &Default::default(),
        )
    }

    // T41 T03 (ADR 0023 §2): one executable is `local`; several are named by
    // their engine.
    #[test]
    fn tensorfold_installations_are_named_like_the_others() {
        let one = EngineSettings {
            tensorfold: Some("/t/bin/tensorfold".into()),
            ..defaults()
        };
        assert_eq!(
            one.installations(),
            vec![("local", Engine::Tensorfold, "/t/bin/tensorfold".into())]
        );
        let two = EngineSettings {
            vllm: Some("/v/bin/vllm".into()),
            tensorfold: Some("/t/bin/tensorfold".into()),
            ..defaults()
        };
        assert_eq!(
            two.installations(),
            vec![
                ("local-vllm", Engine::Vllm, "/v/bin/vllm".into()),
                (
                    "local-tensorfold",
                    Engine::Tensorfold,
                    "/t/bin/tensorfold".into()
                ),
            ]
        );
    }

    // T41 (ADR 0023 §2, §6): a host's own TensorFold needs its
    // build toolchain and never parks, whatever deep_park says.
    #[test]
    fn a_host_tensorfold_is_checked_and_never_parks() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let settings = EngineSettings {
            tensorfold: Some(bin.join("tensorfold")),
            build_fingerprint: Some("0.6.0".into()),
            deep_park: true,
            ..defaults()
        };
        // No system directory is searched, so the refusal does not depend on
        // what this machine has installed.
        let search = crate::toolchain::ToolchainSearch {
            system: String::new(),
            default_cuda_home: dir.path().join("no-cuda"),
        };
        let error = apply_to_host_with(&mut json!({}), &settings, &|_, _| unreachable!(), &search)
            .unwrap_err();
        assert!(error.detail.contains("toolchain_missing"), "{error}");
        assert!(error.detail.contains("ninja"), "{error}");
        assert!(error.detail.contains("--cuda-home"), "{error}");
        use std::os::unix::fs::PermissionsExt;
        for tool in ["ninja", "nvcc", "c++"] {
            let path = bin.join(tool);
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut document = json!({});
        apply_to_host_with(&mut document, &settings, &|_, _| unreachable!(), &search).unwrap();
        let profile = &document["runtime_profiles"]["local"];
        assert_eq!(profile["engine"], "tensorfold");
        assert_eq!(profile["security"]["deep_park"], "disabled");
    }

    // T03 (ADR 0018 §3): a live reload keeps what the host resolved at start.
    #[test]
    fn a_reload_carries_the_start_settings_over() {
        let running = json!({
            "model_store": {"path": "/m"},
            "runtime_dir": "/r",
            "resource_policy": {"max_parked": 2, "endpoint_port_range": {"start": 9000, "end": 9099}},
            "runtime_profiles": {"local": {"engine": "vllm"}, "p": {"engine": "vllm"}},
        });
        let mut reloaded = json!({
            "local_engine": {"vllm": "/v"},
            "resource_policy": {"max_parked": 2},
            "runtime_profiles": {"p": {"engine": "vllm"}, "q": {"engine": "sglang"}},
        });
        carry_start_settings(&running, &mut reloaded);
        assert_eq!(
            reloaded,
            json!({
                "model_store": {"path": "/m"},
                "runtime_dir": "/r",
                "resource_policy": {"max_parked": 2, "endpoint_port_range": {"start": 9000, "end": 9099}},
                "runtime_profiles": {
                    "local": {"engine": "vllm"}, "p": {"engine": "vllm"}, "q": {"engine": "sglang"}
                },
            })
        );
    }
}
