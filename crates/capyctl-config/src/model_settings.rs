//! Owner decisions 2026-09-25: where a host keeps models, and whether it may
//! download them. Implemented once and used by every role (standalone behaves
//! as a server plus one host), so a host and standalone resolve both settings
//! by the same rule.
//!
//! - The models directory (`model_store.path`), which anchors a relative local
//!   model path (SPEC §7), defaults to `~/models`. It is stated three ways:
//!   `--models-root`, `CAPYCTL_MODELS_ROOT`, or `model_store.path` in the YAML
//!   document.
//! - Hugging Face and HTTP model sources (ADR 0008) are allowed by default on
//!   every host, with a 500 GiB ceiling for the sources store
//!   ([`crate::model_source::DEFAULT_SOURCES_MAX_BYTES`]). They are switched
//!   with `--model-sources allowed|disabled`, `CAPYCTL_MODEL_SOURCES`, or the
//!   document's `model_sources.huggingface` / `model_sources.http`; the
//!   ceiling with `--model-sources-max`, `CAPYCTL_MODEL_SOURCES_MAX`, or
//!   `model_sources.max_bytes`. An explicit `denied` (or `disabled`) in the
//!   document wins over the default.
//! - Downloads live under the models directory, `<model_store>/sources`
//!   (e.g. `~/models/sources`), so copies downloaded before an upgrade are
//!   reused; `model_sources.path` names another directory (downloads then
//!   live in `<path>/sources`). It is stated three ways too:
//!   `--model-sources-path`, `CAPYCTL_MODEL_SOURCES_PATH`, `model_sources.path`.
//! - The Hugging Face endpoint downloads use (owner rule 2026-09-25, every
//!   setting three ways): `--hf-endpoint`, `CAPYCTL_HF_ENDPOINT` (else the
//!   Hugging Face tools' own `HF_ENDPOINT`), or
//!   `model_sources.huggingface_endpoint`; default `https://huggingface.co`.
//!   A host fetches over HTTPS only, unless it approves plain `http://` URLs
//!   for `http` sources (ADR 0008 amendment 2026-10-08):
//!   `--model-sources-plain-http allowed|disabled`,
//!   `CAPYCTL_MODEL_SOURCES_PLAIN_HTTP`, or `model_sources.plain_http`;
//!   default `denied`.
//!
//! Precedence, for every setting: CLI flag > environment > YAML > default.
//! The resolved values are written into the host document before it is
//! published, so the server resolves a deployment against exactly what the
//! host will enforce.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::model_source::{ModelSourcePolicy, RawModelSources, SourceSwitch};
use crate::{ConfigError, ConfigErrorCode};

/// The variable naming the models directory.
pub const MODELS_ROOT_ENV: &str = "CAPYCTL_MODELS_ROOT";
/// The variable switching Hugging Face and HTTP sources: `allowed` or `disabled`.
pub const MODEL_SOURCES_ENV: &str = "CAPYCTL_MODEL_SOURCES";
/// The variable naming the sources store's ceiling, e.g. `500GiB`.
pub const MODEL_SOURCES_MAX_ENV: &str = "CAPYCTL_MODEL_SOURCES_MAX";
/// The variable naming the sources store (`model_sources.path`).
pub const MODEL_SOURCES_PATH_ENV: &str = "CAPYCTL_MODEL_SOURCES_PATH";
/// The variable approving plain `http://` sources
/// (`model_sources.plain_http`): `allowed` or `disabled`.
pub const MODEL_SOURCES_PLAIN_HTTP_ENV: &str = "CAPYCTL_MODEL_SOURCES_PLAIN_HTTP";
/// The variable naming the Hugging Face endpoint
/// (`model_sources.huggingface_endpoint`).
pub const HF_ENDPOINT_ENV: &str = "CAPYCTL_HF_ENDPOINT";
/// The Hugging Face tools' own endpoint variable, read after
/// [`HF_ENDPOINT_ENV`].
pub const HF_TOOLS_ENDPOINT_ENV: &str = "HF_ENDPOINT";
/// The models directory under the home directory when nothing names one.
pub const DEFAULT_MODELS_DIR: &str = "models";
fn refuse(path: &str, detail: impl Into<String>) -> ConfigError {
    ConfigError::new(ConfigErrorCode::UnsupportedCombination, path, detail)
}

