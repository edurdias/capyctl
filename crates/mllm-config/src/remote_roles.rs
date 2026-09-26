//! SPEC §§3–4, §15: role startup resolves explicit authority, never engines or
//! guessed remote addresses. Parsing is side-effect free.
use crate::{parse_strict, ConfigError, ConfigErrorCode, ConfigKind};
use serde_json::{json, Map, Value};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub state_dir: PathBuf,
    pub identity_dir: PathBuf,
    pub management: SocketAddr,
    pub inference: SocketAddr,
    /// Design §9: `listeners.inference.authentication`, `api_key` or the
    /// explicit `none`. `--no-inference-auth` and `MLLM_INFERENCE_AUTH`
    /// override it for one run.
    pub inference_auth: crate::standalone::InferenceAuth,
    pub bootstrap: SocketAddr,
    pub control: SocketAddr,
    pub bootstrap_address: String,
    pub control_address: String,
    pub certificate_name: String,
    /// `shutdown.drain_timeout`: how long a signalled role lets admitted work finish.
    pub drain_timeout: Duration,
    /// SPEC §6.5 (W5): `lifecycle_defaults`, the controller-owned idle policy.
    pub idle: IdleTimeouts,
    /// Owner decision 2026-09-23: `control.heartbeat_suspend_after` and
    /// `control.heartbeat_lost_after`.
    pub heartbeat: HeartbeatTimeouts,
    /// SPEC §10 (W10): `switching.drain_timeout`, how long a switch lets its
    /// victims' accepted requests drain before it fails and they serve again.
    pub switch_drain_timeout: Duration,
    /// SPEC §17 (M80): `observability.timing_header`, whether the router adds
    /// the `x-mllm-timing` header to inference responses. Off by default.
    pub timing_header: bool,
}
/// SPEC §17 (M80): `observability.timing_header` of a server (or a standalone
/// document's `server:` block). Omitted, off; anything but a boolean is refused.
pub fn timing_header(document: &Value) -> Result<bool, ConfigError> {
    match document
        .get("observability")
        .and_then(|o| o.get("timing_header"))
    {
        None => Ok(false),
        Some(value) => value.as_bool().ok_or_else(|| {
            ConfigError::new(
                ConfigErrorCode::UnsupportedCombination,
                "observability.timing_header",
                "must be true or false",
            )
        }),
    }
}
/// SPEC §10 (W10): the switch drain bound when a server document names none,
/// the same as a role's `shutdown.drain_timeout`.
pub const DEFAULT_SWITCH_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
/// Bounds on `switching.drain_timeout`: a switch always gives accepted work a
/// moment, and never holds a waiting deployment for more than ten minutes.
pub const MIN_SWITCH_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
pub const MAX_SWITCH_DRAIN_TIMEOUT: Duration = Duration::from_secs(600);
/// SPEC §10 (W10): `switching.drain_timeout` of a server (or a standalone
/// document's `server:` block). Omitted, 30 s; a value outside 1 s to 600 s is
/// refused, never read as the default. A drain that does not finish in time
/// fails the switch; nothing is killed.
pub fn switch_drain_timeout(document: &Value) -> Result<Duration, ConfigError> {
    match document
        .get("switching")
        .and_then(|s| s.get("drain_timeout"))
    {
        None => Ok(DEFAULT_SWITCH_DRAIN_TIMEOUT),
        Some(value) => value
            .as_str()
            .and_then(|text| crate::effective::parse_duration_ms(text).ok())
            .and_then(|ms| u64::try_from(ms).ok())
            .map(Duration::from_millis)
            .filter(|bound| (MIN_SWITCH_DRAIN_TIMEOUT..=MAX_SWITCH_DRAIN_TIMEOUT).contains(bound))
            .ok_or_else(|| {
                ConfigError::new(
                    ConfigErrorCode::UnsupportedCombination,
                    "switching.drain_timeout",
                    "must be a duration from 1s to 600s",
                )
            }),
    }
}
/// Owner decision 2026-09-23: server and host agent exchange heartbeats every
/// second on the control session. After `suspend_after` without hearing from a
/// host the server suspends dispatch to it (accounting kept, nothing
/// released); after `lost_after` it treats the session as lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeartbeatTimeouts {
    pub suspend_after: Duration,
    pub lost_after: Duration,
}
/// The fixed heartbeat period (owner decision 2026-09-23).
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
pub const DEFAULT_HEARTBEAT_SUSPEND_AFTER: Duration = Duration::from_secs(5);
pub const DEFAULT_HEARTBEAT_LOST_AFTER: Duration = Duration::from_secs(30);
/// Bounds: suspension needs at least two missed heartbeats, so one late frame
/// never suspends a host; neither may be so long that a frozen host holds
/// requests for minutes.
pub const MIN_HEARTBEAT_SUSPEND_AFTER: Duration = Duration::from_secs(2);
pub const MAX_HEARTBEAT_SUSPEND_AFTER: Duration = Duration::from_secs(120);
pub const MIN_HEARTBEAT_LOST_AFTER: Duration = Duration::from_secs(3);
pub const MAX_HEARTBEAT_LOST_AFTER: Duration = Duration::from_secs(600);
impl Default for HeartbeatTimeouts {
    fn default() -> Self {
        Self {
            suspend_after: DEFAULT_HEARTBEAT_SUSPEND_AFTER,
            lost_after: DEFAULT_HEARTBEAT_LOST_AFTER,
        }
    }
}
/// Owner decision 2026-09-23: the heartbeat timeouts of a server document.
/// Omitted, 5 s and 30 s. A value outside its bounds, or a lost bound not
/// beyond the suspend bound, is refused, never read as the default.
pub fn heartbeat_timeouts(document: &Value) -> Result<HeartbeatTimeouts, ConfigError> {
    let read =
        |field: &str, default: Duration, min: Duration, max: Duration, bounds: &str| match document
            .get("control")
            .and_then(|c| c.get(field))
        {
            None => Ok(default),
            Some(value) => value
                .as_str()
                .and_then(|text| crate::effective::parse_duration_ms(text).ok())
                .and_then(|ms| u64::try_from(ms).ok())
                .map(Duration::from_millis)
                .filter(|bound| (min..=max).contains(bound))
                .ok_or_else(|| {
                    ConfigError::new(
                        ConfigErrorCode::UnsupportedCombination,
                        format!("control.{field}"),
                        format!("must be a duration from {bounds}"),
                    )
                }),
        };
    let timeouts = HeartbeatTimeouts {
        suspend_after: read(
            "heartbeat_suspend_after",
            DEFAULT_HEARTBEAT_SUSPEND_AFTER,
            MIN_HEARTBEAT_SUSPEND_AFTER,
            MAX_HEARTBEAT_SUSPEND_AFTER,
            "2s to 120s",
        )?,
        lost_after: read(
            "heartbeat_lost_after",
            DEFAULT_HEARTBEAT_LOST_AFTER,
            MIN_HEARTBEAT_LOST_AFTER,
            MAX_HEARTBEAT_LOST_AFTER,
            "3s to 600s",
        )?,
    };
    if timeouts.lost_after <= timeouts.suspend_after {
        return Err(ConfigError::new(
            ConfigErrorCode::UnsupportedCombination,
            "control.heartbeat_lost_after",
            "must be longer than control.heartbeat_suspend_after",
        ));
    }
    Ok(timeouts)
}
/// SPEC §6.5 (W5): after `ready_idle_timeout` with nothing in flight a Ready
/// instance parks at its declared tier (a restart-only one stops); after
/// `parked_idle_timeout` a parked instance stops. Omitted, a timer is off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IdleTimeouts {
    pub ready_idle: Option<Duration>,
    pub parked_idle: Option<Duration>,
}
/// The shortest and longest idle timeout a server document may name.
pub const MIN_IDLE_TIMEOUT: Duration = Duration::from_secs(1);
pub const MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(7 * 24 * 3600);
/// SPEC §6.5, §16.1 (W5): `lifecycle_defaults.ready_idle_timeout` and
/// `lifecycle_defaults.parked_idle_timeout` of a server document. A value
/// outside 1 s to 7 days is refused, never read as off.
pub fn idle_timeouts(document: &Value) -> Result<IdleTimeouts, ConfigError> {
    let read = |field: &str| -> Result<Option<Duration>, ConfigError> {
        match document
            .get("lifecycle_defaults")
            .and_then(|d| d.get(field))
        {
            None => Ok(None),
            Some(value) => value
                .as_str()
                .and_then(|text| crate::effective::parse_duration_ms(text).ok())
                .and_then(|ms| u64::try_from(ms).ok())
                .map(Duration::from_millis)
                .filter(|timeout| (MIN_IDLE_TIMEOUT..=MAX_IDLE_TIMEOUT).contains(timeout))
                .map(Some)
                .ok_or_else(|| {
                    ConfigError::new(
                        ConfigErrorCode::UnsupportedCombination,
                        format!("lifecycle_defaults.{field}"),
                        "must be a duration from 1s to 7d",
                    )
                }),
        }
    };
    Ok(IdleTimeouts {
        ready_idle: read("ready_idle_timeout")?,
        parked_idle: read("parked_idle_timeout")?,
    })
}
#[derive(Debug, Clone)]
pub struct HostConfig {
    pub name: String,
    pub state_dir: PathBuf,
    pub identity_dir: PathBuf,
    pub runtime_dir: PathBuf,
    /// Whether the document names `runtime_dir`. Omitted, the runtime is the
    /// managed `<state_dir>/runtime` the binary writes from its embedded copy
    /// (SPEC §3.3, ADR 0001); declared, mllm never writes to it.
    pub runtime_dir_declared: bool,
    pub ingress: Option<HostIngress>,
    pub profiles: Map<String, Value>,
    /// The period of the agent's engine load reports (`load_report_interval`).
    pub load_report_interval: Duration,
    /// `shutdown.drain_timeout`: how long a signalled role lets admitted work finish.
    pub drain_timeout: Duration,
    pub document: Value,
    /// Owner decision 2026-09-25: the generic overrides (`--set`,
    /// `MLLM_SET__…`) the host started with. A live reload of the document
    /// applies them again, so the reloaded document is compared with what the
    /// host runs, not with the file alone.
    pub overrides: crate::setting_overrides::SettingOverrides,
}
/// The shutdown drain bound when a role document names none (plan W11).
pub const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
/// The largest drain bound, so a typo cannot hold a restart for ever.
pub const MAX_DRAIN_TIMEOUT: Duration = Duration::from_secs(600);
/// SPEC §4.3 (owner decision P3), §15.3: the drain bound of a server, host or
/// standalone document, `shutdown.drain_timeout`. Omitted, it is 30 s; a value
/// outside 0 s to 600 s is refused, never read as the default.
pub fn drain_timeout(document: &Value) -> Result<Duration, ConfigError> {
    match document
        .get("shutdown")
        .and_then(|s| s.get("drain_timeout"))
    {
        None => Ok(DEFAULT_DRAIN_TIMEOUT),
        Some(value) => value
            .as_str()
            .and_then(|text| crate::effective::parse_duration_ms(text).ok())
            .and_then(|ms| u64::try_from(ms).ok())
            .map(Duration::from_millis)
            .filter(|bound| *bound <= MAX_DRAIN_TIMEOUT)
            .ok_or_else(|| {
                ConfigError::new(
                    ConfigErrorCode::UnsupportedCombination,
                    "shutdown.drain_timeout",
                    "must be a duration from 0s to 600s",
                )
            }),
    }
}
/// The load-report period when a host document names none.
pub const DEFAULT_LOAD_REPORT_INTERVAL: Duration = Duration::from_secs(1);
/// Bounds on `load_report_interval`: frequent enough for placement to see
/// load, never so frequent that scraping becomes the engine's workload.
pub const MIN_LOAD_REPORT_INTERVAL: Duration = Duration::from_millis(250);
pub const MAX_LOAD_REPORT_INTERVAL: Duration = Duration::from_secs(5);
#[derive(Debug, Clone)]
pub struct HostIngress {
    pub bind: SocketAddr,
    pub address: String,
}
/// SPEC §15: cleartext ingress requires an explicitly configured protected link.
pub fn private_ingress_endpoint(address: &str) -> Result<SocketAddr, ConfigError> {
    let socket: SocketAddr = address
        .strip_prefix("http://")
        .ok_or_else(|| invalid("ingress"))?
        .parse()
        .map_err(|_| invalid("ingress"))?;
    let protected = socket.ip().is_loopback()
        || matches!(socket.ip(), std::net::IpAddr::V4(ip) if ip.octets()[0] == 100 && (64..=127).contains(&ip.octets()[1]));
    if !protected || socket.port() == 0 {
        return Err(invalid("ingress"));
    }
    Ok(socket)
}
fn invalid(field: &str) -> ConfigError {
    ConfigError::new(
        ConfigErrorCode::UnsupportedCombination,
        field,
        "invalid or unsupported remote role setting",
    )
}
fn text<'a>(v: &'a Value, field: &str) -> Result<&'a str, ConfigError> {
    v.get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid(field))
}
fn path(v: &Value, field: &str) -> Result<PathBuf, ConfigError> {
    let s = text(v, field)?;
    if !s.starts_with('/')
        || s.len() > 4000
        || s.chars().any(char::is_control)
        || s[1..].split('/').any(|p| matches!(p, "" | "." | ".."))
    {
        return Err(invalid(field));
    }
    Ok(PathBuf::from(s))
}
fn listener(v: &Value, name: &str, auth: &str, local: bool) -> Result<SocketAddr, ConfigError> {
    let value = &v["listeners"][name];
    let addr: SocketAddr = text(value, "bind")?
        .parse()
        .map_err(|_| invalid("listeners"))?;
    if text(value, "authentication")? != auth
        || addr.port() == 0
        || addr.ip().is_multicast()
        || (local && !addr.ip().is_loopback())
    {
        return Err(invalid("listeners"));
    }
    Ok(addr)
}
/// No two listeners share a port on overlapping addresses (an unspecified
/// address overlaps every address).
fn distinct_listeners(all: &[SocketAddr]) -> Result<(), ConfigError> {
    for (i, a) in all.iter().enumerate() {
        if all[i + 1..].iter().any(|b| {
            a.port() == b.port()
                && (a.ip() == b.ip() || a.ip().is_unspecified() || b.ip().is_unspecified())
        }) {
            return Err(invalid("listeners"));
        }
    }
    Ok(())
}
pub fn endpoint_name(address: &str) -> Result<String, ConfigError> {
    let authority = address
        .strip_prefix("https://")
        .ok_or_else(|| invalid("enrollment"))?;
    let (name, port) = authority
        .rsplit_once(':')
        .ok_or_else(|| invalid("enrollment"))?;
    if name.is_empty()
        || name.len() > 253
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
        || port.parse::<u16>().ok().filter(|p| *p != 0).is_none()
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_unspecified() || ip.is_multicast())
    {
        return Err(invalid("enrollment"));
    }
    Ok(name.into())
}
impl ServerConfig {
    pub fn parse(source: &str) -> Result<Self, ConfigError> {
        let v = parse_strict(ConfigKind::Server, source)?;
        let state_dir = path(&v, "state_dir")?;
        let identity_dir = path(&v, "identity_dir")?;
        if identity_dir != state_dir.join("identity") {
            return Err(invalid("identity_dir"));
        }
        let management = listener(&v, "management", "token", true)?;
        // Design §9 (owner decision 5): the inference listener is not forced
        // to loopback; it keeps the API key unless the document states the
        // explicit `none` (design §9), and the router's allowlist (SPEC
        // §13.3); engines stay on loopback (ADR 0012).
        let inference_auth = crate::standalone::listener_auth(
            &v["listeners"]["inference"],
            "listeners.inference.authentication",
        )
        .map_err(|_| invalid("listeners"))?;
        let inference = listener(&v, "inference", inference_auth.as_str(), false)?;
        let bootstrap = listener(&v, "bootstrap", "server_tls", false)?;
        let control = listener(&v, "control", "mutual_tls", false)?;
        let listeners = v["listeners"]
            .as_object()
            .ok_or_else(|| invalid("listeners"))?;
        if listeners.len() != 4 {
            return Err(invalid("listeners"));
        }
        distinct_listeners(&[management, inference, bootstrap, control])?;
        let bootstrap_address = text(&v["enrollment"], "bootstrap_address")?.to_owned();
        let control_address = text(&v["enrollment"], "control_address")?.to_owned();
        let certificate_name = endpoint_name(&bootstrap_address)?;
        if endpoint_name(&control_address)? != certificate_name
            || bootstrap_address == control_address
        {
            return Err(invalid("enrollment"));
        }
        Ok(Self {
            state_dir,
            identity_dir,
            management,
            inference,
            inference_auth,
            bootstrap,
            control,
            bootstrap_address,
            control_address,
            certificate_name,
            drain_timeout: drain_timeout(&v)?,
            idle: idle_timeouts(&v)?,
            heartbeat: heartbeat_timeouts(&v)?,
            switch_drain_timeout: switch_drain_timeout(&v)?,
            timing_header: timing_header(&v)?,
        })
    }
    /// Design §9: `--listen` replaces the inference bind for one run. The
    /// address follows the document's rule
    /// ([`crate::standalone::inference_address`]) and must not collide with
    /// another listener.
    /// Final review I8-bis: the state directory this run uses when
    /// `--state-dir` or `MLLM_STATE_DIR` overrides the document's, with the
    /// identity directory it implies (`<state_dir>/identity`).
    pub fn with_state_dir(mut self, state_dir: PathBuf) -> Self {
        self.identity_dir = state_dir.join("identity");
        self.state_dir = state_dir;
        self
    }
    /// Final review I8: the management listener this run serves on
    /// (`--management-listen` or `MLLM_MANAGEMENT_ADDR`), a loopback address
    /// with a non-zero port that no other server listener shares.
    pub fn with_management(mut self, address: SocketAddr) -> Result<Self, ConfigError> {
        crate::standalone::management_address(&address.to_string())
            .ok_or_else(|| invalid("listeners"))?;
        distinct_listeners(&[address, self.inference, self.bootstrap, self.control])?;
        self.management = address;
        Ok(self)
    }
    pub fn with_inference(mut self, address: SocketAddr) -> Result<Self, ConfigError> {
        crate::standalone::inference_address(&address.to_string())
            .ok_or_else(|| invalid("listeners"))?;
        distinct_listeners(&[self.management, address, self.bootstrap, self.control])?;
        self.inference = address;
        Ok(self)
    }
    pub fn template(root: &Path) -> String {
        // JSON is a strict YAML subset and quotes every generated path safely.
        serde_json::to_string_pretty(&json!({
            "schema_version":1,"kind":"server","name":"mllm-server",
            "state_dir":root,"identity_dir":root.join("identity"),
            "listeners":{
                "management":{"bind":"127.0.0.1:7443","authentication":"token"},
                // Design §9 (owner decision 5): as standalone.
                "inference":{"bind":crate::standalone::DEFAULT_INFERENCE_BIND,"authentication":"api_key"},
                "bootstrap":{"bind":"127.0.0.1:7444","authentication":"server_tls"},
                "control":{"bind":"127.0.0.1:7445","authentication":"mutual_tls"}},
            "enrollment":{"bootstrap_address":"https://127.0.0.1:7444","control_address":"https://127.0.0.1:7445"}
        })).expect("serializable role template")
    }
}
impl HostConfig {
    /// Final review I8-bis: as [`ServerConfig::with_state_dir`]. A runtime
    /// directory the document does not name follows the state directory, and
    /// the held document states the directories this run uses.
    pub fn with_state_dir(mut self, state_dir: PathBuf) -> Self {
        self.identity_dir = state_dir.join("identity");
        if !self.runtime_dir_declared {
            self.runtime_dir = state_dir.join("runtime");
        }
        self.document["state_dir"] = Value::String(state_dir.to_string_lossy().into_owned());
        self.document["identity_dir"] =
            Value::String(self.identity_dir.to_string_lossy().into_owned());
        self.state_dir = state_dir;
        self
    }
    pub fn parse(source: &str) -> Result<Self, ConfigError> {
        let document = parse_strict(ConfigKind::Host, source)?;
        let state_dir = path(&document, "state_dir")?;
        let identity_dir = path(&document, "identity_dir")?;
        if identity_dir != state_dir.join("identity") {
            return Err(invalid("identity_dir"));
        }
        let runtime_dir_declared = document.get("runtime_dir").is_some();
        let runtime_dir = if runtime_dir_declared {
            path(&document, "runtime_dir")?
        } else {
            state_dir.join("runtime")
        };
        let ingress = document
            .get("ingress")
            .map(|v| {
                if text(v, "transport")? != "trusted_private_link" {
                    return Err(invalid("ingress"));
                }
                let address = text(v, "address")?.to_owned();
                let bind = private_ingress_endpoint(&address)?;
                if text(v, "bind")?.parse::<SocketAddr>().ok() != Some(bind) {
                    return Err(invalid("ingress"));
                }
                Ok(HostIngress { bind, address })
            })
            .transpose()?;
        let name = text(&document, "name")?.to_owned();
        if name.len() > 128
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        {
            return Err(invalid("name"));
        }
        // Owner decision 2026-09-25: the model store may be omitted (the role
        // fills `~/models`, `MLLM_MODELS_ROOT` or `--models-root` through
        // `crate::model_settings` before publishing); stated, it is a path.
        if document.get("model_store").is_some() {
            path(&document["model_store"], "path")?;
        }
        let profiles = match document.get("runtime_profiles") {
            None => Map::new(),
            Some(v) => v
                .as_object()
                .cloned()
                .ok_or_else(|| invalid("runtime_profiles"))?,
        };
        let load_report_interval = match document.get("load_report_interval") {
            None => DEFAULT_LOAD_REPORT_INTERVAL,
            Some(value) => value
                .as_str()
                .and_then(|text| crate::effective::parse_duration_ms(text).ok())
                .and_then(|ms| u64::try_from(ms).ok())
                .map(Duration::from_millis)
                .filter(|period| {
                    (MIN_LOAD_REPORT_INTERVAL..=MAX_LOAD_REPORT_INTERVAL).contains(period)
                })
                .ok_or_else(|| invalid("load_report_interval"))?,
        };
        Ok(Self {
            name,
            state_dir,
            identity_dir,
            runtime_dir,
            runtime_dir_declared,
            ingress,
            profiles,
            load_report_interval,
            drain_timeout: drain_timeout(&document)?,
            document,
            overrides: crate::setting_overrides::SettingOverrides::none(ConfigKind::Host),
        })
    }
    /// Owner decision 2026-09-25: state the host's models directory and
    /// model-source policy in its document, resolved by the shared rule
    /// (`crate::model_settings`, flag > environment > document > default:
    /// `~/models` from `home`, downloads under `<model_store>/sources`), so the
    /// document the host publishes is the one it enforces.
    pub fn with_models(
        mut self,
        flags: &crate::model_settings::ModelOverrides,
        env: &crate::model_settings::ModelOverrides,
        home: Option<&Path>,
    ) -> Result<Self, ConfigError> {
        crate::model_settings::apply(
            &mut self.document,
            flags,
            env,
            crate::model_settings::default_models_root(home).as_deref(),
        )?;
        Ok(self)
    }
    /// Owner rule 2026-09-25 (every setting three ways, standalone is a
    /// server plus one host): apply the host's engine settings, resolved
    /// flag > environment > document > default
    /// (`crate::engine_settings`), to its document before it is published:
    /// the runtime directory, the engines' port range and the `local_engine`
    /// executables as the `local` runtime profiles. `probe` reads an
    /// executable's version when no fingerprint is stated.
    pub fn with_engines(
        self,
        flags: &crate::engine_settings::EngineOverrides,
        env: &crate::engine_settings::EngineOverrides,
        probe: &dyn Fn(&Path) -> Result<String, String>,
    ) -> Result<Self, ConfigError> {
        let stated = crate::engine_settings::EngineOverrides::from_document(&self.document)?;
        let settings = crate::engine_settings::resolve(flags, env, &stated);
        let overrides = self.overrides;
        let mut document = self.document;
        crate::engine_settings::apply_to_host(&mut document, &settings, probe)?;
        let mut config = Self::parse(&document.to_string())?;
        config.overrides = overrides;
        Ok(config)
    }
    pub fn template(root: &Path) -> String {
        serde_json::to_string_pretty(&json!({
            "schema_version":1,"kind":"host","name":"mllm-host",
            "state_dir":root,"identity_dir":root.join("identity"),
            "model_store":{"path":root.join("models")},"runtime_profiles":{}
        }))
        .expect("serializable role template")
    }
    /// ADR 0018 §2: the host document at `path` with the engines registered
    /// beside it (`engines.yaml`) merged in. The file itself is never
    /// rewritten; a profile name declared in both is refused.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        Self::load_with_engines(path, &crate::registration::engines_beside(path))
    }
    /// As [`HostConfig::load`], with the engines file named explicitly: the
    /// role resolves it by the same rule as `mllm engine` (ADR 0018 §2), which
    /// for a host started without a named document is
    /// `<config home>/mllm/engines.yaml`, not the file beside it.
    pub fn load_with_engines(path: &Path, engines: &Path) -> Result<Self, ConfigError> {
        Self::load_with_overrides(
            path,
            engines,
            &crate::setting_overrides::SettingOverrides::none(ConfigKind::Host),
        )
    }
    /// As [`HostConfig::load_with_engines`], with this run's generic
    /// overrides (owner decision 2026-09-25: `--set` > `MLLM_SET__…` > YAML)
    /// applied to the document before it is validated, exactly as if the
    /// file stated them.
    pub fn load_with_overrides(
        path: &Path,
        engines: &Path,
        overrides: &crate::setting_overrides::SettingOverrides,
    ) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            ConfigError::new(
                ConfigErrorCode::Io,
                path.display().to_string(),
                e.to_string(),
            )
        })?;
        let document = crate::parse_document(&text)?;
        let mut document = overrides.apply_and_validate(document)?;
        let engines = crate::registration::EnginesFile::load(engines)?;
        crate::registration::merge_into_host(&mut document, &engines)?;
        let mut config = Self::parse(&document.to_string()).map_err(|e| overrides.annotate(e))?;
        config.overrides = overrides.clone();
        Ok(config)
    }
}
