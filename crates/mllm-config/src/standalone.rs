//! SPEC §15.3: a standalone document is refused when it states a setting the
//! standalone role does not honour.
//!
//! The standalone role serves both listeners over plain HTTP on loopback, keeps
//! its state under the state root it was started with, and derives its embedded
//! host's policy from the installation and capacity it observes
//! (`mllm-cli/src/standalone_config.rs`). A document that names another state
//! directory, another listener address, TLS, a model store, a runtime profile or
//! a numeric resource limit would otherwise be accepted and silently ignored, so
//! an operator could believe a setting is in force that is not. Each such value
//! is refused here with the path that names it.
//!
//! What is accepted is exactly what the role does: the per-role state
//! directories under the state root, the two loopback listeners at their
//! defaults with their fixed authentication, an `embedded` connection, `auto`
//! resource values and no runtime profiles. A listener moves for one run only
//! through `MLLM_STANDALONE_INFERENCE_ADDR` / `MLLM_STANDALONE_MANAGEMENT_ADDR`
//! (SPEC §15.2: a run-time override of an ordinary setting). The `name` fields
//! are labels and are not checked.
//!
//! One exception keeps existing installations starting (SPEC §15.2, R13). An
//! older generator wrote a `server.tls` block (`mode: managed`, `identity_dir:
//! <state root>/identity`) that the role never honoured. A block equal to that
//! generated value is accepted and returned as an [`IgnoredSetting`] for the
//! role to report; the file is never rewritten, because generated configuration
//! is created once and never replaced. Any other `server.tls` value is an
//! operator's statement and is refused.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::error::{ConfigError, ConfigErrorCode};

/// The standalone listeners, their loopback default addresses and the only
/// authentication each supports (SPEC §16.5).
pub const LISTENERS: &[(&str, &str, &str)] = &[
    ("management", "127.0.0.1:7443", "admin_token"),
    ("inference", "127.0.0.1:8443", "api_key"),
];

/// A value of the document the role accepts but does not honour. It exists only
/// for the exact shape an older generator wrote, and is reported so that nobody
/// believes it is in force (SPEC §15.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IgnoredSetting {
    /// The document path of the ignored value.
    pub path: String,
    /// Why it is ignored and what to do about it.
    pub reason: String,
}

impl std::fmt::Display for IgnoredSetting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "standalone configuration: `{}` is ignored: {}",
            self.path, self.reason
        )
    }
}

const TLS_REFUSAL: &str =
    "standalone listeners serve plain HTTP on loopback; TLS is not supported, remove this block";

/// SPEC §15.2 (R13): whether `tls` is exactly the block an older mllm generator
/// wrote into the standalone default: `mode: managed` and `identity_dir` naming
/// `identity` under the state root, and nothing else.
fn is_legacy_generated_tls(tls: &Value, config_dir: &Path, root: &Path) -> bool {
    let Some(map) = tls.as_object() else {
        return false;
    };
    map.len() == 2
        && map.get("mode").and_then(Value::as_str) == Some("managed")
        && map
            .get("identity_dir")
            .and_then(Value::as_str)
            .is_some_and(|dir| resolve(config_dir, dir) == root.join("identity"))
}

fn refuse(path: &str, detail: impl Into<String>) -> ConfigError {
    ConfigError::new(ConfigErrorCode::UnsupportedCombination, path, detail)
}

