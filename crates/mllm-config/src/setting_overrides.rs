//! Owner decision 2026-09-25 (SPEC §15.2: run-time role flags override
//! ordinary settings under one documented precedence): every YAML setting of a
//! role document is settable three ways. Besides the named flags and variables
//! of the settings that have them (`crate::engine_settings`,
//! `crate::model_settings`, the listener and state-root forms), a generic
//! override names any setting by its path in the document:
//!
//! - CLI: `--set path.to.key=value` (repeatable) on `start server|host|
//!   standalone`, `validate config` and `config show`;
//! - environment: `MLLM_SET__PATH__TO__KEY=value`, a double underscore
//!   separating the path's keys, matched case-insensitively against the
//!   schema.
//!
//! Precedence for one setting: `--set` > `MLLM_SET__…` > its named flag >
//! its named variable > YAML > default. When a setting has a named form and a
//! generic override at once, the two must agree, or the start is refused with
//! both named ([`SettingOverrides::check_named`]).
//!
//! A value is typed exactly as a plain YAML scalar is (`true`, `30`, `30s`,
//! `0.0.0.0:8443`), a list is written in YAML flow form (`[a, b]`), and the
//! document is then validated exactly as the YAML would be: an override is
//! the document with one value changed, never a separate path into the role.
//! An unknown path is refused with the valid paths nearest the typo. A
//! secret-bearing setting (a key, token or credential reference) is refused on
//! the command line, where it would show in the process list and the shell
//! history; it is set in the document or the environment only.

use serde_json::{Map, Value};

use crate::error::{ConfigError, ConfigErrorCode};
use crate::schema::{schema, ConfigKind, FieldSpec};

/// The environment prefix of a generic override.
pub const SET_ENV_PREFIX: &str = "MLLM_SET__";

/// Where an effective value came from, weakest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Source {
    Default,
    Yaml,
    Env,
    Flag,
    Set,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Default => "default",
            Source::Yaml => "yaml",
            Source::Env => "env",
            Source::Flag => "flag",
            Source::Set => "set",
        }
    }
}

/// One generic override, resolved against the schema.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingOverride {
    /// The canonical dotted path (schema spelling).
    pub path: String,
    segments: Vec<String>,
    /// The value, typed as YAML types it.
    pub value: Value,
    /// [`Source::Set`] for `--set`, [`Source::Env`] for `MLLM_SET__…`.
    pub source: Source,
    /// What stated it: `--set <path>` or the variable's name.
    pub origin: String,
}

/// A value stated by a setting's named flag or variable, in its YAML form,
/// for the agreement check and for `config show`.
#[derive(Debug, Clone, PartialEq)]
pub struct NamedValue {
    pub path: String,
    pub value: Value,
    /// [`Source::Flag`] or [`Source::Env`].
    pub source: Source,
    /// The flag or variable, e.g. `--deep-park` or `MLLM_DEEP_PARK`.
    pub origin: String,
}

/// The generic overrides of one run for one document kind: the environment's,
/// then the command line's, one effective value per path.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingOverrides {
    kind: ConfigKind,
    /// Every override, environment first, so a later entry wins.
    all: Vec<SettingOverride>,
}

fn refuse(path: &str, detail: impl Into<String>) -> ConfigError {
    ConfigError::new(ConfigErrorCode::UnsupportedCombination, path, detail)
}

/// Kinds a generic override applies to: the role documents.
pub fn is_role_kind(kind: ConfigKind) -> bool {
    matches!(
        kind,
        ConfigKind::Server | ConfigKind::Host | ConfigKind::Standalone
    )
}

impl SettingOverrides {
    /// No override.
    pub fn none(kind: ConfigKind) -> Self {
        Self {
            kind,
            all: Vec::new(),
        }
    }

