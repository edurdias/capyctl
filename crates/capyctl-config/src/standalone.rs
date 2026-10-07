//! SPEC §15.3: a standalone document is refused when it states a setting the
//! standalone role does not honour.
//!
//! The standalone role serves both listeners over plain HTTP, keeps its state
//! under the state root it was started with, and derives its embedded host's
//! policy from the installation and capacity it observes
//! (`capyctl-cli/src/standalone_config.rs`). A document that names another state
//! directory, another management address, TLS, a model store, a runtime profile
//! or a numeric resource limit would otherwise be accepted and silently ignored,
//! so an operator could believe a setting is in force that is not. Each such
//! value is refused here with the path that names it.
//!
//! What is accepted is exactly what the role does: the per-role state
//! directories under the state root, the management listener on loopback (any
//! port, owner decision 2026-09-25) with its admin token, the inference listener at any unicast
//! address with a port (design §9: `0.0.0.0:8443` by default, as for a
//! server) with `api_key` or the explicit `none` authentication, an `embedded` connection, `auto` resource values and no
//! runtime profiles. The models directory and model sources, the engine
//! installation (`host.local_engine`), the runtime directory
//! (`host.runtime_dir`), the engines' port range
//! (`host.resource_policy.endpoint_port_range`), the queue bounds
//! (`host.resource_policy.queue`), the parked growth bound
//! (`host.resource_policy.parked_growth_limit`, ADR 0014 amendment A18) and
//! the memory limits (`host.resource_policy.memory.system`, ADR 0025: a size
//! or a share of the observed memory) are honoured as on a host
//! (owner rule 2026-09-25: every setting three ways). A listener moves for one run through `--listen` /
//! `CAPYCTL_INFERENCE_ADDR` or `--management-listen` / `CAPYCTL_MANAGEMENT_ADDR`
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

use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::error::{ConfigError, ConfigErrorCode};

/// Design §9 (owner decisions B and 5): the inference listener of a new
/// standalone or server document serves every interface. The API key stays
/// required (SPEC §13.3); the listener has no engine control path (ADR 0012).
pub const DEFAULT_INFERENCE_BIND: &str = "0.0.0.0:8443";

/// The standalone listeners, their default addresses and default
/// authentication (SPEC §16.5). Management stays on loopback with its admin
/// token; inference defaults to [`DEFAULT_INFERENCE_BIND`] and `api_key`, and
/// may state `none` ([`InferenceAuth`], design §9).
pub const LISTENERS: &[(&str, &str, &str)] = &[
    ("management", "127.0.0.1:7443", "admin_token"),
    ("inference", DEFAULT_INFERENCE_BIND, "api_key"),
];

/// Design §9: the one rule for an inference address, whichever role, document,
/// flag or variable states it: a socket address with a non-zero port that is
/// not multicast. IPv6 (`[::]:8443`) is accepted.
pub fn inference_address(text: &str) -> Option<SocketAddr> {
    text.parse::<SocketAddr>()
        .ok()
        .filter(|address| address.port() != 0 && !address.ip().is_multicast())
}

/// Design §9: the inference address a standalone `document` binds:
/// `server.listeners.inference.bind` when stated, else
/// [`DEFAULT_INFERENCE_BIND`]. Refuses port 0, a multicast address and
/// anything that is not a socket address.
pub fn inference_bind(document: &Value) -> Result<SocketAddr, ConfigError> {
    const PATH: &str = "server.listeners.inference.bind";
    let Some(bind) = document["server"]["listeners"]["inference"].get("bind") else {
        return Ok(DEFAULT_INFERENCE_BIND.parse().expect("valid default"));
    };
    bind.as_str().and_then(inference_address).ok_or_else(|| {
        refuse(
            PATH,
            "the inference listener binds an address with a non-zero port that is not \
             multicast, e.g. 0.0.0.0:8443, 127.0.0.1:8443 or a Tailscale address",
        )
    })
}

/// SPEC §16.5 (owner decision 2026-09-25): the one rule for the standalone
/// management address, whichever document, flag or variable states it: a
/// loopback socket address with a non-zero port. Management carries the admin
/// token and never leaves loopback.
pub fn management_address(text: &str) -> Option<SocketAddr> {
    text.parse::<SocketAddr>()
        .ok()
        .filter(|address| address.port() != 0 && address.ip().is_loopback())
}

