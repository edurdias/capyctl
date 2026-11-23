//! Owner decision 2026-09-25, ADR 0040 §10: a role's effective
//! configuration, each value with its source (`default`, `yaml`, `env`,
//! `flag` or `set`), by the precedence the role itself applies: `--set` >
//! `CAPYCTL_SET__…` > named flag > named variable > YAML > default. This is
//! what `capyctl config show` prints and a host's configuration route
//! returns.
use capyctl_config::setting_overrides::{NamedLayer, SettingOverrides, Source};
use capyctl_config::ConfigKind;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::Path;

/// The layers over a role's document: its named variables, its named flags
/// and its generic overrides (`--set` over `CAPYCTL_SET__…`).
pub struct ConfigLayers<'a> {
    pub env: &'a NamedLayer,
    pub flags: &'a NamedLayer,
    pub overrides: &'a SettingOverrides,
}

/// The effective configuration of the `kind` document at `document`
/// (`stated` when it exists), already validated with its overrides by the
/// caller. Nothing is written.
pub fn effective_config(
    kind: ConfigKind,
    document: &Path,
    stated: Option<&Value>,
    layers: ConfigLayers<'_>,
    home: Option<&Path>,
    state_root: &Path,
) -> Value {
    let mut effective: BTreeMap<String, (Value, Source)> = BTreeMap::new();
    let mut put = |path: String, value: Value, source: Source| {
        effective.insert(path, (value, source));
    };
    for (path, value) in capyctl_config::setting_overrides::defaults(kind, home, state_root) {
        put(path, value, Source::Default);
    }
    if let Some(document) = stated {
        for (path, value) in capyctl_config::setting_overrides::leaves(document) {
            put(path, value, Source::Yaml);
        }
    }
    for value in layers.env.values(kind, Source::Env) {
        put(value.path, value.value, value.source);
    }
    for value in layers.flags.values(kind, Source::Flag) {
        put(value.path, value.value, value.source);
    }
    for item in layers
        .overrides
        .effective()
        .into_iter()
        .filter(|item| item.source == Source::Env)
        .chain(
            layers
                .overrides
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
    json!({
        "role": kind.as_str(),
        "document": stated.map(|_| document.display().to_string()),
        "settings": settings,
    })
}