    /// The overrides of `sets` (each `path=value`, from `--set`) and of the
    /// `MLLM_SET__…` pairs in `env`. Every path is resolved against the
    /// schema of `kind`; an unknown one, a block, a malformed value or a
    /// secret on the command line is refused.
    pub fn parse(
        kind: ConfigKind,
        sets: &[String],
        env: &[(String, String)],
    ) -> Result<Self, ConfigError> {
        if !is_role_kind(kind) {
            if sets.is_empty() {
                // `MLLM_SET__…` names role settings; another kind ignores it.
                return Ok(Self::none(kind));
            }
            return Err(refuse(
                "--set",
                format!(
                    "generic overrides apply to role documents (server, host, standalone), \
                     not to a {} document",
                    kind.as_str()
                ),
            ));
        }
        let mut all = Vec::new();
        let mut env: Vec<&(String, String)> = env
            .iter()
            .filter(|(key, _)| key.starts_with(SET_ENV_PREFIX))
            .collect();
        env.sort();
        for (key, raw) in env {
            let segments: Vec<&str> = key[SET_ENV_PREFIX.len()..].split("__").collect();
            all.push(resolve(kind, &segments, raw, Source::Env, key)?);
        }
        for set in sets {
            let (path, raw) = set.split_once('=').ok_or_else(|| {
                refuse("--set", format!("must be path.to.key=value; got {set:?}"))
            })?;
            let segments: Vec<&str> = path.split('.').collect();
            let resolved = resolve(kind, &segments, raw, Source::Set, &format!("--set {path}"))?;
            if is_secret(&resolved.path) {
                return Err(refuse(
                    &resolved.path,
                    "names a secret; secrets are never command-line values (they show in the \
                     process list and shell history). State it in the document or in the \
                     environment (MLLM_SET__…) instead",
                ));
            }
            all.push(resolved);
        }
        Ok(Self { kind, all })
    }

    /// As [`Self::parse`], with the process environment.
    pub fn from_process(kind: ConfigKind, sets: &[String]) -> Result<Self, ConfigError> {
        let env: Vec<(String, String)> = std::env::vars_os()
            .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
            .filter(|(key, _)| key.starts_with(SET_ENV_PREFIX))
            .collect();
        Self::parse(kind, sets, &env)
    }

    pub fn kind(&self) -> ConfigKind {
        self.kind
    }

    pub fn is_empty(&self) -> bool {
        self.all.is_empty()
    }

    /// The effective override of each path (`--set` over `MLLM_SET__…`, the
    /// last `--set` of a path over earlier ones), in path order.
    pub fn effective(&self) -> Vec<&SettingOverride> {
        let mut winners: Vec<&SettingOverride> = Vec::new();
        for item in &self.all {
            match winners.iter_mut().find(|w| w.path == item.path) {
                Some(slot) => *slot = item,
                None => winners.push(item),
            }
        }
        winners.sort_by(|a, b| a.path.cmp(&b.path));
        winners
    }

    /// The effective override of `path`, if any.
    pub fn get(&self, path: &str) -> Option<&SettingOverride> {
        self.all.iter().rev().find(|item| item.path == path)
    }

    /// Write every effective value into `document`, creating the blocks on
    /// the way. The result still has to be validated
    /// ([`Self::apply_and_validate`] or the role's own parse).
    pub fn apply(&self, document: &mut Value) -> Result<(), ConfigError> {
        for item in self.effective() {
            let mut node = &mut *document;
            let (last, parents) = item.segments.split_last().expect("a path has a key");
            for (depth, key) in parents.iter().enumerate() {
                let map = node.as_object_mut().ok_or_else(|| {
                    refuse(
                        &item.segments[..depth].join("."),
                        format!("is not a mapping, so {} cannot be set", item.origin),
                    )
                })?;
                let key = existing_key(map, key);
                node = map.entry(key).or_insert_with(|| Value::Object(Map::new()));
                if node.is_null() {
                    *node = Value::Object(Map::new());
                }
            }
            let map = node.as_object_mut().ok_or_else(|| {
                refuse(
                    &parents.join("."),
                    format!("is not a mapping, so {} cannot be set", item.origin),
                )
            })?;
            let key = existing_key(map, last);
            map.insert(key, item.value.clone());
        }
        Ok(())
    }