/// The standalone management listener's default address.
pub const DEFAULT_MANAGEMENT_BIND: &str = "127.0.0.1:7443";

/// The management address a standalone `document` binds:
/// `server.listeners.management.bind` when stated, else
/// [`DEFAULT_MANAGEMENT_BIND`].
pub fn management_bind(document: &Value) -> Result<SocketAddr, ConfigError> {
    let Some(bind) = document["server"]["listeners"]["management"].get("bind") else {
        return Ok(DEFAULT_MANAGEMENT_BIND.parse().expect("valid default"));
    };
    bind.as_str().and_then(management_address).ok_or_else(|| {
        refuse(
            "server.listeners.management.bind",
            "the management listener binds a loopback address with a non-zero port, e.g. \
             127.0.0.1:7443",
        )
    })
}

/// Design §9 (owner decision B): whether the inference listener requires the
/// API key. `api_key` is the default; `none` is the operator's explicit
/// opt-out, stated as `listeners.inference.authentication: none`, the
/// `--no-inference-auth` flag or `CAPYCTL_INFERENCE_AUTH=none`. Management and
/// the server's other listeners keep their fixed authentication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InferenceAuth {
    /// SPEC §13.3 (T37): every inference route requires the bearer key.
    ApiKey,
    /// The router's key check is off (design §9).
    None,
}

impl InferenceAuth {
    /// The document spelling: `api_key` or `none`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ApiKey => "api_key",
            Self::None => "none",
        }
    }

    /// Parse the document spelling; anything else is `None`.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "api_key" => Some(Self::ApiKey),
            "none" => Some(Self::None),
            _ => None,
        }
    }
}

/// Design §9: the `authentication` of an inference `listener` block: `api_key`
/// when unstated, else `api_key` or `none`. `path` names the field in errors.
pub fn listener_auth(listener: &Value, path: &str) -> Result<InferenceAuth, ConfigError> {
    match listener.get("authentication") {
        None => Ok(InferenceAuth::ApiKey),
        Some(value) => value
            .as_str()
            .and_then(InferenceAuth::parse)
            .ok_or_else(|| {
                refuse(
                    path,
                    "the inference listener's authentication is `api_key` (the default) or `none`",
                )
            }),
    }
}

/// Design §9: the inference authentication of a standalone `document`:
/// `none` when `no_auth_flag` (`--no-inference-auth`) is set, else
/// `server.listeners.inference.authentication` (default `api_key`).
/// `CAPYCTL_INFERENCE_AUTH` sits between the two; the role applies it
/// (`capyctl_cli::roles::effective_inference_auth`).
pub fn inference_auth(document: &Value, no_auth_flag: bool) -> Result<InferenceAuth, ConfigError> {
    let stated = listener_auth(
        &document["server"]["listeners"]["inference"],
        "server.listeners.inference.authentication",
    )?;
    Ok(if no_auth_flag {
        InferenceAuth::None
    } else {
        stated
    })
}

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
    "standalone listeners serve plain HTTP; TLS is not supported, remove this block";

/// SPEC §15.2 (R13): whether `tls` is exactly the block an older capyctl generator
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

/// The memory limits a standalone document may state under
/// `host.resource_policy.memory.system` (owner decision 2026-10-03: standalone
/// is a server and one host, every setting three ways). `auto` keeps the
/// derived default. Owner decision 2026-10-06 (ADR 0014 amendment A17): the
/// parked limit is one of them, so a parked footprint above the derived
/// quarter of memory can be admitted.
pub const STATED_MEMORY_LIMITS: &[&str] = &["managed_limit", "free_reserve", "parked_limit"];

/// A stated standalone memory limit: a size, or a whole percentage of the
/// memory the host observes at start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryShare {
    Bytes(i64),
    Percent(i64),
}

