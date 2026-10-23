//! SPEC §4.2: inventory reads report enrollment and connectivity separately.
use crate::{error, ManagementCredentials};
use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use capyctl_controller::{agent_sessions::AgentSessions, ownership::SharedCoordinatorState};
use std::sync::Arc;
struct HostState {
    credentials: ManagementCredentials,
    owner: SharedCoordinatorState,
    sessions: Arc<AgentSessions>,
    reads: Arc<tokio::sync::Semaphore>,
}
pub fn hosts_router(
    credentials: ManagementCredentials,
    owner: SharedCoordinatorState,
    sessions: Arc<AgentSessions>,
) -> Router {
    let state = Arc::new(HostState {
        credentials,
        owner,
        sessions,
        reads: Arc::new(tokio::sync::Semaphore::new(2)),
    });
    Router::new()
        .route("/management/v1/hosts", get(hosts))
        .route("/management/v1/engines", get(engines))
        .layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .with_state(state)
}
async fn authenticate(
    State(state): State<Arc<HostState>>,
    request: Request,
    next: Next,
) -> Response {
    guard(&state.credentials, request, next).await
}
/// The authentication and response headers every inventory read shares, on a
/// server and on a standalone role alike.
async fn guard(credentials: &ManagementCredentials, request: Request, next: Next) -> Response {
    let mut response = if !credentials.accepts(&request) {
        error(StatusCode::UNAUTHORIZED, "unauthenticated", false)
    } else if request.uri().query().is_some() {
        error(StatusCode::BAD_REQUEST, "invalid_request", false)
    } else {
        next.run(request).await
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    response
}
async fn hosts(State(state): State<Arc<HostState>>) -> Response {
    let Ok(permit) = state.reads.clone().try_acquire_owned() else {
        return error(StatusCode::TOO_MANY_REQUESTS, "queue_full", true);
    };
    let owner = state.owner.clone();
    let hosts = match tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let owner = owner.lock().ok()?;
        let store = owner.store();
        let hosts = store.enrolled_hosts().ok()?;
        // SPEC §9.1 / T21 / P4: the approved published document is the host's
        // installation list. A missing or unreadable one marks the host
        // `unknown`; it never fails the inventory read.
        Some(
            hosts
                .into_iter()
                .map(|host| {
                    let document = store
                        .host_publication(&host.host_id)
                        .ok()
                        .flatten()
                        .and_then(|p| serde_json::from_str(&p.config_json).ok());
                    // ADR 0017: the version and skew verdict of the host's
                    // latest session, kept while it is offline.
                    let version = store.host_version(&host.host_id).ok().flatten();
                    (host, development_controls(document.as_ref()), version)
                })
                .collect::<Vec<_>>(),
        )
    })
    .await
    {
        Ok(Some(hosts)) => hosts,
        _ => {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "snapshot_unavailable",
                true,
            )
        }
    };
    let hosts: Vec<_> = hosts.into_iter().map(|(host, controls, version)| {
        let session = state.sessions.inspect(&host.host_id);
        // ADR 0017: `binary_version`, `compatibility` (supported,
        // upgrade_recommended, upgrade_required, refused) and its reason, from
        // the host's latest session; absent for a host not seen since.
        let (binary_version, compatibility, reason) = match &version {
            Some(v) => (Some(v.binary_version.clone()), Some(v.compatibility.clone()), (!v.reason.is_empty()).then(|| v.reason.clone())),
            None => (None, None, None),
        };
        serde_json::json!({"host_id":host.host_id,"name":host.host_name,"revoked":host.revoked,
            "online":session.as_ref().is_some_and(|s| s.online && !host.revoked),
            "eligible":session.as_ref().is_some_and(|s| s.eligible && !host.revoked),"session":session,
            "binary_version":binary_version,"compatibility":compatibility,"compatibility_reason":reason,
            "capabilities":version.map(|v| v.capabilities),
            "development_controls":controls})
    }).collect();
    Json(serde_json::json!({"api_version":"1","server_version":capyctl_controller::agent_sessions::SERVER_VERSION,"hosts":hosts})).into_response()
}