/// Lexically normalise `value` (relative paths resolve against `base`, SPEC
/// §16.5), without touching the filesystem.
fn resolve(base: &Path, value: &str) -> PathBuf {
    let joined = if Path::new(value).is_absolute() {
        PathBuf::from(value)
    } else {
        base.join(value)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

fn check_state_dir(
    block: &Value,
    path: &str,
    config_dir: &Path,
    expected: &Path,
) -> Result<(), ConfigError> {
    let Some(value) = block.get("state_dir") else {
        return Ok(());
    };
    let text = value
        .as_str()
        .ok_or_else(|| refuse(path, "must be a path"))?;
    if resolve(config_dir, text) != resolve(Path::new("/"), &expected.to_string_lossy()) {
        return Err(refuse(
            path,
            format!(
                "standalone keeps this state at `{}` under the state root it was started with; \
                 another directory is not supported",
                expected.display()
            ),
        ));
    }
    Ok(())
}

/// Every leaf of `value` must be the string `auto`.
fn only_auto(value: &Value, path: &str) -> Result<(), ConfigError> {
    match value {
        Value::Object(map) => map
            .iter()
            .try_for_each(|(key, child)| only_auto(child, &format!("{path}.{key}"))),
        Value::String(text) if text == "auto" => Ok(()),
        _ => Err(refuse(
            path,
            "standalone derives its limits from the capacity it observes; only `auto` is supported",
        )),
    }
}

/// SPEC §15.3: refuse every value of a standalone `document` the role would
/// not honour. `config_dir` is the directory of the configuration file (relative
/// paths resolve against it) and `state_root` the state root the role runs with.
///
/// On success it returns the values accepted but ignored (only the legacy
/// generated `server.tls` block); the caller must report each one.
pub fn check_honoured(
    document: &Value,
    config_dir: &Path,
    state_root: &Path,
) -> Result<Vec<IgnoredSetting>, ConfigError> {
    let mut ignored = Vec::new();
    let root = resolve(config_dir, &state_root.to_string_lossy());
    let server = &document["server"];
    let host = &document["host"];
    check_state_dir(server, "server.state_dir", config_dir, &root.join("server"))?;
    check_state_dir(host, "host.state_dir", config_dir, &root.join("host"))?;
    if let Some(tls) = server.get("tls") {
        if !is_legacy_generated_tls(tls, config_dir, &root) {
            return Err(refuse("server.tls", TLS_REFUSAL));
        }
        ignored.push(IgnoredSetting {
            path: "server.tls".into(),
            reason: "an older mllm generated this block; the standalone listeners serve \
                     plain HTTP on loopback. It has no effect and may be removed"
                .into(),
        });
    }
    if let Some(listeners) = server.get("listeners").and_then(Value::as_object) {
        for (name, listener) in listeners {
            let path = format!("server.listeners.{name}");
            let Some((_, address, authentication)) =
                LISTENERS.iter().find(|(known, _, _)| known == name)
            else {
                return Err(refuse(
                    &path,
                    "standalone has only the `management` and `inference` listeners",
                ));
            };
            if let Some(bind) = listener.get("bind") {
                if bind.as_str() != Some(address) {
                    return Err(refuse(
                        &format!("{path}.bind"),
                        format!(
                            "standalone binds {address}; move it for one run with \
                             MLLM_STANDALONE_{}_ADDR, not in the document",
                            name.to_ascii_uppercase()
                        ),
                    ));
                }
            }
            if let Some(mode) = listener.get("authentication") {
                if mode.as_str() != Some(authentication) {
                    return Err(refuse(
                        &format!("{path}.authentication"),
                        format!("the standalone {name} listener supports only `{authentication}`"),
                    ));
                }
            }
        }
    }
    if let Some(connection) = host.get("connection") {
        if connection.as_str() != Some("embedded") {
            return Err(refuse(
                "host.connection",
                "a standalone host is always `embedded`",
            ));
        }
    }
    if host.get("model_store").is_some() {
        return Err(refuse(
            "host.model_store",
            "standalone keeps models under its engine installation's models root; \
             another store is not supported",
        ));
    }
    if let Some(policy) = host.get("resource_policy") {
        only_auto(policy, "host.resource_policy")?;
    }
    if let Some(profiles) = host.get("runtime_profiles") {
        if profiles.as_object().is_none_or(|map| !map.is_empty()) {
            return Err(refuse(
                "host.runtime_profiles",
                "standalone runs the engine installation it discovers; declared profiles are not supported",
            ));
        }
    }
    Ok(ignored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn generated(root: &str) -> Value {
        json!({
            "schema_version": 1, "kind": "standalone", "name": "local",
            "server": {
                "name": "local",
                "state_dir": format!("{root}/server"),
                "listeners": {
                    "management": {"bind": "127.0.0.1:7443", "authentication": "admin_token"},
                    "inference": {"bind": "127.0.0.1:8443", "authentication": "api_key"}
                }
            },
            "host": {
                "name": "local", "state_dir": format!("{root}/host"), "connection": "embedded",
                "resource_policy": {"allowed_devices": "auto", "memory": {"accounting": "auto",
                    "system": {"managed_limit": "auto", "free_reserve": "auto"}}},
                "runtime_profiles": {}
            }
        })
    }

    // T03 (SPEC §15.3): the generated shape is exactly what the role honours.
    #[test]
    fn the_generated_shape_is_accepted() {
        let root = Path::new("/s");
        check_honoured(&generated("/s"), Path::new("/s/config"), root).unwrap();
        // Relative paths resolve against the configuration file.
        let mut doc = generated("/s");
        doc["server"]["state_dir"] = json!("../server");
        check_honoured(&doc, Path::new("/s/config"), root).unwrap();
        // Omitted fields are not settings.
        let bare = json!({"schema_version": 1, "kind": "standalone", "name": "x"});
        check_honoured(&bare, Path::new("/s/config"), root).unwrap();
    }

    // T03 (SPEC §15.2, R13): a document written by an older mllm generator carries
    // the `server.tls` block that generator used to emit. The installation must
    // still start: the block is recognised by its exact generated value, accepted
    // and reported as ignored, and the file is never rewritten.
    #[test]
    fn the_legacy_generated_tls_block_is_accepted_and_reported_as_ignored() {
        let root = Path::new("/s");
        let mut doc = generated("/s");
        doc["server"]["tls"] = json!({"mode": "managed", "identity_dir": "/s/identity"});
        let ignored = check_honoured(&doc, Path::new("/s/config"), root).unwrap();
        assert_eq!(ignored.len(), 1, "{ignored:?}");
        assert_eq!(ignored[0].path, "server.tls");
        assert!(
            ignored[0].to_string().contains("server.tls"),
            "{}",
            ignored[0]
        );
        // An older generator given a relative state root wrote relative paths,
        // which resolve against the configuration file (SPEC §16.5).
        doc["server"]["state_dir"] = json!("../server");
        doc["server"]["tls"]["identity_dir"] = json!("../identity");
        assert_eq!(
            check_honoured(&doc, Path::new("/s/config"), root)
                .unwrap()
                .len(),
            1
        );
        // The current generated shape reports nothing ignored.
        assert!(
            check_honoured(&generated("/s"), Path::new("/s/config"), root)
                .unwrap()
                .is_empty()
        );
    }

    // T03 (SPEC §15.3): a `server.tls` block that differs from the legacy
    // generated value in any way is an operator edit and is still refused.
    #[test]
    fn an_operator_edited_tls_block_is_still_refused() {
        let edits = [
            json!({"mode": "managed", "identity_dir": "/elsewhere/identity"}),
            json!({"mode": "provided", "identity_dir": "/s/identity"}),
            json!({"mode": "managed", "identity_dir": "/s/identity", "cert": "/c.pem"}),
            json!({"mode": "managed"}),
            json!("managed"),
        ];
        for tls in edits {
            let mut doc = generated("/s");
            doc["server"]["tls"] = tls.clone();
            let error = check_honoured(&doc, Path::new("/s/config"), Path::new("/s")).unwrap_err();
            assert_eq!(error.path, "server.tls", "{tls}");
            assert_eq!(error.code, ConfigErrorCode::UnsupportedCombination);
            assert!(
                error.detail.contains("TLS is not supported"),
                "{}",
                error.detail
            );
        }
    }

    // T03 (SPEC §15.3): every value the standalone role would silently ignore is
    // refused with the path that names it.
    #[test]
    fn values_the_role_would_ignore_are_refused() {
        type Mutation = Box<dyn Fn(&mut Value)>;
        let cases: Vec<(&str, Mutation)> = vec![
            (
                "server.state_dir",
                Box::new(|d| d["server"]["state_dir"] = json!("/elsewhere")),
            ),
            (
                "host.state_dir",
                Box::new(|d| d["host"]["state_dir"] = json!("/s/other")),
            ),
            (
                "server.tls",
                Box::new(|d| {
                    d["server"]["tls"] = json!({"mode": "managed", "identity_dir": "/s/other"})
                }),
            ),
            (
                "server.listeners.inference.bind",
                Box::new(|d| d["server"]["listeners"]["inference"]["bind"] = json!("0.0.0.0:8443")),
            ),
            (
                "server.listeners.management.authentication",
                Box::new(|d| {
                    d["server"]["listeners"]["management"]["authentication"] = json!("none")
                }),
            ),
            (
                "server.listeners.admin",
                Box::new(|d| d["server"]["listeners"]["admin"] = json!({"bind": "127.0.0.1:1"})),
            ),
            (
                "host.connection",
                Box::new(|d| d["host"]["connection"] = json!("remote")),
            ),
            (
                "host.model_store",
                Box::new(|d| d["host"]["model_store"] = json!({"path": "/m"})),
            ),
            (
                "host.resource_policy.memory.system.managed_limit",
                Box::new(|d| {
                    d["host"]["resource_policy"]["memory"]["system"]["managed_limit"] =
                        json!("8GiB")
                }),
            ),
            (
                "host.runtime_profiles",
                Box::new(|d| d["host"]["runtime_profiles"] = json!({"p": {"engine": "vllm"}})),
            ),
        ];
        for (path, mutate) in cases {
            let mut doc = generated("/s");
            mutate(&mut doc);
            let error = check_honoured(&doc, Path::new("/s/config"), Path::new("/s")).unwrap_err();
            assert_eq!(error.path, path);
            assert_eq!(error.code, ConfigErrorCode::UnsupportedCombination);
        }
    }
}