impl MemoryShare {
    /// `auto` is `None`; `N%` is a whole percentage from 0 to 100; anything
    /// else is a size (`90GiB`). The field's own range is checked by
    /// [`check_memory_limit`].
    pub fn parse(text: &str) -> Result<Option<Self>, String> {
        if text == "auto" {
            return Ok(None);
        }
        if let Some(number) = text.strip_suffix('%') {
            return match number.parse::<i64>() {
                Ok(percent) if (0..=100).contains(&percent) && !number.starts_with('+') => {
                    Ok(Some(Self::Percent(percent)))
                }
                _ => Err(format!(
                    "`{text}` is not a whole percentage from 0% to 100%"
                )),
            };
        }
        crate::effective::parse_bytes(text)
            .map(|bytes| Some(Self::Bytes(bytes)))
            .map_err(|_| {
                format!("`{text}` is neither `auto`, a size such as `90GiB` nor a percentage such as `70%`")
            })
    }

    /// The bytes this share is of `capacity`.
    pub fn of(self, capacity: i64) -> i64 {
        match self {
            Self::Bytes(bytes) => bytes,
            Self::Percent(percent) => capacity / 100 * percent,
        }
    }
}

/// One stated memory limit's form and range: the managed limit is above zero
/// and the free reserve below the whole memory; a parked limit of zero parks
/// nothing. Whether the limits fit the observed memory together, and the
/// parked limit the managed one, is checked at start, when it is known.
fn check_memory_limit(field: &str, value: &Value, path: &str) -> Result<(), ConfigError> {
    let Some(text) = value.as_str() else {
        return Err(refuse(
            path,
            "a memory limit is `auto`, a size or a percentage",
        ));
    };
    let share = MemoryShare::parse(text).map_err(|detail| refuse(path, detail))?;
    match (field, share) {
        ("managed_limit", Some(MemoryShare::Bytes(0) | MemoryShare::Percent(0))) => {
            Err(refuse(path, "the managed limit must be above zero"))
        }
        ("free_reserve", Some(MemoryShare::Percent(100))) => {
            Err(refuse(path, "the free reserve must leave memory to manage"))
        }
        _ => Ok(()),
    }
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
    // Owner decision 2026-09-25: the top-level `state_dir` is the YAML form
    // of the state root, read before the role starts (`--state-dir` and
    // `CAPYCTL_STATE_DIR` win over it); it must be a path.
    if let Some(value) = document.get("state_dir") {
        value
            .as_str()
            .filter(|text| !text.is_empty())
            .ok_or_else(|| refuse("state_dir", "must be a path"))?;
    }
    check_state_dir(server, "server.state_dir", config_dir, &root.join("server"))?;
    check_state_dir(host, "host.state_dir", config_dir, &root.join("host"))?;
    if let Some(tls) = server.get("tls") {
        if !is_legacy_generated_tls(tls, config_dir, &root) {
            return Err(refuse("server.tls", TLS_REFUSAL));
        }
        ignored.push(IgnoredSetting {
            path: "server.tls".into(),
            reason: "an older capyctl generated this block; the standalone listeners serve \
                     plain HTTP. It has no effect and may be removed"
                .into(),
        });
    }
    if let Some(listeners) = server.get("listeners").and_then(Value::as_object) {
        for (name, listener) in listeners {
            let path = format!("server.listeners.{name}");
            let Some((_, _, authentication)) = LISTENERS.iter().find(|(known, _, _)| known == name)
            else {
                return Err(refuse(
                    &path,
                    "standalone has only the `management` and `inference` listeners",
                ));
            };
            if name == "inference" {
                // Design §9: any unicast address, as for a server.
                inference_bind(document)?;
            } else {
                // SPEC §16.5, owner decision 2026-09-25: management stays on
                // loopback, at any port.
                management_bind(document)?;
            }
            if name == "inference" {
                // Design §9: `api_key` (default) or the explicit `none`.
                listener_auth(listener, &format!("{path}.authentication"))?;
            } else if let Some(mode) = listener.get("authentication") {
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
    // Owner decision 2026-09-25: the models directory and the model-source
    // policy are honoured here exactly as on a host
    // (`crate::model_settings`); a malformed value is refused before any
    // side effect.
    if host.get("model_store").is_some() || host.get("model_sources").is_some() {
        crate::model_settings::resolve(
            host,
            &Default::default(),
            &Default::default(),
            Some(Path::new("/")),
        )
        .map_err(|error| refuse(&format!("host.{}", error.path), error.detail))?;
    }
    // Owner rule 2026-09-25 (`crate::engine_settings`): the engine
    // installation (`local_engine`), the runtime directory and the engines'
    // port range are honoured here exactly as on a host; a malformed value is
    // refused before any side effect. Every other resource value is `auto`.
    crate::engine_settings::EngineOverrides::from_document(host)
        .map_err(|error| refuse(&format!("host.{}", error.path), error.detail))?;
    if let Some(policy) = host.get("resource_policy") {
        let mut policy = policy.clone();
        if let Some(map) = policy.as_object_mut() {
            map.remove("endpoint_port_range");
            // ADR 0028 §3 (owner rule: standalone is a server plus one
            // host): the group policy is honoured as on a host; its values
            // were checked by `from_document` above.
            map.remove("groups");
            // The queue bounds are honoured as on a host; their values are
            // checked when the embedded host's policy is normalized.
            map.remove("queue");
            // ADR 0014 amendment A18: the parked growth bound is honoured
            // as on a host: `auto`, `off`, a percentage or a size.
            if let Some(value) = map.remove("parked_growth_limit") {
                let path = "host.resource_policy.parked_growth_limit";
                let text = value
                    .as_str()
                    .ok_or_else(|| refuse(path, "`auto`, `off`, a size or a percentage"))?;
                crate::parked_growth::ParkedGrowthLimit::parse(text)
                    .map_err(|detail| refuse(path, detail))?;
            }
        }
        // Owner decision 2026-10-03: the memory limits take a size or a
        // share of the observed memory; the derived value stays the default.
        if let Some(system) = policy
            .get_mut("memory")
            .and_then(|memory| memory.get_mut("system"))
            .and_then(Value::as_object_mut)
        {
            for field in STATED_MEMORY_LIMITS {
                if let Some(value) = system.remove(*field) {
                    check_memory_limit(
                        field,
                        &value,
                        &format!("host.resource_policy.memory.system.{field}"),
                    )?;
                }
            }
        }
        only_auto(&policy, "host.resource_policy")?;
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

    /// The generated shape before design §9 (inference on loopback), which
    /// must keep validating; the current generator is tested in `defaults.rs`.
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
        // T14 (owner decision 2026-09-25): the models directory and the
        // model-source policy are honoured, as on a host.
        let mut doc = generated("/s");
        doc["host"]["model_store"] = json!({"path": "/data/models"});
        doc["host"]["model_sources"] =
            json!({"huggingface": "disabled", "http": "allowed", "max_bytes": "100GiB"});
        check_honoured(&doc, Path::new("/s/config"), root).unwrap();
    }

    // T03 (SPEC §15.2, R13): a document written by an older capyctl generator carries
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
                Box::new(|d| {
                    d["server"]["listeners"]["inference"]["bind"] = json!("224.0.0.1:8443")
                }),
            ),
            (
                "server.listeners.management.bind",
                Box::new(|d| {
                    d["server"]["listeners"]["management"]["bind"] = json!("0.0.0.0:7443")
                }),
            ),
            (
                "server.listeners.management.bind",
                Box::new(|d| d["server"]["listeners"]["management"]["bind"] = json!("127.0.0.1:0")),
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
                "host.model_store.path",
                Box::new(|d| d["host"]["model_store"] = json!({"path": "relative/m"})),
            ),
            (
                "host.model_sources.max_bytes",
                Box::new(|d| d["host"]["model_sources"] = json!({"max_bytes": "0B"})),
            ),
            (
                "host.resource_policy.memory.system.managed_limit",
                Box::new(|d| {
                    d["host"]["resource_policy"]["memory"]["system"]["managed_limit"] =
                        json!("8 gigabytes")
                }),
            ),
            (
                "host.resource_policy.memory.accounting",
                Box::new(|d| d["host"]["resource_policy"]["memory"]["accounting"] = json!("8GiB")),
            ),
            (
                "host.runtime_profiles",
                Box::new(|d| d["host"]["runtime_profiles"] = json!({"p": {"engine": "vllm"}})),
            ),
            (
                "host.local_engine.deep_park",
                Box::new(|d| d["host"]["local_engine"] = json!({"deep_park": "disabled"})),
            ),
            (
                "host.runtime_dir",
                Box::new(|d| d["host"]["runtime_dir"] = json!("relative/runtime")),
            ),
            (
                "host.resource_policy.endpoint_port_range",
                Box::new(|d| {
                    d["host"]["resource_policy"]["endpoint_port_range"] =
                        json!({"start": 80, "end": 90})
                }),
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

    // T03 (owner rule 2026-09-25: every setting three ways): the engine
    // installation, the runtime directory and the port range are honoured in
    // the document, as on a host.
    #[test]
    fn the_engine_settings_are_accepted_in_the_document() {
        // Owner decision 2026-09-25: the management listener moves to any
        // loopback address with a port.
        let mut moved = generated("/s");
        moved["server"]["listeners"]["management"]["bind"] = json!("127.0.0.1:7543");
        check_honoured(&moved, Path::new("/s/config"), Path::new("/s")).unwrap();
        assert_eq!(
            management_bind(&moved).unwrap(),
            "127.0.0.1:7543".parse().unwrap()
        );
        let mut doc = generated("/s");
        doc["server"]["state_dir"] = json!("/s/server");
        doc["host"]["local_engine"] = json!({
            "vllm": "/opt/vllm/bin/vllm", "sglang": "/opt/sglang/bin/python3",
            "build_fingerprint": "vllm 0.29.0", "args": ["--enforce-eager"],
            "kv_cache": "8GiB", "deep_park": "off", "trust_remote_code": true,
            "installation_drift": "refuse",
        });
        doc["host"]["runtime_dir"] = json!("/opt/capyctl/runtime");
        doc["host"]["resource_policy"]["endpoint_port_range"] = json!({"start": 9000, "end": 9099});
        crate::validate(
            &serde_json::to_string(&doc).unwrap(),
            crate::ConfigKind::Standalone,
        )
        .unwrap();
        check_honoured(&doc, Path::new("/s/config"), Path::new("/s")).unwrap();
    }

    // T14 (ADR 0028 §3, R21): standalone honours `host.resource_policy.groups`
    // and refuses a malformed one with its path.
    #[test]
    fn standalone_honours_the_group_policy() {
        let mut doc = generated("/s");
        doc["host"]["resource_policy"]["groups"] = json!({
            "peer_address": "192.0.2.10",
            "rendezvous_port_range": {"start": 26000, "end": 26009},
            "require_rdma": true
        });
        crate::validate(
            &serde_json::to_string(&doc).unwrap(),
            crate::ConfigKind::Standalone,
        )
        .unwrap();
        check_honoured(&doc, Path::new("/s/config"), Path::new("/s")).unwrap();
        doc["host"]["resource_policy"]["groups"]["peer_address"] = json!("127.0.0.1");
        let error = check_honoured(&doc, Path::new("/s/config"), Path::new("/s")).unwrap_err();
        assert!(error.to_string().contains("groups.peer_address"), "{error}");
    }

    fn check(document: &Value) -> Result<Vec<IgnoredSetting>, ConfigError> {
        check_honoured(document, Path::new("/s/config"), Path::new("/s"))
    }

    // T03 (owner rule 2026-09-25: standalone is a server and one host, every
    // setting three ways; found live 2026-10-03): the memory limits under
    // `host.resource_policy.memory.system` take a size or a share of the
    // observed memory, and `--set` and `CAPYCTL_SET__…` override them.
    #[test]
    fn the_memory_limits_take_a_size_or_a_share() {
        use crate::setting_overrides::SettingOverrides;
        for (managed, reserve) in [("90GiB", "auto"), ("75%", "10%"), ("auto", "8GiB")] {
            let mut doc = generated("/s");
            doc["host"]["resource_policy"]["memory"]["system"] =
                json!({"managed_limit": managed, "free_reserve": reserve});
            crate::validate(
                &serde_json::to_string(&doc).unwrap(),
                crate::ConfigKind::Standalone,
            )
            .unwrap();
            check(&doc).unwrap_or_else(|error| panic!("{managed}/{reserve}: {error}"));
        }
        for (field, bad) in [
            ("managed_limit", "0B"),
            ("managed_limit", "0%"),
            ("managed_limit", "101%"),
            ("managed_limit", "-1GiB"),
            ("managed_limit", "7.5%"),
            ("managed_limit", "lots"),
            ("free_reserve", "100%"),
            ("free_reserve", "1 GiB"),
        ] {
            let mut doc = generated("/s");
            doc["host"]["resource_policy"]["memory"]["system"][field] = json!(bad);
            let error = check(&doc).expect_err(bad);
            assert_eq!(
                error.path,
                format!("host.resource_policy.memory.system.{field}"),
                "{bad}"
            );
        }
        let overrides = SettingOverrides::parse(
            crate::ConfigKind::Standalone,
            &["host.resource_policy.memory.system.managed_limit=90GiB".to_owned()],
            &[
                (
                    "CAPYCTL_SET__HOST__RESOURCE_POLICY__MEMORY__SYSTEM__MANAGED_LIMIT".to_owned(),
                    "70%".to_owned(),
                ),
                (
                    "CAPYCTL_SET__HOST__RESOURCE_POLICY__MEMORY__SYSTEM__FREE_RESERVE".to_owned(),
                    "12GiB".to_owned(),
                ),
            ],
        )
        .unwrap();
        let applied = overrides.apply_and_validate(generated("/s")).unwrap();
        let system = &applied["host"]["resource_policy"]["memory"]["system"];
        assert_eq!(system["managed_limit"], "90GiB", "--set wins");
        assert_eq!(system["free_reserve"], "12GiB", "the environment over YAML");
        check(&applied).unwrap();
    }

    #[test]
    fn a_memory_share_resolves_against_the_observed_capacity() {
        assert_eq!(MemoryShare::parse("auto").unwrap(), None);
        assert_eq!(
            MemoryShare::parse("90GiB").unwrap(),
            Some(MemoryShare::Bytes(90 << 30))
        );
        assert_eq!(
            MemoryShare::parse("75%").unwrap(),
            Some(MemoryShare::Percent(75))
        );
        assert_eq!(MemoryShare::Percent(75).of(200 << 30), 150 << 30);
        assert_eq!(MemoryShare::Bytes(90 << 30).of(200 << 30), 90 << 30);
    }

    // T03 (owner decision 2026-10-07, ADR 0014 amendment A18): the parked
    // growth bound is set three ways with one precedence, `--set` over
    // `CAPYCTL_SET__…` over the YAML, and takes `auto`, `off`, a percentage
    // or a size.
    #[test]
    fn the_parked_growth_limit_is_set_three_ways() {
        use crate::setting_overrides::SettingOverrides;
        const FLAG: &str = "host.resource_policy.parked_growth_limit=50%";
        const ENV: &str = "CAPYCTL_SET__HOST__RESOURCE_POLICY__PARKED_GROWTH_LIMIT";
        let mut yaml = generated("/s");
        yaml["host"]["resource_policy"]["parked_growth_limit"] = json!("8GiB");
        let growth = |flags: &[&str], env: &[(&str, &str)]| {
            let overrides = SettingOverrides::parse(
                crate::ConfigKind::Standalone,
                &flags.iter().map(|f| (*f).to_owned()).collect::<Vec<_>>(),
                &env.iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect::<Vec<_>>(),
            )
            .unwrap();
            let applied = overrides.apply_and_validate(yaml.clone()).unwrap();
            check(&applied).unwrap();
            applied["host"]["resource_policy"]["parked_growth_limit"].clone()
        };
        assert_eq!(growth(&[FLAG], &[(ENV, "off")]), "50%", "the flag wins");
        assert_eq!(
            growth(&[], &[(ENV, "off")]),
            "off",
            "the environment over YAML"
        );
        assert_eq!(growth(&[], &[]), "8GiB", "the YAML");
        for good in ["auto", "off", "0%", "100%", "250%", "0B", "8GiB"] {
            let mut doc = generated("/s");
            doc["host"]["resource_policy"]["parked_growth_limit"] = json!(good);
            check(&doc).unwrap_or_else(|error| panic!("{good}: {error}"));
        }
        for bad in ["10001%", "-1GiB", "7.5%", "lots", "1 GiB", "true"] {
            let mut doc = generated("/s");
            doc["host"]["resource_policy"]["parked_growth_limit"] = json!(bad);
            let error = check(&doc).expect_err(bad);
            assert_eq!(
                error.path, "host.resource_policy.parked_growth_limit",
                "{bad}"
            );
        }
    }

    // T03 (owner decision 2026-10-06, ADR 0014 amendment A17): the parked
    // limit is set three ways with one precedence, `--set` over
    // `CAPYCTL_SET__…` over the YAML, and takes `auto`, a size or a share.
    #[test]
    fn the_parked_limit_is_set_three_ways() {
        use crate::setting_overrides::SettingOverrides;
        const FLAG: &str = "host.resource_policy.memory.system.parked_limit=50GiB";
        const ENV: &str = "CAPYCTL_SET__HOST__RESOURCE_POLICY__MEMORY__SYSTEM__PARKED_LIMIT";
        let mut yaml = generated("/s");
        yaml["host"]["resource_policy"]["memory"]["system"]["parked_limit"] = json!("40GiB");
        let parked = |flags: &[&str], env: &[(&str, &str)]| {
            let overrides = SettingOverrides::parse(
                crate::ConfigKind::Standalone,
                &flags.iter().map(|f| (*f).to_owned()).collect::<Vec<_>>(),
                &env.iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect::<Vec<_>>(),
            )
            .unwrap();
            let applied = overrides.apply_and_validate(yaml.clone()).unwrap();
            check(&applied).unwrap();
            applied["host"]["resource_policy"]["memory"]["system"]["parked_limit"].clone()
        };
        assert_eq!(parked(&[FLAG], &[(ENV, "45GiB")]), "50GiB", "the flag wins");
        assert_eq!(
            parked(&[], &[(ENV, "45GiB")]),
            "45GiB",
            "the environment over YAML"
        );
        assert_eq!(parked(&[], &[]), "40GiB", "the YAML");
        for good in ["auto", "0B", "0%", "40GiB", "35%", "100%"] {
            let mut doc = generated("/s");
            doc["host"]["resource_policy"]["memory"]["system"]["parked_limit"] = json!(good);
            check(&doc).unwrap_or_else(|error| panic!("{good}: {error}"));
        }
        for bad in ["101%", "-1GiB", "7.5%", "lots", "1 GiB"] {
            let mut doc = generated("/s");
            doc["host"]["resource_policy"]["memory"]["system"]["parked_limit"] = json!(bad);
            let error = check(&doc).expect_err(bad);
            assert_eq!(
                error.path, "host.resource_policy.memory.system.parked_limit",
                "{bad}"
            );
        }
    }

    // T03 (owner rule 2026-09-25: standalone is a server and one host, every
    // setting three ways; found live 2026-10-02): the host queue bounds,
    // `host.resource_policy.queue`, are honoured in a standalone document and
    // set by `--set` and `CAPYCTL_SET__…`, as on a host.
    #[test]
    fn the_host_queue_bounds_are_accepted() {
        use crate::setting_overrides::SettingOverrides;
        let mut doc = generated("/s");
        doc["host"]["resource_policy"]["queue"] =
            json!({"stream_idle_timeout": "600s", "request_deadline": "3600s"});
        crate::validate(
            &serde_json::to_string(&doc).unwrap(),
            crate::ConfigKind::Standalone,
        )
        .unwrap();
        check(&doc).unwrap();
        let overrides = SettingOverrides::parse(
            crate::ConfigKind::Standalone,
            &["host.resource_policy.queue.stream_idle_timeout=900s".to_owned()],
            &[(
                "CAPYCTL_SET__HOST__RESOURCE_POLICY__QUEUE__STREAM_IDLE_TIMEOUT".to_owned(),
                "450s".to_owned(),
            )],
        )
        .unwrap();
        let applied = overrides.apply_and_validate(doc).unwrap();
        let queue = &applied["host"]["resource_policy"]["queue"];
        assert_eq!(queue["stream_idle_timeout"], "900s", "--set wins");
        assert_eq!(queue["request_deadline"], "3600s", "YAML kept");
        check(&applied).unwrap();
    }

    // T03 (design §9): a document an older capyctl generated, with inference on
    // loopback, still validates and binds where it says (the one-time
    // migration moves it before the bind is read).
    #[test]
    fn the_old_loopback_document_still_validates() {
        let doc = generated("/s");
        assert!(check(&doc).is_ok());
        assert_eq!(inference_bind(&doc).unwrap().to_string(), "127.0.0.1:8443");
    }

    // T03 (design §9): a document that states no inference bind binds the
    // default on every interface.
    #[test]
    fn an_unstated_inference_bind_is_the_all_interfaces_default() {
        assert_eq!(DEFAULT_INFERENCE_BIND, "0.0.0.0:8443");
        let bare = json!({"schema_version": 1, "kind": "standalone", "name": "x"});
        assert_eq!(inference_bind(&bare).unwrap().to_string(), "0.0.0.0:8443");
        let mut doc = generated("/s");
        doc["server"]["listeners"]["inference"]
            .as_object_mut()
            .unwrap()
            .remove("bind");
        assert_eq!(inference_bind(&doc).unwrap().to_string(), "0.0.0.0:8443");
    }

    // T03 T37 (design §9): the inference bind accepts any unicast address with
    // a port; the management listener stays on loopback.
    #[test]
    fn bind_rules() {
        for ok in [
            "0.0.0.0:8443",
            "100.64.0.5:8443",
            "[::]:8443",
            "127.0.0.1:9000",
        ] {
            let mut d = generated("/s");
            d["server"]["listeners"]["inference"]["bind"] = ok.into();
            assert!(check(&d).is_ok(), "{ok}");
            assert_eq!(inference_bind(&d).unwrap(), ok.parse().unwrap(), "{ok}");
        }
        for bad in ["0.0.0.0:0", "224.0.0.1:8443", "[ff02::1]:8443", "nonsense"] {
            let mut d = generated("/s");
            d["server"]["listeners"]["inference"]["bind"] = bad.into();
            let error = check(&d).unwrap_err();
            assert_eq!(error.path, "server.listeners.inference.bind", "{bad}");
            assert!(inference_bind(&d).is_err(), "{bad}");
        }
        let mut d = generated("/s");
        d["server"]["listeners"]["inference"]["bind"] = json!(8443);
        assert!(check(&d).is_err());
        let mut d = generated("/s");
        d["server"]["listeners"]["management"]["bind"] = "0.0.0.0:7443".into();
        assert!(check(&d).is_err());
    }

    // T03 T37 (design §9): `authentication: none` is accepted for inference
    // only; `--no-inference-auth` turns the key off for one run.
    #[test]
    fn authentication_none_is_inference_only() {
        let mut d = generated("/s");
        assert_eq!(inference_auth(&d, false).unwrap(), InferenceAuth::ApiKey);
        d["server"]["listeners"]["inference"]["authentication"] = "none".into();
        assert!(check(&d).is_ok());
        assert_eq!(inference_auth(&d, false).unwrap(), InferenceAuth::None);
        assert_eq!(
            inference_auth(&generated("/s"), true).unwrap(),
            InferenceAuth::None
        );
        let bare = json!({"schema_version": 1, "kind": "standalone", "name": "x"});
        assert_eq!(inference_auth(&bare, false).unwrap(), InferenceAuth::ApiKey);
        d["server"]["listeners"]["management"]["authentication"] = "none".into();
        let error = check(&d).unwrap_err();
        assert_eq!(error.path, "server.listeners.management.authentication");
        for bad in [json!("token"), json!("None"), json!(true)] {
            let mut d = generated("/s");
            d["server"]["listeners"]["inference"]["authentication"] = bad.clone();
            let error = check(&d).unwrap_err();
            assert_eq!(
                error.path, "server.listeners.inference.authentication",
                "{bad}"
            );
            assert!(inference_auth(&d, false).is_err(), "{bad}");
        }
    }
}