    /// [`Self::apply`], then the strict schema check of the document kind, so
    /// an override is validated exactly as the YAML value would be. An error
    /// at an overridden path names the override.
    pub fn apply_and_validate(&self, mut document: Value) -> Result<Value, ConfigError> {
        self.apply(&mut document)?;
        crate::parse_strict_value(self.kind, document).map_err(|error| self.annotate(error))
    }

    /// `error`, naming the override that stated its path when one did.
    pub fn annotate(&self, mut error: ConfigError) -> ConfigError {
        if let Some(item) = self.all.iter().rev().find(|item| {
            error.path == item.path
                || error.path.starts_with(&format!("{}.", item.path))
                || error.path.starts_with(&format!("{}[", item.path))
        }) {
            error.detail = format!("{} (set by {})", error.detail, item.origin);
        }
        error
    }

    /// Owner decision 2026-09-25: a setting stated both by its named flag or
    /// variable and by a generic override must agree; otherwise the start is
    /// refused naming both.
    pub fn check_named(&self, named: &[NamedValue]) -> Result<(), ConfigError> {
        for value in named {
            let Some(item) = self.get(&value.path) else {
                continue;
            };
            if !agree(&value.path, &item.value, &value.value) {
                return Err(ConfigError::new(
                    ConfigErrorCode::ConflictingArgs,
                    value.path.clone(),
                    format!(
                        "conflict: {} states {} but {} states {}; state one of them, or make \
                         them agree",
                        item.origin,
                        display(&item.value),
                        value.origin,
                        display(&value.value),
                    ),
                ));
            }
        }
        Ok(())
    }
}