/// One layer of run-time settings: the CLI flags, or the environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelOverrides {
    /// `--models-root` / `CAPYCTL_MODELS_ROOT` (absolute).
    pub models_root: Option<PathBuf>,
    /// `--model-sources` / `CAPYCTL_MODEL_SOURCES`: both remote kinds.
    pub sources: Option<SourceSwitch>,
    /// `--model-sources-max` / `CAPYCTL_MODEL_SOURCES_MAX`, as written.
    pub sources_max: Option<String>,
    /// `--model-sources-path` / `CAPYCTL_MODEL_SOURCES_PATH` (absolute).
    pub sources_path: Option<PathBuf>,
    /// `--hf-endpoint` / `CAPYCTL_HF_ENDPOINT` (else `HF_ENDPOINT`), as written.
    pub hf_endpoint: Option<String>,
    /// `--model-sources-plain-http` / `CAPYCTL_MODEL_SOURCES_PLAIN_HTTP`.
    pub plain_http: Option<SourceSwitch>,
}

impl ModelOverrides {
    /// The environment's layer, read through `get` (unset and empty are the
    /// same). A malformed value is refused with the variable's name.
    pub fn from_env(get: &dyn Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let get = |key: &str| get(key).filter(|value| !value.is_empty());
        Ok(Self {
            models_root: get(MODELS_ROOT_ENV)
                .map(|value| absolute(MODELS_ROOT_ENV, &value))
                .transpose()?,
            sources: get(MODEL_SOURCES_ENV)
                .map(|value| switch(MODEL_SOURCES_ENV, &value))
                .transpose()?,
            sources_max: get(MODEL_SOURCES_MAX_ENV)
                .map(|value| max_bytes(MODEL_SOURCES_MAX_ENV, &value).map(|_| value))
                .transpose()?,
            sources_path: get(MODEL_SOURCES_PATH_ENV)
                .map(|value| absolute(MODEL_SOURCES_PATH_ENV, &value))
                .transpose()?,
            hf_endpoint: match get(HF_ENDPOINT_ENV) {
                Some(value) => Some(hf_endpoint(HF_ENDPOINT_ENV, &value)?),
                None => get(HF_TOOLS_ENDPOINT_ENV)
                    .map(|value| hf_endpoint(HF_TOOLS_ENDPOINT_ENV, &value))
                    .transpose()?,
            },
            plain_http: get(MODEL_SOURCES_PLAIN_HTTP_ENV)
                .map(|value| switch(MODEL_SOURCES_PLAIN_HTTP_ENV, &value))
                .transpose()?,
        })
    }

    /// The process environment's layer.
    pub fn from_process_env() -> Result<Self, ConfigError> {
        Self::from_env(&|key| std::env::var(key).ok())
    }
}

/// `allowed` or `disabled` (also `denied`), as a flag or variable spells it.
pub fn switch(name: &str, text: &str) -> Result<SourceSwitch, ConfigError> {
    SourceSwitch::parse(text).ok_or_else(|| {
        refuse(
            name,
            format!("must be `allowed` or `disabled`; got {text:?}"),
        )
    })
}

/// A positive byte size such as `500GiB`.
pub fn max_bytes(name: &str, text: &str) -> Result<i64, ConfigError> {
    crate::effective::parse_bytes(text)
        .ok()
        .filter(|bytes| *bytes > 0)
        .ok_or_else(|| {
            refuse(
                name,
                format!("must be a positive byte size such as 500GiB; got {text:?}"),
            )
        })
}

/// A Hugging Face endpoint a host downloads from: an `https://` URL
/// (ADR 0008: a host fetches over HTTPS only).
pub fn hf_endpoint(name: &str, text: &str) -> Result<String, ConfigError> {
    crate::model_source::https_host(text)
        .map(|_| text.trim_end_matches('/').to_owned())
        .ok_or_else(|| refuse(name, format!("must be an https:// URL; got {text:?}")))
}

