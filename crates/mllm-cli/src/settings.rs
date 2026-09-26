//! Owner decision 2026-09-25: every YAML setting of a role is settable three
//! ways. The generic overrides (`--set path=value`, `MLLM_SET__PATH=value`)
//! are parsed and applied by `mllm_config::setting_overrides`; this module
//! gathers the named flags and variables of one run and refuses a run whose
//! named form and generic override of one setting disagree.

use std::path::PathBuf;

use mllm_config::setting_overrides::{NamedLayer, NamedValue, SettingOverrides, Source};
use mllm_config::{ConfigError, ConfigKind};

use crate::grammar::Invocation;

/// The variable naming the state root.
pub const STATE_DIR_ENV: &str = "MLLM_STATE_DIR";

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|value| !value.is_empty())
}

/// The named flags of `invocation`, in their YAML form.
pub fn flag_layer(invocation: &Invocation) -> NamedLayer {
    NamedLayer {
        models: invocation.model_overrides.clone(),
        engines: invocation.engine_overrides.clone(),
        inference_bind: invocation.listen.map(|address| address.to_string()),
        inference_auth: invocation.no_inference_auth.then(|| "none".to_owned()),
        management_bind: invocation
            .management_listen
            .map(|address| address.to_string()),
        state_dir: invocation.state_dir.clone(),
    }
}

/// The named variables of the process environment, in their YAML form. A
/// malformed engine or model variable is refused with its name, as the role
/// itself refuses it.
pub fn env_layer() -> Result<NamedLayer, ConfigError> {
    Ok(NamedLayer {
        models: mllm_config::model_settings::ModelOverrides::from_process_env()?,
        engines: mllm_config::engine_settings::EngineOverrides::from_process_env()?,
        inference_bind: env(crate::roles::INFERENCE_ADDR_ENV)
            .or_else(|| env(crate::roles::DEPRECATED_INFERENCE_ADDR_ENV)),
        inference_auth: env(crate::exposure::INFERENCE_AUTH_ENV),
        management_bind: env(crate::roles::MANAGEMENT_ADDR_ENV)
            .or_else(|| env(crate::roles::DEPRECATED_MANAGEMENT_ADDR_ENV)),
        state_dir: env(STATE_DIR_ENV).map(PathBuf::from),
    })
}

/// The effective named value of each setting: a flag over its variable.
pub fn named_values(kind: ConfigKind, flags: &NamedLayer, env: &NamedLayer) -> Vec<NamedValue> {
    let mut named = flags.values(kind, Source::Flag);
    for value in env.values(kind, Source::Env) {
        if !named.iter().any(|flag| flag.path == value.path) {
            named.push(value);
        }
    }
    named
}

/// The generic overrides of this run for a `kind` document: `--set` (`sets`)
/// over `MLLM_SET__…`. A setting stated by its named flag or variable and by
/// a generic override must agree, or the run is refused.
pub fn role_overrides(
    kind: ConfigKind,
    sets: &[String],
    flags: &NamedLayer,
) -> Result<SettingOverrides, ConfigError> {
    let overrides = SettingOverrides::from_process(kind, sets)?;
    if !overrides.is_empty() {
        overrides.check_named(&named_values(kind, flags, &env_layer()?))?;
    }
    Ok(overrides)
}

/// A configuration error as the roles print it: `<path>: <detail>`.
pub fn describe(error: &ConfigError) -> String {
    if error.path.is_empty() {
        error.detail.clone()
    } else {
        format!("{}: {}", error.path, error.detail)
    }
}