/// `value` as a person reads it: strings unquoted.
pub fn display(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// The spellings the roles read as one value: `disabled` is `denied` for a
/// model-source switch, and `1`/`on` are `true` for a boolean switch.
fn canonical(path: &str, value: &Value) -> String {
    let text = display(value);
    let leaf = path.rsplit('.').next().unwrap_or(path);
    let under_sources = path.contains("model_sources.");
    match (leaf, text.as_str()) {
        ("huggingface" | "http", "disabled") if under_sources => "denied".into(),
        ("trust_remote_code" | "timing_header", "1" | "on") => "true".into(),
        ("trust_remote_code" | "timing_header", "0" | "off") => "false".into(),
        _ => text,
    }
}

fn agree(path: &str, generic: &Value, named: &Value) -> bool {
    generic == named || canonical(path, generic) == canonical(path, named)
}

/// The key of `map` matching `key` case-insensitively, else `key` itself.
fn existing_key(map: &Map<String, Value>, key: &str) -> String {
    map.keys()
        .find(|existing| existing.eq_ignore_ascii_case(key))
        .cloned()
        .unwrap_or_else(|| key.to_owned())
}

/// Whether the setting at `path` holds or names a secret: a key, a token, a
/// password or a credential reference, or an engine environment value.
pub fn is_secret(path: &str) -> bool {
    let segments: Vec<&str> = path.split('.').collect();
    if segments.len() >= 2 && segments[segments.len() - 2] == "env" {
        return true;
    }
    let leaf = segments.last().copied().unwrap_or_default();
    leaf.split('_')
        .any(|word| matches!(word, "key" | "token" | "secret" | "password" | "credential"))
}

enum Leaf {
    Scalar,
    Seq,
}

/// What `spec` is at the end of a path.
fn leaf_of(spec: &FieldSpec) -> Option<Leaf> {
    match spec {
        FieldSpec::Scalar
        | FieldSpec::Unit
        | FieldSpec::Bytes
        | FieldSpec::Duration
        | FieldSpec::ScalarOrStruct(_)
        | FieldSpec::Moved(_) => Some(Leaf::Scalar),
        FieldSpec::Seq(_) => Some(Leaf::Seq),
        FieldSpec::Struct(_) | FieldSpec::RequiredStruct(_) | FieldSpec::MapOf(_) => None,
    }
}

/// The top-level fields that are not settings.
const NOT_SETTINGS: &[&str] = &["schema_version", "kind"];

fn resolve(
    kind: ConfigKind,
    segments: &[&str],
    raw: &str,
    source: Source,
    origin: &str,
) -> Result<SettingOverride, ConfigError> {
    let given = segments.join(".");
    let unknown = || unknown_path(kind, &given, origin);
    if segments.iter().any(|segment| segment.is_empty()) {
        return Err(unknown());
    }
    let mut fields: &[(&str, FieldSpec)] = schema(kind).fields;
    let mut spec: Option<&FieldSpec> = None;
    let mut canonical: Vec<String> = Vec::new();
    for (index, segment) in segments.iter().enumerate() {
        let current = match spec {
            None => {
                let (name, field) = fields
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(segment))
                    .ok_or_else(unknown)?;
                if index == 0 && NOT_SETTINGS.contains(name) {
                    return Err(refuse(
                        name,
                        format!("`{name}` identifies the document; it is not a setting"),
                    ));
                }
                canonical.push((*name).to_owned());
                field
            }
            Some(FieldSpec::MapOf(entry)) => {
                // An entry name (a listener, a profile, a label): kept as
                // written on the command line; a variable's is lowercased.
                canonical.push(if source == Source::Env {
                    segment.to_ascii_lowercase()
                } else {
                    (*segment).to_owned()
                });
                entry
            }
            Some(
                FieldSpec::Struct(inner)
                | FieldSpec::RequiredStruct(inner)
                | FieldSpec::ScalarOrStruct(inner),
            ) => {
                fields = inner;
                let (name, field) = fields
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(segment))
                    .ok_or_else(unknown)?;
                canonical.push((*name).to_owned());
                field
            }
            Some(_) => return Err(unknown()),
        };
        spec = Some(current);
        if let FieldSpec::Struct(inner) | FieldSpec::RequiredStruct(inner) = current {
            fields = inner;
        }
    }
    let path = canonical.join(".");
    let spec = spec.ok_or_else(unknown)?;
    let value = match leaf_of(spec) {
        None => {
            return Err(ConfigError::new(
                ConfigErrorCode::UnknownField,
                path.clone(),
                format!(
                    "{origin} names a block, not a setting; set one of its fields: {}",
                    children(spec, &path).join(", ")
                ),
            ))
        }
        Some(Leaf::Scalar) => {
            let value = crate::strict_yaml::plain_scalar(raw);
            if value.is_null() {
                return Err(refuse(
                    &path,
                    format!("{origin} needs a value (path=value)"),
                ));
            }
            value
        }
        Some(Leaf::Seq) => match crate::strict_yaml::build_value(raw) {
            Ok(Value::Array(items)) => Value::Array(items),
            _ => {
                return Err(refuse(
                    &path,
                    format!("{origin} must be a YAML list such as [a, b]; got {raw:?}"),
                ))
            }
        },
    };
    Ok(SettingOverride {
        path,
        segments: canonical,
        value,
        source,
        origin: origin.to_owned(),
    })
}

/// The settings one level under the block `spec` at `path`.
fn children(spec: &FieldSpec, path: &str) -> Vec<String> {
    match spec {
        FieldSpec::Struct(fields) | FieldSpec::RequiredStruct(fields) => fields
            .iter()
            .map(|(name, _)| format!("{path}.{name}"))
            .collect(),
        FieldSpec::MapOf(_) => vec![format!("{path}.<name>")],
        _ => Vec::new(),
    }
}

