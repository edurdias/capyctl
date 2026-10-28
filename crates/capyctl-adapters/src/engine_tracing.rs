//! ADR 0037 §§7, 11: engine tracing, the engine's own spans exported to the
//! host's OpenTelemetry collector on loopback.
//!
//! The host's `observability.tracing.engines` (`off` by default) is read when
//! the role starts and holds for every launch from then on, like
//! `engine_telemetry`. Each vLLM or SGLang single launch decides once, when it
//! is launched, whether its engine traces ([`decide`]); the decision is frozen
//! with the launch: the rendered options, the ingress's `EngineAccess`, the
//! router's lane and status all read that one answer.
//!
//! What each engine expects, read at the pinned tags (no engine was run):
//! - vLLM 0.32.0 `vllm/tracing/otel.py`
//!   (<https://github.com/vllm-project/vllm/blob/v0.32.0/vllm/tracing/otel.py>):
//!   `--otlp-traces-endpoint` turns tracing on; `get_span_exporter` builds
//!   `OTLPSpanExporter(endpoint=endpoint)` from
//!   `opentelemetry.exporter.otlp.proto.http.trace_exporter` when
//!   `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL` is `http/protobuf` (gRPC with
//!   `insecure=True` otherwise). `Resource.create` merges
//!   `OTEL_SERVICE_NAME` and `OTEL_RESOURCE_ATTRIBUTES` from the environment.
//! - SGLang 0.5.22 `python/sglang/srt/observability/trace.py`
//!   (<https://github.com/sgl-project/sglang/blob/v0.5.22/python/sglang/srt/observability/trace.py>):
//!   `process_tracing_init` hands `--otlp-traces-endpoint` to
//!   `get_otlp_span_exporter`, which builds the same
//!   `HTTPSpanExporter(endpoint=endpoint)` under `http/protobuf`; the
//!   service name is `--otlp-service-name`. SGLang's help text says
//!   `<ip>:<port>`, which is the gRPC form only.
//! - The OTLP HTTP exporter
//!   (<https://github.com/open-telemetry/opentelemetry-python/blob/main/exporter/opentelemetry-exporter-otlp-proto-http/src/opentelemetry/exporter/otlp/proto/http/trace_exporter/__init__.py>,
//!   `self._endpoint = endpoint or _resolve_endpoint(...)`) posts to an
//!   explicit endpoint exactly as given: `/v1/traces` is appended only to the
//!   generic `OTEL_EXPORTER_OTLP_ENDPOINT`. So both engines need the full
//!   traces URL, scheme and path included, which is what the host's
//!   `observability.tracing.endpoint` holds (§12:
//!   `http://127.0.0.1:4318/v1/traces`); it is rendered verbatim.
//! - Both engines import the gRPC exporter in the same `try` block as the
//!   HTTP one, so the gRPC exporter must be importable too (the launch-time
//!   check, `capyctl_agent::tracing_install`).

use std::collections::BTreeMap;
use std::sync::RwLock;

use capyctl_config::decisions::EngineVersion;
use capyctl_config::engine_policy::Engine;

/// ADR 0037 §7: the first vLLM release with the options CapyCTL renders.
pub const VLLM_FLOOR: EngineVersion = EngineVersion::new(0, 31, 0);
/// ADR 0037 §7: the first SGLang release with the options CapyCTL renders.
pub const SGLANG_FLOOR: EngineVersion = EngineVersion::new(0, 5, 22);