/// ADR 0018: every host's published runtime profiles, from the approved
/// snapshots and the hosts' reported inventories. `custom` is derived from
/// the version (outside the verified set); `deployments` are the instances of
/// the profile holding a runtime on the host now.
async fn engines(State(state): State<Arc<HostState>>) -> Response {
    let Ok(permit) = state.reads.clone().try_acquire_owned() else {
        return error(StatusCode::TOO_MANY_REQUESTS, "queue_full", true);
    };
    let owner = state.owner.clone();
    let rows = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let owner = owner.lock().ok()?;
        let store = owner.store();
        let mut rows = Vec::new();
        for host in store.enrolled_hosts().ok()? {
            let Some(publication) = store.host_publication(&host.host_id).ok().flatten() else {
                continue;
            };
            let Ok(document) = serde_json::from_str::<serde_json::Value>(&publication.config_json)
            else {
                continue;
            };
            // ADR 0033 §9: the host's `lmcache` and `retention`, as published.
            let lmcache = capyctl_config::lmcache::host_settings(&document);
            for (name, profile) in document["runtime_profiles"]
                .as_object()
                .cloned()
                .unwrap_or_default()
            {
                let deployments: Vec<String> = store
                    .profile_candidates(&host.host_id, &name)
                    .map(|found| found.into_iter().map(|c| c.name).collect())
                    .unwrap_or_default();
                let retiring = store
                    .profile_retirement(&host.host_id, &name)
                    .ok()
                    .flatten()
                    .is_some();
                rows.push((
                    (
                        host.host_id.clone(),
                        host.host_name.clone(),
                        host.revoked,
                        lmcache,
                    ),
                    name,
                    profile,
                    deployments,
                    retiring,
                ));
            }
        }
        Some(rows)
    })
    .await;
    let Ok(Some(rows)) = rows else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "snapshot_unavailable",
            true,
        );
    };
    let engines: Vec<_> = rows
        .into_iter()
        .map(
            |((host_id, host_name, revoked, lmcache), name, profile, deployments, retiring)| {
                let session = state.sessions.inspect(&host_id);
                let reported = session
                    .as_ref()
                    .and_then(|s| s.profiles.iter().find(|p| p.name == name).cloned());
                let version = reported
                    .as_ref()
                    .map(|p| p.installation.version.clone())
                    .filter(|v| !v.is_empty());
                let missing = reported.as_ref().is_some_and(|p| {
                    p.installation
                        .capabilities_missing
                        .iter()
                        .any(|c| c == "deep_park")
                });
                engine_row(EngineRow {
                    host_id: &host_id,
                    host_name: &host_name,
                    online: session.as_ref().is_some_and(|s| s.online && !revoked),
                    profile_name: &name,
                    profile: &profile,
                    reported_version: version,
                    fingerprint: reported.as_ref().map(|p| {
                        serde_json::json!({
                            "version": p.installation.version, "digest": p.installation.digest,
                            "state": p.installation.state,
                        })
                    }),
                    deep_park_missing: missing,
                    retiring,
                    deployments,
                    host_lmcache: lmcache,
                })
            },
        )
        .collect();
    Json(serde_json::json!({"api_version":"1","engines":engines})).into_response()
}

/// One row of the engines inventory, from what a host published and reported.
struct EngineRow<'a> {
    host_id: &'a str,
    host_name: &'a str,
    online: bool,
    profile_name: &'a str,
    profile: &'a serde_json::Value,
    /// The installation's version as the host reports it, when it does.
    reported_version: Option<String>,
    fingerprint: Option<serde_json::Value>,
    deep_park_missing: bool,
    retiring: bool,
    deployments: Vec<String>,
    /// ADR 0033 §9: the host's `lmcache` and `retention`.
    host_lmcache: (
        capyctl_config::lmcache::HostLmcache,
        capyctl_config::lmcache::Retention,
    ),
}