/// Every settable path of `kind`, an entry name written `<name>` (the
/// listeners by their names).
pub fn setting_paths(kind: ConfigKind) -> Vec<String> {
    fn walk(fields: &[(&str, FieldSpec)], prefix: &str, out: &mut Vec<String>) {
        for (name, spec) in fields {
            let path = if prefix.is_empty() {
                (*name).to_owned()
            } else {
                format!("{prefix}.{name}")
            };
            if prefix.is_empty() && NOT_SETTINGS.contains(name) {
                continue;
            }
            visit(spec, &path, out);
        }
    }
    fn visit(spec: &FieldSpec, path: &str, out: &mut Vec<String>) {
        match spec {
            FieldSpec::Struct(inner) | FieldSpec::RequiredStruct(inner) => walk(inner, path, out),
            FieldSpec::MapOf(entry) => {
                let names: &[&str] = if path.ends_with("listeners") {
                    &["management", "inference", "bootstrap", "control"]
                } else {
                    &["<name>"]
                };
                for name in names {
                    visit(entry, &format!("{path}.{name}"), out);
                }
            }
            FieldSpec::Moved(_) => {}
            _ => out.push(path.to_owned()),
        }
    }
    let mut out = Vec::new();
    walk(schema(kind).fields, "", &mut out);
    out
}

fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let current = row[j + 1];
            row[j + 1] = if ca == *cb {
                previous
            } else {
                1 + previous.min(row[j]).min(row[j + 1])
            };
            previous = current;
        }
    }
    row[b.len()]
}

/// The valid paths nearest `given`, closest first (at most five).
pub fn nearest_paths(kind: ConfigKind, given: &str) -> Vec<String> {
    let given = given.to_ascii_lowercase();
    let mut ranked: Vec<(usize, String)> = setting_paths(kind)
        .into_iter()
        .map(|path| (distance(&given, &path), path))
        .collect();
    ranked.sort();
    ranked.into_iter().take(5).map(|(_, path)| path).collect()
}

fn unknown_path(kind: ConfigKind, given: &str, origin: &str) -> ConfigError {
    ConfigError::new(
        ConfigErrorCode::UnknownField,
        given,
        format!(
            "unknown {} setting (named by {origin}); valid settings near it: {}",
            kind.as_str(),
            nearest_paths(kind, given).join(", ")
        ),
    )
}

/// Where a setting with named forms lives, relative to its block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// A host setting: top level in a host document, under `host:` in a
    /// standalone one.
    Host,
    /// A server setting: top level in a server document, under `server:` in a
    /// standalone one.
    Server,
    /// A standalone setting, top level.
    Standalone,
}

/// Every setting with a named flag or variable: its path in its block, its
/// scope, its flag and its variable (`docs/operations/configuration.md`).
const NAMED_FORMS: &[(&str, Scope, &str, &str)] = &[
    (
        "model_store.path",
        Scope::Host,
        "--models-root",
        "MLLM_MODELS_ROOT",
    ),
    (
        "model_sources.huggingface",
        Scope::Host,
        "--model-sources",
        "MLLM_MODEL_SOURCES",
    ),
    (
        "model_sources.http",
        Scope::Host,
        "--model-sources",
        "MLLM_MODEL_SOURCES",
    ),
    (
        "model_sources.max_bytes",
        Scope::Host,
        "--model-sources-max",
        "MLLM_MODEL_SOURCES_MAX",
    ),
    (
        "model_sources.path",
        Scope::Host,
        "--model-sources-path",
        "MLLM_MODEL_SOURCES_PATH",
    ),
    (
        "model_sources.huggingface_endpoint",
        Scope::Host,
        "--hf-endpoint",
        "MLLM_HF_ENDPOINT",
    ),
    (
        "local_engine.vllm",
        Scope::Host,
        "--vllm-bin",
        "MLLM_VLLM_BIN",
    ),
    (
        "local_engine.sglang",
        Scope::Host,
        "--sglang-bin",
        "MLLM_SGLANG_BIN",
    ),
    (
        "local_engine.build_fingerprint",
        Scope::Host,
        "--engine-fingerprint",
        "MLLM_ENGINE_FINGERPRINT",
    ),
    (
        "local_engine.args",
        Scope::Host,
        "--engine-args",
        "MLLM_ENGINE_ARGS",
    ),
    (
        "local_engine.kv_cache",
        Scope::Host,
        "--kv-cache",
        "MLLM_KV_CACHE_BYTES",
    ),
    (
        "local_engine.deep_park",
        Scope::Host,
        "--deep-park",
        "MLLM_DEEP_PARK",
    ),
    (
        "local_engine.trust_remote_code",
        Scope::Host,
        "--trust-remote-code",
        "MLLM_TRUST_REMOTE_CODE",
    ),
    (
        "local_engine.installation_drift",
        Scope::Host,
        "--installation-drift",
        "MLLM_INSTALLATION_DRIFT",
    ),
    (
        "local_engine.cuda_home",
        Scope::Host,
        "--cuda-home",
        "MLLM_CUDA_HOME",
    ),
    (
        "runtime_dir",
        Scope::Host,
        "--runtime-dir",
        "MLLM_RUNTIME_DIR",
    ),
    (
        "resource_policy.endpoint_port_range.start",
        Scope::Host,
        "--engine-ports",
        "MLLM_ENGINE_PORTS",
    ),
    (
        "resource_policy.endpoint_port_range.end",
        Scope::Host,
        "--engine-ports",
        "MLLM_ENGINE_PORTS",
    ),
    (
        "listeners.inference.bind",
        Scope::Server,
        "--listen",
        "MLLM_INFERENCE_ADDR",
    ),
    (
        "listeners.inference.authentication",
        Scope::Server,
        "--no-inference-auth",
        "MLLM_INFERENCE_AUTH",
    ),
    (
        "server.listeners.management.bind",
        Scope::Standalone,
        "--management-listen",
        "MLLM_MANAGEMENT_ADDR",
    ),
    (
        "state_dir",
        Scope::Standalone,
        "--state-dir",
        "MLLM_STATE_DIR",
    ),
];

