//! Owner decisions 2026-09-25: where a host keeps models, and whether it may
//! download them. Implemented once and used by every role (standalone behaves
//! as a server plus one host), so a host and standalone resolve both settings
//! by the same rule.
//!
//! - The models directory (`model_store.path`), which anchors a relative local
//!   model path (SPEC §7), defaults to `~/models`. It is stated three ways:
//!   `--models-root`, `MLLM_MODELS_ROOT`, or `model_store.path` in the YAML
//!   document.
//! - Hugging Face and HTTP model sources (ADR 0008) are allowed by default on
//!   every host, with a 500 GiB ceiling for the sources store
//!   ([`crate::model_source::DEFAULT_SOURCES_MAX_BYTES`]). They are switched
//!   with `--model-sources allowed|disabled`, `MLLM_MODEL_SOURCES`, or the
//!   document's `model_sources.huggingface` / `model_sources.http`; the
//!   ceiling with `--model-sources-max`, `MLLM_MODEL_SOURCES_MAX`, or
//!   `model_sources.max_bytes`. An explicit `denied` (or `disabled`) in the
//!   document wins over the default.
//! - Downloads live in their own store, `<state_dir>/models/sources`, unless
//!   the document names another with `model_sources.path`.
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
pub const MODELS_ROOT_ENV: &str = "MLLM_MODELS_ROOT";
/// The variable switching Hugging Face and HTTP sources: `allowed` or `disabled`.
pub const MODEL_SOURCES_ENV: &str = "MLLM_MODEL_SOURCES";
/// The variable naming the sources store's ceiling, e.g. `500GiB`.
pub const MODEL_SOURCES_MAX_ENV: &str = "MLLM_MODEL_SOURCES_MAX";
/// The models directory under the home directory when nothing names one.
pub const DEFAULT_MODELS_DIR: &str = "models";
/// The directory under a role's state directory that holds the sources store
/// (`<state_dir>/models/sources`).
pub const SOURCES_STORE_DIR: &str = "models";

fn refuse(path: &str, detail: impl Into<String>) -> ConfigError {
    ConfigError::new(ConfigErrorCode::UnsupportedCombination, path, detail)
}

/// One layer of run-time settings: the CLI flags, or the environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelOverrides {
    /// `--models-root` / `MLLM_MODELS_ROOT` (absolute).
    pub models_root: Option<PathBuf>,
    /// `--model-sources` / `MLLM_MODEL_SOURCES`: both remote kinds.
    pub sources: Option<SourceSwitch>,
    /// `--model-sources-max` / `MLLM_MODEL_SOURCES_MAX`, as written.
    pub sources_max: Option<String>,
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
    /// The `model_sources` block the host publishes, with the sources store
    /// always named.
    pub sources: RawModelSources,
    /// Its normalized form.
    pub policy: ModelSourcePolicy,
}

/// Resolve both settings for a host whose document block is `stated` (the
/// host document, or a standalone document's `host:` block) and whose state
/// directory is `state_dir`. `default_root` is the models directory when no
/// layer names one (`~/models`, [`default_models_root`]).
pub fn resolve(
    stated: &Value,
    state_dir: &Path,
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
    if sources.path.is_none() {
        sources.path = Some(
            state_dir
                .join(SOURCES_STORE_DIR)
                .to_string_lossy()
                .into_owned(),
        );
    }
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
    state_dir: &Path,
    flag: &ModelOverrides,
    env: &ModelOverrides,
    default_root: Option<&Path>,
) -> Result<ModelSettings, ConfigError> {
    let settings = resolve(document, state_dir, flag, env, default_root)?;
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
    // ~/models, downloads in <state_dir>/models/sources, and both remote
    // kinds are allowed with the 500 GiB ceiling.
    #[test]
    fn nothing_stated_resolves_the_defaults() {
        let settings = resolve(
            &json!({}),
            Path::new("/state"),
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
            Path::new("/state/models")
        );
        let mut document = json!({"name": "h"});
        settings.write_into(&mut document);
        assert_eq!(document["model_store"]["path"], "/home/user/models");
        assert_eq!(document["model_sources"], json!({"path": "/state/models"}));
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
            let settings =
                resolve(document, Path::new("/s"), flag, env, home().as_deref()).unwrap();
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
        let error = resolve(&json!({}), Path::new("/s"), &none, &none, None).unwrap_err();
        assert_eq!(error.code, ConfigErrorCode::MissingRequired);
        assert!(error.detail.contains(MODELS_ROOT_ENV), "{error}");
        // A relative document path is refused; a relative variable is made
        // absolute against the working directory.
        assert!(resolve(
            &json!({"model_store": {"path": "rel"}}),
            Path::new("/s"),
            &none,
            &none,
            None
        )
        .is_err());
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
            resolve(document, Path::new("/s"), flag, env, home().as_deref())
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
        let settings = resolve(&stated, Path::new("/s"), &none, &none, home().as_deref()).unwrap();
        assert_eq!(
            settings.policy.root(&settings.models_root),
            Path::new("/data/downloads")
        );
    }

    // T03: malformed variables are refused with their names.
    #[test]
    fn malformed_variables_are_refused() {
        for (key, value) in [
            (MODEL_SOURCES_ENV, "yes"),
            (MODEL_SOURCES_MAX_ENV, "lots"),
            (MODEL_SOURCES_MAX_ENV, "0GiB"),
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
