//! vLLM development-control exposure, derived for status and inspect views.
//!
//! SPEC §9.1, T21, ADR 0012 and owner decision P4: deep parking is on by
//! default, and for vLLM it runs through sleep mode, which needs vLLM's
//! development mode. Status must mark every deployment and host installation
//! whose launch enables that mode, with the mitigations that stay mandatory.
//!
//! The mark is derived from the effective configuration, never declared: no
//! configuration field can set or clear it. ADR 0014 §4 makes vLLM sleep mode
//! (and with it development mode) a derived, reserved setting: it is on exactly
//! when the installation is vLLM, the host leaves deep parking enabled and the
//! deployment's residency parks. A deployment is marked from its effective
//! `engine_config.enable_sleep_mode`, the value the launch renders from; a host
//! installation is marked when parking deployments launched on it would run in
//! development mode.
//!
//! Owner decision 2026-09-22: SGLang serves `/metrics` without its API key
//! (SGLang exempts the route), on its loopback listener and read-only. That is
//! accepted, and every SGLang deployment, instance and host installation is
//! marked with it as `unauthenticated_local_surfaces`, derived from the engine
//! family exactly like the vLLM mark.
//!
//! This is a read-only projection. It is not a production-safety claim and it
//! grants no authority.

use capyctl_config::effective::{DeepPark, DeepParkSource, EffectiveDeployment, Engine, Residency};
use capyctl_domain::launch::LaunchSettings;
use serde::Serialize;
use serde_json::Value;

/// The vLLM development routes that the key-guard middleware closes
/// (`runtime/capyctl_vllm_guard.py`).
pub const SURFACE: &[&str] = &["/sleep", "/wake_up", "/is_sleeping", "/collective_rpc"];

/// ADR 0012 decision 5: the protections that stay mandatory whenever
/// development mode is on. Each is enforced by construction of the launch, not
/// by configuration.
pub const MITIGATIONS: &[&str] = &[
    "loopback_engine_listener",
    "per_launch_engine_key",
    "engine_key_guard_middleware",
    "no_ingress_or_router_path",
];

/// A route an engine serves without its credential. Only on the engine's
/// loopback listener, never through ingress or the router (SPEC §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct UnauthenticatedSurfaces {
    pub surface: &'static [&'static str],
    pub listener: &'static str,
    pub access: &'static str,
}

/// T21, owner decision 2026-09-22: SGLang's unauthenticated `/metrics`.
pub const UNAUTHENTICATED_LOCAL_SURFACES: UnauthenticatedSurfaces = UnauthenticatedSurfaces {
    surface: &["/metrics"],
    listener: "loopback",
    access: "read_only",
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExposureState {
    /// The launch enables vLLM development mode.
    Exposed,
    /// The launch does not enable it.
    NotExposed,
    /// The configuration could not be read, so no claim is made either way.
    Unknown,
}

/// The status mark for one deployment or host installation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DevelopmentControls {
    pub state: ExposureState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine: Option<Engine>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deep_park: Option<DeepPark>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deep_park_source: Option<DeepParkSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_sleep_mode: Option<bool>,
    /// Deployments only; a host installation has no residency of its own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub residency: Option<Residency>,
    /// Present only when exposed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub surface: Option<&'static [&'static str]>,
    /// Present only when exposed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mitigations: Option<&'static [&'static str]>,
    /// Present only when exposed, and always false (SPEC §9.1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub production_safe: Option<bool>,
    /// Host installations only: which launches the mark applies to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applies_to: Option<&'static str>,
    /// SGLang only: the routes it serves on loopback without its key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unauthenticated_local_surfaces: Option<UnauthenticatedSurfaces>,
}

impl DevelopmentControls {
    pub fn unknown() -> Self {
        Self {
            state: ExposureState::Unknown,
            engine: None,
            deep_park: None,
            deep_park_source: None,
            enable_sleep_mode: None,
            residency: None,
            surface: None,
            mitigations: None,
            production_safe: None,
            applies_to: None,
            unauthenticated_local_surfaces: None,
        }
    }

    pub fn is_exposed(&self) -> bool {
        self.state == ExposureState::Exposed
    }
}