/// The full path of a named setting in a `kind` document, or `None` when
/// that kind has no such setting.
fn named_path(kind: ConfigKind, path: &str, scope: Scope) -> Option<String> {
    match (kind, scope) {
        (ConfigKind::Host, Scope::Host) | (ConfigKind::Server, Scope::Server) => {
            Some(path.to_owned())
        }
        (ConfigKind::Standalone, Scope::Host) => Some(format!("host.{path}")),
        (ConfigKind::Standalone, Scope::Server) => Some(format!("server.{path}")),
        (ConfigKind::Standalone, Scope::Standalone) => Some(path.to_owned()),
        _ => None,
    }
}

/// The flag and variable naming the setting at `path` of a `kind` document.
pub fn named_forms(kind: ConfigKind, path: &str) -> Option<(&'static str, &'static str)> {
    NAMED_FORMS.iter().find_map(|(suffix, scope, flag, env)| {
        (named_path(kind, suffix, *scope).as_deref() == Some(path)).then_some((*flag, *env))
    })
}

/// One layer of named values (the flags, or the variables) in their YAML
/// form. `None` is "this layer does not state it".
#[derive(Debug, Clone, Default)]
pub struct NamedLayer {
    pub models: crate::model_settings::ModelOverrides,
    pub engines: crate::engine_settings::EngineOverrides,
    /// `--listen` / `MLLM_INFERENCE_ADDR`.
    pub inference_bind: Option<String>,
    /// `--no-inference-auth` (`none`) / `MLLM_INFERENCE_AUTH`.
    pub inference_auth: Option<String>,
    /// `--management-listen` / `MLLM_MANAGEMENT_ADDR` (standalone).
    pub management_bind: Option<String>,
    /// `--state-dir` / `MLLM_STATE_DIR` (the standalone state root).
    pub state_dir: Option<std::path::PathBuf>,
}