/// The protocol variable both engines read to pick the HTTP exporter.
pub const PROTOCOL_ENV: &str = "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL";
/// OTLP over HTTP with protobuf encoding (ADR 0037 §8: no gRPC).
pub const HTTP_PROTOBUF: &str = "http/protobuf";
/// vLLM's service name variable (the OpenTelemetry SDK's resource detector).
pub const SERVICE_NAME_ENV: &str = "OTEL_SERVICE_NAME";
/// vLLM's resource attributes variable (the SDK's resource detector).
pub const RESOURCE_ATTRIBUTES_ENV: &str = "OTEL_RESOURCE_ATTRIBUTES";
/// SGLang's span level, 0–3 (default 3).
pub const SGLANG_TRACE_LEVEL_ENV: &str = "SGLANG_TRACE_LEVEL";
/// ADR 0037 §7: important slices only [the level is to be confirmed by OT8].
pub const SGLANG_TRACE_LEVEL: &str = "1";
/// vLLM's option naming the collector.
pub const VLLM_ENDPOINT_FLAG: &str = "--otlp-traces-endpoint";
/// SGLang's service name, rendered as `--otlp-service-name`.
pub const SGLANG_SERVICE_NAME: &str = "sglang";
/// vLLM's service name, rendered as `OTEL_SERVICE_NAME`.
pub const VLLM_SERVICE_NAME: &str = "vllm";

/// The environment names a traced vLLM launch renders; the launch's closed
/// environment keeps exactly these beside its allowlist.
pub const VLLM_ENV: &[&str] = &[PROTOCOL_ENV, SERVICE_NAME_ENV, RESOURCE_ATTRIBUTES_ENV];
/// The environment names a traced SGLang launch renders.
pub const SGLANG_ENV: &[&str] = &[PROTOCOL_ENV, SGLANG_TRACE_LEVEL_ENV];

/// ADR 0037 §7: why a launch runs without engine tracing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EngineTracingOff {
    /// The host's `engines` is `off`, or the host names no endpoint.
    HostOff,
    /// Not vLLM 0.31.0 or newer, nor SGLang 0.5.22 or newer.
    EngineUnsupported,
    /// The engine's environment lacks the OpenTelemetry SDK or exporters.
    NotInstalled,
    /// A multi-node group (ADR 0028) until a live row covers them.
    Group,
    /// §11: the deployment's or the host's `retention: zero`.
    RetentionZero,
}

impl EngineTracingOff {
    pub const ALL: [Self; 5] = [
        Self::HostOff,
        Self::EngineUnsupported,
        Self::NotInstalled,
        Self::Group,
        Self::RetentionZero,
    ];

    /// The closed reason code (`engine_tracing_off:<code>`).
    pub fn code(self) -> &'static str {
        match self {
            Self::HostOff => "host_off",
            Self::EngineUnsupported => "engine_unsupported",
            Self::NotInstalled => "not_installed",
            Self::Group => "group",
            Self::RetentionZero => "retention_zero",
        }
    }

    pub fn parse(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|reason| reason.code() == code)
    }
}

/// ADR 0037 §7: one launch's decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineTracing {
    /// The engine exports its spans to `endpoint` (the host's, on loopback).
    On {
        endpoint: String,
    },
    Off(EngineTracingOff),
}

impl EngineTracing {
    pub fn is_on(&self) -> bool {
        matches!(self, Self::On { .. })
    }

    /// The decision as recorded and reported, without the endpoint.
    pub fn outcome(&self) -> TracingOutcome {
        match self {
            Self::On { .. } => TracingOutcome::On,
            Self::Off(reason) => TracingOutcome::Off(*reason),
        }
    }

    /// The endpoint the engine exports to, when on.
    pub fn endpoint(&self) -> Option<&str> {
        match self {
            Self::On { endpoint } => Some(endpoint),
            Self::Off(_) => None,
        }
    }
}

/// A launch's decision as the host reports it, the journals keep it and
/// status shows it: `on`, or off with its reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TracingOutcome {
    On,
    Off(EngineTracingOff),
}

impl TracingOutcome {
    /// `on`, or the off reason's code: the wire and journal form.
    pub fn code(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Off(reason) => reason.code(),
        }
    }

    pub fn parse(code: &str) -> Option<Self> {
        match code {
            "on" => Some(Self::On),
            other => EngineTracingOff::parse(other).map(Self::Off),
        }
    }

    pub fn is_on(self) -> bool {
        self == Self::On
    }
}

/// ADR 0037 §7: one launch's reported decision, shared between whatever
/// learns it (an embedded launch's spec, a remote host's result) and the
/// coordinator's driver of the launch, which records it before Ready.
#[derive(Debug, Default)]
pub struct TracingReport(std::sync::Mutex<Option<TracingOutcome>>);