fn classify(
    engine: Engine,
    deep_park: DeepPark,
    source: DeepParkSource,
    enable_sleep_mode: Option<bool>,
    residency: Option<Residency>,
) -> DevelopmentControls {
    let mut controls = DevelopmentControls {
        state: ExposureState::NotExposed,
        engine: Some(engine),
        deep_park: Some(deep_park),
        deep_park_source: Some(source),
        enable_sleep_mode,
        residency,
        ..DevelopmentControls::unknown()
    };
    if engine == Engine::Sglang {
        controls.unauthenticated_local_surfaces = Some(UNAUTHENTICATED_LOCAL_SURFACES);
    }
    if engine != Engine::Vllm {
        return controls;
    }
    // SPEC §9.1: sleep mode is the only input to vLLM's
    // `VLLM_SERVER_DEV_MODE`. A vLLM configuration always carries it; without
    // it no claim is made. Deep parking is checked too, so a stored revision
    // that claims sleep mode on an opted-out host is still marked by what it
    // renders, never hidden.
    let Some(sleep) = enable_sleep_mode else {
        return DevelopmentControls::unknown();
    };
    if sleep && deep_park.is_enabled() {
        controls.state = ExposureState::Exposed;
        controls.surface = Some(SURFACE);
        controls.mitigations = Some(MITIGATIONS);
        controls.production_safe = Some(false);
    }
    controls
}

/// The mark for a resolved effective deployment.
pub fn for_effective(effective: &EffectiveDeployment) -> DevelopmentControls {
    let profile = &effective.profile;
    let sleep = match &effective.engine_config {
        LaunchSettings::Vllm(settings) => Some(settings.enable_sleep_mode),
        LaunchSettings::Sglang(_) => None,
    };
    classify(
        profile.engine,
        profile.security.deep_park,
        profile.security.deep_park_source,
        sleep,
        Some(effective.residency),
    )
}

fn scalar<T: serde::de::DeserializeOwned>(value: Option<String>) -> Option<T> {
    serde_json::from_value(Value::String(value?)).ok()
}

/// The mark for scalars extracted from a stored effective revision. Anything
/// absent or unrecognized yields `unknown`; nothing stored is reflected.
pub(crate) fn from_stored(
    engine: Option<String>,
    deep_park: Option<String>,
    deep_park_source: Option<String>,
    enable_sleep_mode: Option<i64>,
    residency: Option<String>,
) -> DevelopmentControls {
    let (Some(engine), Some(deep_park)) = (scalar::<Engine>(engine), scalar::<DeepPark>(deep_park))
    else {
        return DevelopmentControls::unknown();
    };
    // SPEC §7 / T14: only a defaulted value carries a provenance marker.
    let source = match deep_park_source.as_deref() {
        None => DeepParkSource::HostPolicy,
        Some("default") => DeepParkSource::Default,
        Some(_) => return DevelopmentControls::unknown(),
    };
    let sleep = match enable_sleep_mode {
        None => None,
        Some(0) => Some(false),
        Some(1) => Some(true),
        Some(_) => return DevelopmentControls::unknown(),
    };
    let Some(residency) = scalar::<Residency>(residency) else {
        return DevelopmentControls::unknown();
    };
    classify(engine, deep_park, source, sleep, Some(residency))
}

/// The mark for one runtime profile (engine installation) of a host document.
/// SPEC §9.1 / ADR 0012: an omitted `security.deep_park` resolves enabled.
/// ADR 0014 §4: sleep mode is derived per deployment, so a vLLM installation
/// with deep parking on is marked for the parking deployments launched on it.
pub fn for_host_profile(profile: &Value) -> DevelopmentControls {
    let Some(engine) = profile["engine"]
        .as_str()
        .and_then(|engine| scalar::<Engine>(Some(engine.to_owned())))
    else {
        return DevelopmentControls::unknown();
    };
    let (deep_park, source) = match profile["security"].get("deep_park") {
        None => (DeepPark::default(), DeepParkSource::Default),
        Some(Value::String(value)) => match scalar::<DeepPark>(Some(value.clone())) {
            Some(deep_park) => (deep_park, DeepParkSource::HostPolicy),
            None => return DevelopmentControls::unknown(),
        },
        Some(_) => return DevelopmentControls::unknown(),
    };
    let mut controls = classify(engine, deep_park, source, Some(true), None);
    // Sleep mode is not an installation setting; it is not reported as one.
    controls.enable_sleep_mode = None;
    if controls.is_exposed() {
        controls.applies_to = Some("parking_deployments");
    }
    controls
}