impl NamedLayer {
    /// The values this layer states for a `kind` document, as
    /// [`NamedValue`]s from `source` ([`Source::Flag`] or [`Source::Env`]).
    pub fn values(&self, kind: ConfigKind, source: Source) -> Vec<NamedValue> {
        use crate::effective::InstallationDrift;
        use crate::model_source::SourceSwitch;
        let path_text = |path: &Option<std::path::PathBuf>| {
            path.as_ref()
                .map(|path| Value::String(path.to_string_lossy().into_owned()))
        };
        let switch = self.models.sources.map(|switch| {
            Value::String(
                match switch {
                    SourceSwitch::Allowed => "allowed",
                    SourceSwitch::Denied => "denied",
                }
                .into(),
            )
        });
        let engines = &self.engines;
        let stated: Vec<(&str, Option<Value>)> = vec![
            ("model_store.path", path_text(&self.models.models_root)),
            ("model_sources.huggingface", switch.clone()),
            ("model_sources.http", switch),
            (
                "model_sources.max_bytes",
                self.models.sources_max.clone().map(Value::String),
            ),
            ("model_sources.path", path_text(&self.models.sources_path)),
            (
                "model_sources.huggingface_endpoint",
                self.models.hf_endpoint.clone().map(Value::String),
            ),
            ("local_engine.vllm", path_text(&engines.vllm)),
            ("local_engine.sglang", path_text(&engines.sglang)),
            (
                "local_engine.build_fingerprint",
                engines.build_fingerprint.clone().map(Value::String),
            ),
            (
                "local_engine.args",
                engines
                    .args
                    .as_ref()
                    .map(|args| Value::Array(args.iter().cloned().map(Value::String).collect())),
            ),
            (
                "local_engine.kv_cache",
                engines.kv_cache.clone().map(Value::String),
            ),
            (
                "local_engine.deep_park",
                engines
                    .deep_park
                    .map(|on| Value::String(if on { "on" } else { "off" }.into())),
            ),
            (
                "local_engine.trust_remote_code",
                engines.trust_remote_code.map(Value::Bool),
            ),
            (
                "local_engine.installation_drift",
                engines.installation_drift.map(|drift| {
                    Value::String(
                        match drift {
                            InstallationDrift::Warn => "warn",
                            InstallationDrift::Refuse => "refuse",
                        }
                        .into(),
                    )
                }),
            ),
            ("local_engine.cuda_home", path_text(&engines.cuda_home)),
            ("runtime_dir", path_text(&engines.runtime_dir)),
            (
                "resource_policy.endpoint_port_range.start",
                engines.engine_ports.map(|(start, _)| Value::from(start)),
            ),
            (
                "resource_policy.endpoint_port_range.end",
                engines.engine_ports.map(|(_, end)| Value::from(end)),
            ),
            (
                "listeners.inference.bind",
                self.inference_bind.clone().map(Value::String),
            ),
            (
                "listeners.inference.authentication",
                self.inference_auth.clone().map(Value::String),
            ),
            (
                "server.listeners.management.bind",
                self.management_bind.clone().map(Value::String),
            ),
            ("state_dir", path_text(&self.state_dir)),
        ];
        stated
            .into_iter()
            .filter_map(|(suffix, value)| {
                let value = value?;
                let (_, scope, flag, env) =
                    NAMED_FORMS.iter().find(|(name, ..)| *name == suffix)?;
                Some(NamedValue {
                    path: named_path(kind, suffix, *scope)?,
                    value,
                    source,
                    origin: if source == Source::Flag { flag } else { env }.to_string(),
                })
            })
            .collect()
    }
}

fn secs(duration: std::time::Duration) -> Value {
    Value::String(format!("{}s", duration.as_secs()))
}