impl TracingReport {
    pub fn of(outcome: TracingOutcome) -> Self {
        Self(std::sync::Mutex::new(Some(outcome)))
    }

    pub fn record(&self, outcome: TracingOutcome) {
        *self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(outcome);
    }

    /// What was reported; `None` before anything was (an older host).
    pub fn get(&self) -> Option<TracingOutcome> {
        *self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The host's engine tracing setting, as the role read it at start.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HostEngineTracing {
    /// `observability.tracing.engines: on`.
    pub engines_on: bool,
    /// The host's loopback endpoint (the standalone server block's for a
    /// standalone role); `None` when the role names none.
    pub endpoint: Option<String>,
}

/// This role's setting: off until a role says otherwise.
static HOST: RwLock<HostEngineTracing> = RwLock::new(HostEngineTracing {
    engines_on: false,
    endpoint: None,
});

/// Set by the host or standalone role at start, from its document.
pub fn set_host_setting(setting: HostEngineTracing) {
    *HOST
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = setting;
}

/// The host's setting for this role.
pub fn host_setting() -> HostEngineTracing {
    HOST.read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// Whether `engine` at `engine_version` (the profile's registered build) has
/// the tracing options CapyCTL renders. A pre-release of a floor is older.
pub fn engine_supported(engine: Engine, engine_version: &str) -> bool {
    let floor = match engine {
        Engine::Vllm => VLLM_FLOOR,
        Engine::Sglang => SGLANG_FLOOR,
        Engine::Llamacpp | Engine::Tensorfold | Engine::Custom => return false,
    };
    EngineVersion::parse(engine_version.trim()).is_some_and(|version| version.at_least(&floor))
}

/// ADR 0037 §§7, 11: one launch's decision.
///
/// Precedence, first applicable wins: `host_off`, `engine_unsupported`,
/// `group`, `retention_zero`, `not_installed`. From the widest scope to the
/// narrowest, so the reason shown is the one whose change could turn tracing
/// on: a host with `engines: off` says so for every launch whatever its
/// engine; an engine without the options says so before anything about the
/// launch; a group or zero retention are the launch's own policy, true of any
/// environment; and a missing package last, because only then does the
/// environment's content decide. That order also lets a caller read the
/// environment only for a launch that would otherwise trace
/// (`installed: true`, then the check when the answer is on).
pub fn decide(
    engine: Engine,
    engine_version: &str,
    host: &HostEngineTracing,
    retention_zero: bool,
    group: bool,
    installed: bool,
) -> EngineTracing {
    let endpoint = match (&host.endpoint, host.engines_on) {
        (Some(endpoint), true) if !endpoint.is_empty() => endpoint,
        _ => return EngineTracing::Off(EngineTracingOff::HostOff),
    };
    if !engine_supported(engine, engine_version) {
        return EngineTracing::Off(EngineTracingOff::EngineUnsupported);
    }
    if group {
        return EngineTracing::Off(EngineTracingOff::Group);
    }
    if retention_zero {
        return EngineTracing::Off(EngineTracingOff::RetentionZero);
    }
    if !installed {
        return EngineTracing::Off(EngineTracingOff::NotInstalled);
    }
    EngineTracing::On {
        endpoint: endpoint.clone(),
    }
}

/// What a traced vLLM launch renders (`PlanInputVllm::engine_tracing`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VllmTrace {
    pub endpoint: String,
    /// `OTEL_RESOURCE_ATTRIBUTES`, from [`resource_attributes`].
    pub resource_attributes: String,
}

impl VllmTrace {
    /// The rendering of `tracing` for instance `instance` of deployment
    /// `deployment`; `None` when off, which renders nothing at all.
    pub fn for_launch(tracing: &EngineTracing, deployment: &str, instance: u32) -> Option<Self> {
        tracing.endpoint().map(|endpoint| Self {
            endpoint: endpoint.to_owned(),
            resource_attributes: resource_attributes(deployment, instance),
        })
    }