/// ADR 0018: the engines row, one shape for every role.
fn engine_row(row: EngineRow<'_>) -> serde_json::Value {
    let profile = row.profile;
    let engine = profile["engine"].as_str().unwrap_or("unknown").to_owned();
    use capyctl_config::{
        engine_policy::Engine,
        registration::{is_verified, listed_version},
    };
    let kind = Engine::from_name(&engine);
    // ADR 0029 §2: a llama.cpp row shows its `<version>+<commit>` fingerprint.
    let version = listed_version(
        kind,
        row.reported_version.as_deref(),
        profile["build_fingerprint"].as_str(),
    );
    // ADR 0018 §1: `custom` is derived, never declared.
    let custom = kind.is_none_or(|e| !is_verified(e, &version));
    let mut row_value = serde_json::json!({
        "host_id": row.host_id, "host": row.host_name,
        "online": row.online,
        "profile": row.profile_name, "engine": engine, "version": version, "custom": custom,
        "executable": profile["executable"],
        "fingerprint": row.fingerprint,
        "deep_park": profile["security"]["deep_park"].as_str().unwrap_or("enabled"),
        "deep_park_probe": if row.deep_park_missing { "capability_missing" } else { "not_reported_missing" },
        "published": "published",
        "retiring": row.retiring,
        "deployments": row.deployments,
        // ADR 0033 §9: from the host's settings, the engine and the
        // profile's registration; the host's launch-time check is what a
        // launch uses.
        "lmcache": lmcache_view(row.host_lmcache, kind, &version, profile),
    });
    if kind == Some(Engine::Custom) {
        if let Some(object) = row_value.as_object_mut() {
            object.extend(custom_engine_view(profile));
        }
    }
    row_value
}

/// ADR 0033 §9: the `lmcache` field of an engines row. A profile of an
/// engine this build does not know is unsupported.
fn lmcache_view(
    host: (
        capyctl_config::lmcache::HostLmcache,
        capyctl_config::lmcache::Retention,
    ),
    engine: Option<capyctl_config::engine_policy::Engine>,
    version: &str,
    profile: &serde_json::Value,
) -> serde_json::Value {
    use capyctl_config::lmcache::{listed, listed_view, LmcacheOff, LmcacheOffReason};
    let Some(engine) = engine else {
        let reason = LmcacheOffReason::EngineUnsupported;
        return listed_view(&Err(LmcacheOff {
            reason,
            detail: reason.sentence().to_owned(),
        }));
    };
    let registration = serde_json::from_value(profile["lmcache"].clone()).ok();
    listed_view(&listed(host, engine, version, registration.as_ref()))
}

/// ADR 0031 §6, §7, §8: what the server shows of a custom engine: the
/// executable, the argument template, the API, the environment names with
/// their sources (never a value: a profile value is the host's, and a
/// manifest value may render the engine key), the health path, the idle
/// signal (or `none`), the engine key (`per-launch` or `none`) and the
/// manifest's digest. A profile whose manifest does not validate shows
/// `manifest: null`.
pub fn custom_engine_view(
    profile: &serde_json::Value,
) -> serde_json::Map<String, serde_json::Value> {
    let Ok(custom) = capyctl_config::custom::CustomProfile::from_value(&profile["custom"]) else {
        return serde_json::Map::from_iter([("manifest".to_owned(), serde_json::Value::Null)]);
    };
    let manifest = &custom.manifest;
    let environment: Vec<serde_json::Value> = manifest
        .env
        .keys()
        .map(|name| serde_json::json!({"name": name, "source": "manifest"}))
        .chain(
            profile["env"]
                .as_object()
                .into_iter()
                .flat_map(|env| env.keys())
                .map(|name| serde_json::json!({"name": name, "source": "profile"})),
        )
        .collect();
    let idle = manifest
        .idle
        .as_ref()
        .map_or_else(|| "none".to_owned(), |idle| idle.describe());
    let key = if manifest.uses_api_key() {
        "per-launch"
    } else {
        "none"
    };
    serde_json::Map::from_iter([
        ("idle_signal".to_owned(), serde_json::json!(idle)),
        ("engine_key".to_owned(), serde_json::json!(key)),
        (
            "manifest".to_owned(),
            serde_json::json!({
                "executable": profile["executable"],
                "args": manifest.args,
                "api": manifest.api,
                "version": manifest.build_fingerprint(),
                "environment": manifest.environment,
                "env": environment,
                "health": manifest.health,
                "idle": manifest.idle,
                "digest": custom.manifest_digest,
            }),
        ),
    ])
}