/// The defaults of the settings a `kind` role applies when nothing states
/// them, for `mllm config show`: `home` locates `~/models`, `state_root` the
/// standalone state directories. A setting whose default is "off" or "none"
/// (an idle timer, an engine executable) is not listed.
pub fn defaults(
    kind: ConfigKind,
    home: Option<&std::path::Path>,
    state_root: &std::path::Path,
) -> Vec<(String, Value)> {
    use crate::remote_roles::{
        DEFAULT_DRAIN_TIMEOUT, DEFAULT_HEARTBEAT_LOST_AFTER, DEFAULT_HEARTBEAT_SUSPEND_AFTER,
        DEFAULT_LOAD_REPORT_INTERVAL, DEFAULT_SWITCH_DRAIN_TIMEOUT,
    };
    let host_block = |prefix: &str| -> Vec<(String, Value)> {
        let (start, end) = crate::engine_settings::DEFAULT_ENGINE_PORTS;
        let mut out = vec![
            ("model_sources.huggingface", Value::from("allowed")),
            ("model_sources.http", Value::from("allowed")),
            (
                "model_sources.max_bytes",
                Value::from(format!(
                    "{}GiB",
                    crate::model_source::DEFAULT_SOURCES_MAX_BYTES >> 30
                )),
            ),
            (
                "model_sources.huggingface_endpoint",
                Value::from("https://huggingface.co"),
            ),
            ("local_engine.deep_park", Value::from("on")),
            ("local_engine.trust_remote_code", Value::from(false)),
            ("local_engine.installation_drift", Value::from("warn")),
            (
                "resource_policy.endpoint_port_range.start",
                Value::from(start),
            ),
            ("resource_policy.endpoint_port_range.end", Value::from(end)),
        ];
        if let Some(root) = crate::model_settings::default_models_root(home) {
            out.push((
                "model_store.path",
                Value::from(root.to_string_lossy().into_owned()),
            ));
        }
        out.into_iter()
            .map(|(path, value)| (format!("{prefix}{path}"), value))
            .collect()
    };
    let server_block = |prefix: &str| -> Vec<(String, Value)> {
        [
            (
                "switching.drain_timeout",
                secs(DEFAULT_SWITCH_DRAIN_TIMEOUT),
            ),
            ("observability.timing_header", Value::from(false)),
            (
                "listeners.inference.bind",
                Value::from(crate::standalone::DEFAULT_INFERENCE_BIND),
            ),
            ("listeners.inference.authentication", Value::from("api_key")),
        ]
        .into_iter()
        .map(|(path, value)| (format!("{prefix}{path}"), value))
        .collect()
    };
    let mut out = vec![(
        "shutdown.drain_timeout".to_owned(),
        secs(DEFAULT_DRAIN_TIMEOUT),
    )];
    match kind {
        ConfigKind::Server => {
            out.extend(server_block(""));
            out.push((
                "control.heartbeat_suspend_after".into(),
                secs(DEFAULT_HEARTBEAT_SUSPEND_AFTER),
            ));
            out.push((
                "control.heartbeat_lost_after".into(),
                secs(DEFAULT_HEARTBEAT_LOST_AFTER),
            ));
        }
        ConfigKind::Host => {
            out.extend(host_block(""));
            out.push((
                "load_report_interval".into(),
                secs(DEFAULT_LOAD_REPORT_INTERVAL),
            ));
        }
        ConfigKind::Standalone => {
            let root = state_root.to_string_lossy().into_owned();
            out.push(("state_dir".into(), Value::from(root.clone())));
            out.push((
                "server.state_dir".into(),
                Value::from(format!("{root}/server")),
            ));
            out.push(("host.state_dir".into(), Value::from(format!("{root}/host"))));
            out.push((
                "server.listeners.management.bind".into(),
                Value::from(crate::standalone::DEFAULT_MANAGEMENT_BIND),
            ));
            out.extend(server_block("server."));
            out.extend(host_block("host."));
        }
        _ => return Vec::new(),
    }
    out
}

/// Every leaf of `document` (a list is one leaf), by dotted path, without the
/// document's identity (`schema_version`, `kind`).
pub fn leaves(document: &Value) -> Vec<(String, Value)> {
    fn walk(value: &Value, path: &str, out: &mut Vec<(String, Value)>) {
        match value {
            Value::Object(map) if !map.is_empty() => {
                for (key, child) in map {
                    let child_path = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    if path.is_empty() && NOT_SETTINGS.contains(&key.as_str()) {
                        continue;
                    }
                    walk(child, &child_path, out);
                }
            }
            Value::Object(_) => {}
            other if !path.is_empty() => out.push((path.to_owned(), other.clone())),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(document, "", &mut out);
    out
}

#[cfg(test)]
mod tests;