    /// ADR 0037 §7: `--otlp-traces-endpoint <endpoint>` (never
    /// `--collect-detailed-traces`) and the three variables.
    pub fn render(&self, argv: &mut Vec<String>, env: &mut BTreeMap<String, String>) {
        argv.push(VLLM_ENDPOINT_FLAG.into());
        argv.push(self.endpoint.clone());
        env.insert(PROTOCOL_ENV.into(), HTTP_PROTOBUF.into());
        env.insert(SERVICE_NAME_ENV.into(), VLLM_SERVICE_NAME.into());
        env.insert(
            RESOURCE_ATTRIBUTES_ENV.into(),
            self.resource_attributes.clone(),
        );
    }
}

/// ADR 0037 §7: the variables of a traced SGLang launch (its options are
/// rendered by the protected entry from the descriptor's `trace_endpoint`).
/// `SGLANG_TRACE_ASYNC` is never set, so no exporter process joins the
/// launch's group (ADR 0027).
pub fn render_sglang_env(env: &mut BTreeMap<String, String>) {
    env.insert(PROTOCOL_ENV.into(), HTTP_PROTOBUF.into());
    env.insert(SGLANG_TRACE_LEVEL_ENV.into(), SGLANG_TRACE_LEVEL.into());
}

/// `capyctl.deployment=<name>,capyctl.instance=<index>`, each value
/// percent-encoded as the OpenTelemetry resource SDK specification requires
/// of `OTEL_RESOURCE_ATTRIBUTES` (W3C Baggage octets; the Python SDK
/// unquotes them). Every byte outside `A-Z a-z 0-9 - . _ ~` is encoded,
/// which covers `,`, `=`, `%`, spaces and anything non-ASCII.
pub fn resource_attributes(deployment: &str, instance: u32) -> String {
    format!(
        "capyctl.deployment={},capyctl.instance={instance}",
        percent_encode(deployment)
    )
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on() -> HostEngineTracing {
        HostEngineTracing {
            engines_on: true,
            endpoint: Some("http://127.0.0.1:4318/v1/traces".into()),
        }
    }

    fn traced() -> EngineTracing {
        EngineTracing::On {
            endpoint: "http://127.0.0.1:4318/v1/traces".into(),
        }
    }

    // T48 (ADR 0037 §7): the supported engines from their floors, every
    // other engine never.
    #[test]
    fn engine_tracing_needs_vllm_0_31_or_sglang_0_5_22() {
        for (engine, version, supported) in [
            (Engine::Vllm, "0.31.0", true),
            (Engine::Vllm, "0.32.0", true),
            (Engine::Vllm, "0.30.0", false),
            (Engine::Vllm, "0.30.2", false),
            (Engine::Vllm, "0.31.0rc1", false),
            (Engine::Sglang, "0.5.22", true),
            (Engine::Sglang, "0.5.23", true),
            (Engine::Sglang, "0.5.21", false),
            (Engine::Sglang, "not-a-version", false),
            (Engine::Llamacpp, "0.7.0", false),
            (Engine::Tensorfold, "0.6.6", false),
            (Engine::Custom, "1.0.0", false),
        ] {
            assert_eq!(
                engine_supported(engine, version),
                supported,
                "{engine:?} {version}"
            );
        }
    }

    // T48 (ADR 0037 §§7, 11): each reason, and the precedence between them.
    #[test]
    fn the_decision_names_each_reason_first_applicable_wins() {
        let decide = |engine, version, host: &HostEngineTracing, zero, group, installed| {
            decide(engine, version, host, zero, group, installed)
        };
        let off = |reason| EngineTracing::Off(reason);
        assert_eq!(
            decide(Engine::Vllm, "0.32.0", &on(), false, false, true),
            traced()
        );
        assert_eq!(
            decide(Engine::Sglang, "0.5.22", &on(), false, false, true),
            traced()
        );
        // `engines: off`, or on with no endpoint.
        let host_off = HostEngineTracing {
            engines_on: false,
            ..on()
        };
        let no_endpoint = HostEngineTracing {
            engines_on: true,
            endpoint: None,
        };
        for host in [&host_off, &no_endpoint, &HostEngineTracing::default()] {
            assert_eq!(
                decide(Engine::Vllm, "0.32.0", host, true, true, false),
                off(EngineTracingOff::HostOff)
            );
        }
        for (engine, version) in [
            (Engine::Llamacpp, "0.7.0"),
            (Engine::Tensorfold, "0.6.6"),
            (Engine::Custom, "1.0.0"),
            (Engine::Vllm, "0.30.1"),
            (Engine::Sglang, "0.5.21"),
        ] {
            assert_eq!(
                decide(engine, version, &on(), true, true, false),
                off(EngineTracingOff::EngineUnsupported),
                "{engine:?} {version}"
            );
        }
        assert_eq!(
            decide(Engine::Vllm, "0.32.0", &on(), true, true, false),
            off(EngineTracingOff::Group)
        );
        assert_eq!(
            decide(Engine::Sglang, "0.5.22", &on(), true, false, false),
            off(EngineTracingOff::RetentionZero)
        );
        assert_eq!(
            decide(Engine::Vllm, "0.32.0", &on(), false, false, false),
            off(EngineTracingOff::NotInstalled)
        );
    }

    // T48: the closed codes, round-tripped as the wire and journals carry them.
    #[test]
    fn outcomes_round_trip_through_their_codes() {
        assert_eq!(
            EngineTracingOff::ALL.map(EngineTracingOff::code),
            [
                "host_off",
                "engine_unsupported",
                "not_installed",
                "group",
                "retention_zero"
            ]
        );
        for outcome in EngineTracingOff::ALL
            .map(TracingOutcome::Off)
            .into_iter()
            .chain([TracingOutcome::On])
        {
            assert_eq!(TracingOutcome::parse(outcome.code()), Some(outcome));
        }
        assert_eq!(TracingOutcome::parse("off"), None);
        assert_eq!(TracingOutcome::parse(""), None);
        assert_eq!(traced().outcome(), TracingOutcome::On);
    }

    // T48 (ADR 0037 §7): vLLM's rendering exactly; off renders nothing.
    #[test]
    fn vllm_renders_the_endpoint_protocol_service_and_resource() {
        let trace = VllmTrace::for_launch(&traced(), "chat a,b=c%", 2).unwrap();
        let (mut argv, mut env) = (Vec::new(), BTreeMap::new());
        trace.render(&mut argv, &mut env);
        assert_eq!(
            argv,
            ["--otlp-traces-endpoint", "http://127.0.0.1:4318/v1/traces"]
        );
        assert!(!argv.iter().any(|a| a.contains("collect-detailed-traces")));
        assert_eq!(
            env,
            BTreeMap::from([
                (
                    "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL".to_owned(),
                    "http/protobuf".to_owned()
                ),
                (
                    "OTEL_RESOURCE_ATTRIBUTES".to_owned(),
                    "capyctl.deployment=chat%20a%2Cb%3Dc%25,capyctl.instance=2".to_owned()
                ),
                ("OTEL_SERVICE_NAME".to_owned(), "vllm".to_owned()),
            ])
        );
        for reason in EngineTracingOff::ALL {
            assert_eq!(
                VllmTrace::for_launch(&EngineTracing::Off(reason), "chat", 0),
                None
            );
        }
    }

    // T48 (ADR 0037 §7): SGLang's variables; never `SGLANG_TRACE_ASYNC`.
    #[test]
    fn sglang_renders_the_protocol_and_level_only() {
        let mut env = BTreeMap::new();
        render_sglang_env(&mut env);
        assert_eq!(
            env,
            BTreeMap::from([
                (
                    "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL".to_owned(),
                    "http/protobuf".to_owned()
                ),
                ("SGLANG_TRACE_LEVEL".to_owned(), "1".to_owned()),
            ])
        );
        assert!(!env.contains_key("SGLANG_TRACE_ASYNC"));
    }

    #[test]
    fn the_host_setting_is_off_until_a_role_sets_it() {
        // The one test touching the global: read, set, restore.
        let before = host_setting();
        set_host_setting(on());
        assert_eq!(host_setting(), on());
        set_host_setting(before);
    }
}