/// A standalone role's one embedded host, as the inventory reads it.
#[derive(Clone)]
pub struct StandaloneHost {
    /// The host's id and name: the embedded host is published under one name.
    pub host_id: String,
    /// The host document published now, so an `engine add` shows at once.
    pub document: Arc<dyn Fn() -> serde_json::Value + Send + Sync>,
    /// The registered installations, for each profile's fingerprint.
    pub installations: Arc<capyctl_controller::installation_gate::EmbeddedInstallations>,
    /// The host's memory domains as observed now: `domain_id`, `kind`,
    /// `capacity_bytes` and `available_bytes` each. Empty when unreadable.
    pub domains: DomainSampler,
    /// SPEC §7.2 (found live 2026-10-09): the warnings the role logged at
    /// start for domains whose managed limit plus free reserve exceeded the
    /// memory available then, as an enrolled host's session shows them
    /// (`memory_warnings`). Set once the role published its policy.
    pub memory_warnings: Arc<std::sync::OnceLock<Vec<String>>>,
}

pub type DomainSampler = Arc<
    dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<serde_json::Value>> + Send>>
        + Send
        + Sync,
>;

struct StandaloneState {
    credentials: ManagementCredentials,
    owner: SharedCoordinatorState,
    host: StandaloneHost,
    reads: Arc<tokio::sync::Semaphore>,
}

/// The same two inventory routes as [`hosts_router`], for a standalone role:
/// its embedded host is not enrolled and has no agent session, so the row is
/// built from the host itself. Same authentication, same read limit, same
/// JSON shapes, so `list hosts`, `list engines` and `inspect host` read the
/// same on every role.
pub fn standalone_hosts_router(
    credentials: ManagementCredentials,
    owner: SharedCoordinatorState,
    host: StandaloneHost,
) -> Router {
    let state = Arc::new(StandaloneState {
        credentials,
        owner,
        host,
        reads: Arc::new(tokio::sync::Semaphore::new(2)),
    });
    Router::new()
        .route("/management/v1/hosts", get(standalone_hosts))
        .route("/management/v1/engines", get(standalone_engines))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            |State(state): State<Arc<StandaloneState>>, request: Request, next: Next| async move {
                guard(&state.credentials, request, next).await
            },
        ))
        .with_state(state)
}