/// An absolute directory; a relative one is made absolute against the
/// working directory, as a shell user expects of a flag or variable.
pub fn absolute(name: &str, text: &str) -> Result<PathBuf, ConfigError> {
    std::path::absolute(text).map_err(|_| refuse(name, format!("not a usable path: {text:?}")))
}

/// `~/models` for the home directory `home`.
pub fn default_models_root(home: Option<&Path>) -> Option<PathBuf> {
    home.filter(|home| home.is_absolute())
        .map(|home| home.join(DEFAULT_MODELS_DIR))
}

/// Where the models directory came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootSource {
    Flag,
    Environment,
    Document,
    Default,
}

/// The resolved model settings of one host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSettings {
    /// The models directory (`model_store.path`).
    pub models_root: PathBuf,
    pub root_source: RootSource,
    /// The `model_sources` block the host publishes. Its `path` is stated
    /// only when a layer named one; unstated, downloads live under the
    /// models directory (`<model_store>/sources`).
    pub sources: RawModelSources,
    /// Its normalized form.
    pub policy: ModelSourcePolicy,
}

/// Resolve both settings for a host whose document block is `stated` (the
/// host document, or a standalone document's `host:` block). `default_root` is the models directory when no
/// layer names one (`~/models`, [`default_models_root`]).
pub fn resolve(
    stated: &Value,
    flag: &ModelOverrides,
    env: &ModelOverrides,
    default_root: Option<&Path>,
) -> Result<ModelSettings, ConfigError> {
    let document_root = match stated.get("model_store").map(|store| &store["path"]) {
        None => None,
        Some(path) => Some(
            path.as_str()
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .ok_or_else(|| refuse("model_store.path", "must be an absolute directory"))?,
        ),
    };
    let (models_root, root_source) = if let Some(root) = &flag.models_root {
        (root.clone(), RootSource::Flag)
    } else if let Some(root) = &env.models_root {
        (root.clone(), RootSource::Environment)
    } else if let Some(root) = document_root {
        (root, RootSource::Document)
    } else if let Some(root) = default_root {
        (root.to_path_buf(), RootSource::Default)
    } else {
        return Err(ConfigError::new(
            ConfigErrorCode::MissingRequired,
            "model_store.path",
            format!(
                "no models directory: HOME is unset, so set --models-root, \
                 {MODELS_ROOT_ENV} or model_store.path"
            ),
        ));
    };
    if !models_root.is_absolute() {
        return Err(refuse("model_store.path", "must be an absolute directory"));
    }
    let mut sources: RawModelSources = match stated.get("model_sources") {
        None | Some(Value::Null) => RawModelSources::default(),
        Some(block) => serde_json::from_value(block.clone()).map_err(|error| {
            refuse(
                "model_sources",
                format!("not a model_sources block: {error}"),
            )
        })?,
    };
    if let Some(switch) = flag.sources.or(env.sources) {
        sources.huggingface = Some(switch);
        sources.http = Some(switch);
    }
    if let Some(max) = flag.sources_max.as_ref().or(env.sources_max.as_ref()) {
        sources.max_bytes = Some(max.clone());
    }
    if let Some(path) = flag.sources_path.as_ref().or(env.sources_path.as_ref()) {
        sources.path = Some(path.to_string_lossy().into_owned());
    }
    if let Some(endpoint) = flag.hf_endpoint.as_ref().or(env.hf_endpoint.as_ref()) {
        sources.huggingface_endpoint = Some(endpoint.clone());
    }
    if let Some(plain_http) = flag.plain_http.or(env.plain_http) {
        sources.plain_http = Some(plain_http);
    }
    // Owner ruling 2026-09-25: with no `model_sources.path`, downloads stay
    // in `<model_store>/sources` (ModelSourcePolicy::root), the layout every
    // earlier release used, so an existing verified copy is reused rather
    // than downloaded again.
    let policy = ModelSourcePolicy::from_raw(Some(sources.clone()))?;
    Ok(ModelSettings {
        models_root,
        root_source,
        sources,
        policy,
    })
}