/// Owner decision 2026-09-25: `mllm config show`. The effective
/// configuration of a role, each value with its source (`default`, `yaml`,
/// `env`, `flag` or `set`), by the precedence the role itself applies:
/// `--set` > `MLLM_SET__…` > named flag > named variable > YAML > default.
///
/// The document is `--config` (else `MLLM_CONFIG`), whose `kind` is the role;
/// else the role's implicit document under the state root (`--role`, default
/// standalone), which may not exist yet (its settings are then the
/// defaults). The overridden document is validated as the start validates it,
/// and a named form that disagrees with a generic override is refused, so a
/// `config show` that succeeds is what the start would run with. Nothing is
/// written.
pub fn config_show(
    invocation: &Invocation,
    role: Option<crate::grammar::Role>,
    sets: &[String],
    state_root: &std::path::Path,
) -> Result<serde_json::Value, crate::output::StructuredError> {
    use crate::grammar::Role;
    use serde_json::{json, Value};
    let invalid = |message: String| crate::output::StructuredError {
        code: "invalid_config",
        message,
    };
    let named = crate::engine::named_role_document(invocation.config.as_deref(), &env);
    let role_kind = |role: Role| match role {
        Role::Server => ConfigKind::Server,
        Role::Host => ConfigKind::Host,
        Role::Standalone => ConfigKind::Standalone,
    };
    let (kind, path) = match &named {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| invalid(format!("{}: {e}", path.display())))?;
            let document = mllm_config::parse_document(&text)
                .map_err(|e| invalid(format!("{}: {}", path.display(), describe(&e))))?;
            let kind: ConfigKind = document["kind"]
                .as_str()
                .unwrap_or_default()
                .parse()
                .map_err(|_| {
                    invalid(format!(
                        "{}: not a role document (kind: server, host or standalone)",
                        path.display()
                    ))
                })?;
            if let Some(role) = role {
                if role_kind(role) != kind {
                    return Err(invalid(format!(
                        "{} is a {} document, not a {} one",
                        path.display(),
                        kind.as_str(),
                        role_kind(role).as_str()
                    )));
                }
            }
            (kind, path.clone())
        }
        None => {
            let kind = role_kind(role.unwrap_or(Role::Standalone));
            (
                kind,
                state_root
                    .join("config")
                    .join(format!("{}.yaml", kind.as_str())),
            )
        }
    };
    if !mllm_config::setting_overrides::is_role_kind(kind) {
        return Err(invalid(format!(
            "{}: config show reads role documents (server, host, standalone), not a {} document",
            path.display(),
            kind.as_str()
        )));
    }
    let located = |e: ConfigError| invalid(format!("{}: {}", path.display(), describe(&e)));
    let stated = match std::fs::read_to_string(&path) {
        Ok(text) => Some(mllm_config::parse_document(&text).map_err(located)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && named.is_none() => None,
        Err(e) => return Err(invalid(format!("{}: {e}", path.display()))),
    };
    let flags = flag_layer(invocation);
    let overrides = role_overrides(kind, sets, &flags).map_err(located)?;
    // The overridden document is validated as the role validates it.
    let base = stated.clone().unwrap_or_else(
        || json!({"schema_version": 1, "kind": kind.as_str(), "name": kind.as_str()}),
    );
    overrides
        .apply_and_validate(base)
        .map_err(|e| located(overrides.annotate(e)))?;
    let env_values = env_layer().map_err(located)?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut effective: std::collections::BTreeMap<String, (Value, Source)> = Default::default();
    let mut put = |path: String, value: Value, source: Source| {
        effective.insert(path, (value, source));
    };
    for (path, value) in mllm_config::setting_overrides::defaults(kind, home.as_deref(), state_root)
    {
        put(path, value, Source::Default);
    }
    if let Some(document) = &stated {
        for (path, value) in mllm_config::setting_overrides::leaves(document) {
            put(path, value, Source::Yaml);
        }
    }
    for value in env_values.values(kind, Source::Env) {
        put(value.path, value.value, value.source);
    }
    for value in flags.values(kind, Source::Flag) {
        put(value.path, value.value, value.source);
    }
    for item in overrides
        .effective()
        .into_iter()
        .filter(|item| item.source == Source::Env)
        .chain(
            overrides
                .effective()
                .into_iter()
                .filter(|item| item.source == Source::Set),
        )
    {
        put(item.path.clone(), item.value.clone(), item.source);
    }
    let settings: Vec<Value> = effective
        .into_iter()
        .map(|(path, (value, source))| json!({"path": path, "value": value, "source": source.as_str()}))
        .collect();
    Ok(json!({
        "role": kind.as_str(),
        "document": stated.as_ref().map(|_| path.display().to_string()),
        "settings": settings,
    }))
}

/// `config show` as a table: SETTING, VALUE, SOURCE, after a line naming the
/// role and its document.
pub fn render_table(value: &serde_json::Value) -> String {
    let rows: Vec<Vec<String>> = value["settings"]
        .as_array()
        .map(|settings| {
            settings
                .iter()
                .map(|setting| {
                    vec![
                        setting["path"].as_str().unwrap_or_default().to_owned(),
                        mllm_config::setting_overrides::display(&setting["value"]),
                        setting["source"].as_str().unwrap_or_default().to_owned(),
                    ]
                })
                .collect()
        })
        .unwrap_or_default();
    format!(
        "{} ({})\n{}",
        value["role"].as_str().unwrap_or_default(),
        value["document"]
            .as_str()
            .map(|path| format!("document {path}"))
            .unwrap_or_else(|| "no document; defaults".to_owned()),
        crate::table::table(&["SETTING", "VALUE", "SOURCE"], &rows)
    )
}