async fn standalone_hosts(State(state): State<Arc<StandaloneState>>) -> Response {
    let Ok(_permit) = state.reads.clone().try_acquire_owned() else {
        return error(StatusCode::TOO_MANY_REQUESTS, "queue_full", true);
    };
    let document = (state.host.document)();
    let now_ms = capyctl_protocol::now_unix_ms();
    let domains: Vec<_> = (state.host.domains)()
        .await
        .into_iter()
        .map(|mut domain| {
            // Server domains carry when they were observed; standalone samples now.
            if let Some(map) = domain.as_object_mut() {
                map.insert("observed_at_unix_ms".into(), now_ms.into());
                map.insert("observed_at_unix".into(), (now_ms / 1000).into());
            }
            domain
        })
        .collect();
    let views = state.host.installations.views();
    // Owner decision 4: a drain of the embedded host that has not settled.
    let (owner, host_id) = (state.owner.clone(), state.host.host_id.clone());
    let drain_pending = tokio::task::spawn_blocking(move || {
        owner
            .lock()
            .ok()
            .and_then(|owner| owner.store().host_drain_pending(&host_id).ok())
            .unwrap_or(false)
    })
    .await
    .unwrap_or(false);
    let profiles: Vec<_> = document["runtime_profiles"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(name, profile)| {
            let installation = views.iter().find(|v| v["profile"] == name.as_str());
            serde_json::json!({
                "name": name,
                "build_fingerprint": profile["build_fingerprint"],
                "eligibility": "eligible",
                "installation": serde_json::json!({
                    "version": installation.map_or(&serde_json::Value::Null, |v| &v["version"]),
                    "digest": installation.map_or(&serde_json::Value::Null, |v| &v["digest"]),
                    "state": installation.map_or(&serde_json::Value::Null, |v| &v["state"]),
                    "capabilities_missing": installation
                        .and_then(|v| v["capabilities_missing"].as_array().cloned())
                        .unwrap_or_default(),
                }),
            })
        })
        .collect();
    let id = &state.host.host_id;
    let mut session = serde_json::json!({"profiles": profiles, "domains": domains,
        "reconciled": true, "drain_pending": drain_pending});
    if let Some(warnings) = state.host.memory_warnings.get().filter(|w| !w.is_empty()) {
        session["memory_warnings"] = serde_json::json!(warnings);
    }
    Json(serde_json::json!({
        "api_version": "1",
        "server_version": capyctl_controller::agent_sessions::SERVER_VERSION,
        "hosts": [{
            "host_id": id, "name": id, "revoked": false, "online": true, "eligible": !drain_pending,
            "session": session,
            "capabilities": capyctl_protocol::capabilities::agent_capabilities(),
            "binary_version": capyctl_controller::agent_sessions::SERVER_VERSION,
            "compatibility": "supported", "compatibility_reason": null,
            "development_controls": development_controls(Some(&document)),
        }],
    }))
    .into_response()
}

async fn standalone_engines(State(state): State<Arc<StandaloneState>>) -> Response {
    let Ok(permit) = state.reads.clone().try_acquire_owned() else {
        return error(StatusCode::TOO_MANY_REQUESTS, "queue_full", true);
    };
    let document = (state.host.document)();
    let views = state.host.installations.views();
    let owner = state.owner.clone();
    let host_id = state.host.host_id.clone();
    let rows = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let owner = owner.lock().ok()?;
        let store = owner.store();
        let mut rows = Vec::new();
        for (name, profile) in document["runtime_profiles"]
            .as_object()
            .cloned()
            .unwrap_or_default()
        {
            let deployments: Vec<String> = store
                .profile_candidates(&host_id, &name)
                .map(|found| found.into_iter().map(|c| c.name).collect())
                .unwrap_or_default();
            let retiring = store
                .profile_retirement(&host_id, &name)
                .ok()
                .flatten()
                .is_some();
            let installation = views.iter().find(|v| v["profile"] == name.as_str());
            rows.push(engine_row(EngineRow {
                host_id: &host_id,
                host_name: &host_id,
                online: true,
                profile_name: &name,
                profile: &profile,
                reported_version: installation
                    .and_then(|v| v["version"].as_str())
                    .map(str::to_owned),
                fingerprint: installation.map(|v| {
                    serde_json::json!({
                        "version": v["version"], "digest": v["digest"], "state": v["state"],
                    })
                }),
                deep_park_missing: installation.is_some_and(|v| {
                    v["capabilities_missing"]
                        .as_array()
                        .is_some_and(|m| m.iter().any(|c| c == "deep_park"))
                }),
                retiring,
                deployments,
                host_lmcache: capyctl_config::lmcache::host_settings(&document),
            }));
        }
        Some(rows)
    })
    .await;
    let Ok(Some(engines)) = rows else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "snapshot_unavailable",
            true,
        );
    };
    Json(serde_json::json!({"api_version":"1","engines":engines})).into_response()
}