impl ModelSettings {
    /// Write the resolved settings into a host `document`.
    pub fn write_into(&self, document: &mut Value) {
        document["model_store"] = json!({"path": self.models_root});
        document["model_sources"] =
            serde_json::to_value(&self.sources).expect("a model_sources block serializes");
    }
}

/// [`resolve`] a host document's own settings and write them back into it.
pub fn apply(
    document: &mut Value,
    flag: &ModelOverrides,
    env: &ModelOverrides,
    default_root: Option<&Path>,
) -> Result<ModelSettings, ConfigError> {
    let settings = resolve(document, flag, env, default_root)?;
    settings.write_into(document);
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_source::DEFAULT_SOURCES_MAX_BYTES;

    fn env_layer(pairs: &[(&str, &str)]) -> Result<ModelOverrides, ConfigError> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        ModelOverrides::from_env(&move |key| {
            pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
        })
    }

    const HOME: &str = "/home/user";

    fn home() -> Option<PathBuf> {
        default_models_root(Some(Path::new(HOME)))
    }

    // T14 (owner decision 2026-09-25): with nothing stated, models live in
    // ~/models, downloads in ~/models/sources (owner ruling: the layout
    // earlier releases used, so existing copies are reused), and both remote
    // kinds are allowed with the 500 GiB ceiling.
    #[test]
    fn nothing_stated_resolves_the_defaults() {
        let settings = resolve(
            &json!({}),
            &ModelOverrides::default(),
            &ModelOverrides::default(),
            home().as_deref(),
        )
        .unwrap();
        assert_eq!(settings.models_root, Path::new("/home/user/models"));
        assert_eq!(settings.root_source, RootSource::Default);
        assert_eq!(settings.policy.huggingface, SourceSwitch::Allowed);
        assert_eq!(settings.policy.http, SourceSwitch::Allowed);
        assert_eq!(settings.policy.max_bytes, Some(DEFAULT_SOURCES_MAX_BYTES));
        assert_eq!(
            settings.policy.root(&settings.models_root),
            Path::new("/home/user/models")
        );
        assert_eq!(settings.policy.path, None);
        let mut document = json!({"name": "h"});
        settings.write_into(&mut document);
        assert_eq!(document["model_store"]["path"], "/home/user/models");
        assert_eq!(document["model_sources"], json!({}));
    }

    // T14 T03 (owner rule 2026-09-25): CLI flag > environment > YAML > default
    // for the models directory.
    #[test]
    fn the_models_root_follows_flag_env_document_default() {
        let document = json!({"model_store": {"path": "/yaml/models"}});
        let flag = ModelOverrides {
            models_root: Some("/flag/models".into()),
            ..Default::default()
        };
        let env = env_layer(&[(MODELS_ROOT_ENV, "/env/models")]).unwrap();
        let none = ModelOverrides::default();
        let root = |flag: &ModelOverrides, env: &ModelOverrides, document: &Value| {
            let settings = resolve(document, flag, env, home().as_deref()).unwrap();
            (settings.models_root, settings.root_source)
        };
        assert_eq!(
            root(&flag, &env, &document),
            ("/flag/models".into(), RootSource::Flag)
        );
        assert_eq!(
            root(&none, &env, &document),
            ("/env/models".into(), RootSource::Environment)
        );
        assert_eq!(
            root(&none, &none, &document),
            ("/yaml/models".into(), RootSource::Document)
        );
        assert_eq!(
            root(&none, &none, &json!({})),
            ("/home/user/models".into(), RootSource::Default)
        );
        // No home and nothing stated: a clear refusal, never a guess.
        let error = resolve(&json!({}), &none, &none, None).unwrap_err();
        assert_eq!(error.code, ConfigErrorCode::MissingRequired);
        assert!(error.detail.contains(MODELS_ROOT_ENV), "{error}");
        // A relative document path is refused; a relative variable is made
        // absolute against the working directory.
        assert!(resolve(&json!({"model_store": {"path": "rel"}}), &none, &none, None).is_err());
        assert!(env_layer(&[(MODELS_ROOT_ENV, "rel/models")])
            .unwrap()
            .models_root
            .unwrap()
            .is_absolute());
    }

    // T14 T03 (owner rule 2026-09-25): CLI flag > environment > YAML > default
    // for the source switch and the ceiling; an explicit `denied` in the
    // document wins over the default.
    #[test]
    fn the_source_policy_follows_flag_env_document_default() {
        let disabled = json!({"model_sources": {"huggingface": "denied", "http": "disabled"}});
        let none = ModelOverrides::default();
        let policy = |flag: &ModelOverrides, env: &ModelOverrides, document: &Value| {
            resolve(document, flag, env, home().as_deref())
                .unwrap()
                .policy
        };
        let kept = policy(&none, &none, &disabled);
        assert_eq!(kept.huggingface, SourceSwitch::Denied);
        assert_eq!(kept.http, SourceSwitch::Denied);
        let env_on = env_layer(&[(MODEL_SOURCES_ENV, "allowed")]).unwrap();
        assert_eq!(
            policy(&none, &env_on, &disabled).huggingface,
            SourceSwitch::Allowed
        );
        let flag_off = ModelOverrides {
            sources: Some(SourceSwitch::Denied),
            ..Default::default()
        };
        let flagged = policy(&flag_off, &env_on, &json!({}));
        assert_eq!(flagged.huggingface, SourceSwitch::Denied);
        assert_eq!(flagged.http, SourceSwitch::Denied);
        // The ceiling.
        let yaml_max = json!({"model_sources": {"max_bytes": "100GiB"}});
        let env_max = env_layer(&[(MODEL_SOURCES_MAX_ENV, "200GiB")]).unwrap();
        let flag_max = ModelOverrides {
            sources_max: Some("300GiB".into()),
            ..Default::default()
        };
        assert_eq!(
            policy(&flag_max, &env_max, &yaml_max).max_bytes,
            Some(300 << 30)
        );
        assert_eq!(
            policy(&none, &env_max, &yaml_max).max_bytes,
            Some(200 << 30)
        );
        assert_eq!(policy(&none, &none, &yaml_max).max_bytes, Some(100 << 30));
        assert_eq!(
            policy(&none, &none, &json!({})).max_bytes,
            Some(DEFAULT_SOURCES_MAX_BYTES)
        );
        // A stated sources store is kept.
        let stated = json!({"model_sources": {"path": "/data/downloads"}});
        let settings = resolve(&stated, &none, &none, home().as_deref()).unwrap();
        assert_eq!(
            settings.policy.root(&settings.models_root),
            Path::new("/data/downloads")
        );
    }

    // T14 T03 (owner rule 2026-09-25: every setting three ways): the sources
    // store and the Hugging Face endpoint follow flag > environment > YAML >
    // default; `CAPYCTL_HF_ENDPOINT` wins over the tools' `HF_ENDPOINT`.
    #[test]
    fn the_sources_path_and_endpoint_follow_flag_env_document_default() {
        let yaml = json!({"model_sources": {
            "path": "/yaml/downloads", "huggingface_endpoint": "https://yaml.example"}});
        let env = env_layer(&[
            (MODEL_SOURCES_PATH_ENV, "/env/downloads"),
            (HF_ENDPOINT_ENV, "https://env.example/"),
            (HF_TOOLS_ENDPOINT_ENV, "https://tools.example"),
        ])
        .unwrap();
        let flag = ModelOverrides {
            sources_path: Some("/flag/downloads".into()),
            hf_endpoint: Some("https://flag.example".into()),
            ..Default::default()
        };
        let none = ModelOverrides::default();
        let seen = |flag: &ModelOverrides, env: &ModelOverrides, document: &Value| {
            let settings = resolve(document, flag, env, home().as_deref()).unwrap();
            (
                settings.policy.root(&settings.models_root).to_path_buf(),
                settings.policy.huggingface_endpoint().to_owned(),
            )
        };
        assert_eq!(
            seen(&flag, &env, &yaml),
            ("/flag/downloads".into(), "https://flag.example".into())
        );
        assert_eq!(
            seen(&none, &env, &yaml),
            ("/env/downloads".into(), "https://env.example".into())
        );
        assert_eq!(
            seen(&none, &none, &yaml),
            ("/yaml/downloads".into(), "https://yaml.example".into())
        );
        assert_eq!(
            seen(&none, &none, &json!({})),
            (
                "/home/user/models".into(),
                crate::model_source::DEFAULT_HUGGINGFACE_ENDPOINT.into()
            )
        );
        let tools = env_layer(&[(HF_TOOLS_ENDPOINT_ENV, "https://tools.example")]).unwrap();
        assert_eq!(tools.hf_endpoint.as_deref(), Some("https://tools.example"));
    }

    // T14 T03 T37 (ADR 0008 amendment 2026-10-08, owner rule: every setting
    // three ways): plain `http://` sources follow flag > environment > YAML >
    // default, and the default is `denied`. The resolved block a host
    // publishes states it only when a layer did.
    #[test]
    fn plain_http_follows_flag_env_document_default() {
        let yaml_on = json!({"model_sources": {"plain_http": "allowed"}});
        let env_off = env_layer(&[(MODEL_SOURCES_PLAIN_HTTP_ENV, "disabled")]).unwrap();
        let env_on = env_layer(&[(MODEL_SOURCES_PLAIN_HTTP_ENV, "allowed")]).unwrap();
        let flag_on = ModelOverrides {
            plain_http: Some(SourceSwitch::Allowed),
            ..Default::default()
        };
        let none = ModelOverrides::default();
        let resolved = |flag: &ModelOverrides, env: &ModelOverrides, document: &Value| {
            resolve(document, flag, env, home().as_deref()).unwrap()
        };
        let plain = |flag: &ModelOverrides, env: &ModelOverrides, document: &Value| {
            resolved(flag, env, document).policy.plain_http
        };
        assert_eq!(plain(&flag_on, &env_off, &json!({})), SourceSwitch::Allowed);
        assert_eq!(plain(&none, &env_off, &yaml_on), SourceSwitch::Denied);
        assert_eq!(plain(&none, &env_on, &json!({})), SourceSwitch::Allowed);
        assert_eq!(plain(&none, &none, &yaml_on), SourceSwitch::Allowed);
        assert_eq!(plain(&none, &none, &json!({})), SourceSwitch::Denied);
        let mut document = json!({});
        resolved(&none, &none, &json!({})).write_into(&mut document);
        assert_eq!(document["model_sources"], json!({}));
        resolved(&none, &env_on, &json!({})).write_into(&mut document);
        assert_eq!(document["model_sources"], json!({"plain_http": "allowed"}));
        let error = env_layer(&[(MODEL_SOURCES_PLAIN_HTTP_ENV, "yes")]).unwrap_err();
        assert_eq!(error.path, MODEL_SOURCES_PLAIN_HTTP_ENV);
    }

    // T03: malformed variables are refused with their names.
    #[test]
    fn malformed_variables_are_refused() {
        for (key, value) in [
            (MODEL_SOURCES_ENV, "yes"),
            (MODEL_SOURCES_MAX_ENV, "lots"),
            (MODEL_SOURCES_MAX_ENV, "0GiB"),
            (HF_ENDPOINT_ENV, "http://mirror.example"),
            (HF_TOOLS_ENDPOINT_ENV, "not a url"),
        ] {
            let error = env_layer(&[(key, value)]).unwrap_err();
            assert_eq!(error.path, key);
        }
        assert_eq!(
            env_layer(&[(MODEL_SOURCES_ENV, "disabled")])
                .unwrap()
                .sources,
            Some(SourceSwitch::Denied)
        );
        assert_eq!(env_layer(&[(MODEL_SOURCES_ENV, "")]).unwrap().sources, None);
    }
}