/// SPEC §9.1 / T21 / ADR 0012 / P4: mark every runtime profile (engine
/// installation) of the host's published document whose launches enable vLLM
/// development mode. Derived from the document, never declared. The host is
/// `exposed` if any installation is, `unknown` if nothing was published or any
/// installation cannot be read, else `not_exposed`.
fn development_controls(document: Option<&serde_json::Value>) -> serde_json::Value {
    use capyctl_store::development_controls::{for_host_profile, ExposureState};
    let Some(profiles) = document.and_then(|d| d["runtime_profiles"].as_object()) else {
        return serde_json::json!({"state": ExposureState::Unknown, "installations": []});
    };
    let marks: Vec<_> = profiles
        .iter()
        .map(|(name, profile)| (name, for_host_profile(profile)))
        .collect();
    let state = if marks.iter().any(|(_, m)| m.state == ExposureState::Exposed) {
        ExposureState::Exposed
    } else if marks.iter().any(|(_, m)| m.state == ExposureState::Unknown) {
        ExposureState::Unknown
    } else {
        ExposureState::NotExposed
    };
    let installations: Vec<_> = marks
        .into_iter()
        .map(|(name, mark)| {
            let mut value = serde_json::to_value(mark).unwrap_or_default();
            if let Some(object) = value.as_object_mut() {
                object.insert("profile".into(), name.clone().into());
            }
            value
        })
        .collect();
    serde_json::json!({"state": state, "installations": installations})
}

#[cfg(test)]
mod tests {
    // T21, owner decision 2026-09-22: an SGLang installation in the host view
    // carries its unauthenticated loopback `/metrics`, derived, never declared;
    // a vLLM installation does not.
    #[test]
    fn an_sglang_installation_marks_its_unauthenticated_metrics() {
        let document = serde_json::json!({"runtime_profiles": {
            "sglang": {"engine": "sglang", "security": {}},
            "vllm": {"engine": "vllm", "security": {"deep_park": "disabled"}},
        }});
        let view = super::development_controls(Some(&document));
        let find = |profile: &str| {
            view["installations"]
                .as_array()
                .unwrap()
                .iter()
                .find(|i| i["profile"] == profile)
                .unwrap()
                .clone()
        };
        assert_eq!(
            find("sglang")["unauthenticated_local_surfaces"],
            serde_json::json!({"surface": ["/metrics"], "listener": "loopback", "access": "read_only"})
        );
        assert!(find("vllm").get("unauthenticated_local_surfaces").is_none());
        assert_eq!(view["state"], "not_exposed");
    }

    // T44 T37 (ADR 0031 §6, §7, §8): the server's engines row of a custom
    // engine carries its manifest (executable, argument template, API,
    // environment names with their sources), its idle signal and engine key;
    // never an environment value. A known engine's row is unchanged.
    #[test]
    fn a_custom_engine_row_shows_its_manifest_never_a_value() {
        use capyctl_config::custom::{CustomManifest, CustomProfile};
        use capyctl_config::registration::{custom_profile_document, CustomProfileSpec};
        let manifest = CustomManifest::from_value(serde_json::json!({
            "kind": "custom_engine", "schema_version": 1, "api": "decisions",
            "args": ["--host", "{host}", "--port", "{port}", "--model", "{model_dir}"],
            "env": {"SERVE_KEY": "{api_key}"},
            "idle": {"url": "/metrics", "gauges": ["running"]},
        }))
        .unwrap();
        let profile = custom_profile_document(&CustomProfileSpec {
            executable: "/opt/serve/bin/serve".into(),
            custom: CustomProfile::new(manifest.clone()),
            installation_drift: capyctl_config::effective::InstallationDrift::Warn,
            env: [("SERVE_MODE".to_owned(), "zq-profile-value".to_owned())].into(),
            approved_env: vec![],
        });
        let row = |profile: &serde_json::Value| {
            super::engine_row(super::EngineRow {
                host_id: "h",
                host_name: "h",
                online: true,
                profile_name: "serve",
                profile,
                reported_version: None,
                fingerprint: None,
                deep_park_missing: false,
                retiring: false,
                deployments: vec![],
                host_lmcache: Default::default(),
            })
        };
        let custom = row(&profile);
        assert!(!custom.to_string().contains("zq-profile-value"), "{custom}");
        assert_eq!(custom["custom"], true);
        assert_eq!(custom["version"], "unversioned");
        assert_eq!(custom["idle_signal"], "/metrics gauges running");
        assert_eq!(custom["engine_key"], "per-launch");
        assert_eq!(custom["manifest"]["executable"], "/opt/serve/bin/serve");
        assert_eq!(custom["manifest"]["api"], "decisions");
        assert_eq!(custom["manifest"]["args"], serde_json::json!(manifest.args));
        assert_eq!(
            custom["manifest"]["env"],
            serde_json::json!([
                {"name": "SERVE_KEY", "source": "manifest"},
                {"name": "SERVE_MODE", "source": "profile"},
            ])
        );
        assert_eq!(custom["manifest"]["digest"], manifest.digest());
        let known = row(
            &serde_json::json!({"engine": "vllm", "build_fingerprint": "0.30.0",
            "security": {"deep_park": "enabled"}}),
        );
        for field in ["manifest", "idle_signal", "engine_key"] {
            assert!(known.get(field).is_none(), "{known}");
        }
    }

    // T45 (ADR 0033 §9): the server's engines row carries `lmcache`: the
    // registered version, or off with the first reason the server knows
    // (the host's published `retention` and `lmcache`, the engine and its
    // version, the profile's registration).
    #[test]
    fn an_engines_row_shows_lmcache_on_or_off_with_the_reason() {
        use capyctl_config::lmcache::{host_settings, HostLmcache, Retention};
        let row = |profile: serde_json::Value, host: (HostLmcache, Retention)| {
            super::engine_row(super::EngineRow {
                host_id: "h",
                host_name: "h",
                online: true,
                profile_name: "p",
                profile: &profile,
                reported_version: None,
                fingerprint: None,
                deep_park_missing: false,
                retiring: false,
                deployments: vec![],
                host_lmcache: host,
            })["lmcache"]
                .clone()
        };
        let registered = serde_json::json!({"engine": "sglang", "build_fingerprint": "0.5.22",
            "lmcache": {"version": "0.5.6", "digest": "sha256:00"}, "security": {}});
        let defaults = Default::default();
        assert_eq!(
            row(registered.clone(), defaults),
            serde_json::json!({"state": "on", "version": "0.5.6"})
        );
        let reason = |profile: serde_json::Value, host| row(profile, host)["reason"].clone();
        assert_eq!(
            reason(
                registered.clone(),
                host_settings(&serde_json::json!({"lmcache": "off"}))
            ),
            "host_off"
        );
        assert_eq!(
            reason(
                registered.clone(),
                host_settings(&serde_json::json!({"retention": "zero", "lmcache": "off"}))
            ),
            "retention_zero"
        );
        let mut old = registered.clone();
        old["build_fingerprint"] = "0.5.21".into();
        assert_eq!(reason(old, defaults), "engine_version_unsupported");
        let mut bare = registered.clone();
        bare.as_object_mut().unwrap().remove("lmcache");
        assert_eq!(reason(bare, defaults), "not_installed");
        let mut below = registered;
        below["lmcache"]["version"] = "0.5.5".into();
        assert_eq!(reason(below, defaults), "version_unsupported");
        let llamacpp = serde_json::json!({"engine": "llamacpp", "build_fingerprint": "b1+abc"});
        assert_eq!(reason(llamacpp, defaults), "engine_unsupported");
    }
}
