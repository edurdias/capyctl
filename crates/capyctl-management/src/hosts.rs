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
                    (host.host_id.clone(), host.host_name.clone(), host.revoked),
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
            |((host_id, host_name, revoked), name, profile, deployments, retiring)| {
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
}

/// ADR 0018: the engines row, one shape for every role.
fn engine_row(row: EngineRow<'_>) -> serde_json::Value {
    let profile = row.profile;
    let engine = profile["engine"].as_str().unwrap_or("unknown").to_owned();
    let version = row
        .reported_version
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| {
            profile["build_fingerprint"]
                .as_str()
                .unwrap_or("unknown")
                .to_owned()
        });
    // ADR 0018 §1: `custom` is derived, never declared.
    use capyctl_config::{engine_policy::Engine, registration::is_verified};
    let custom = Engine::from_name(&engine).is_none_or(|e| !is_verified(e, &version));
    serde_json::json!({
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
    })
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
    Json(serde_json::json!({
        "api_version": "1",
        "server_version": capyctl_controller::agent_sessions::SERVER_VERSION,
        "hosts": [{
            "host_id": id, "name": id, "revoked": false, "online": true, "eligible": !drain_pending,
            "session": {"profiles": profiles, "domains": domains, "reconciled": true,
                "drain_pending": drain_pending},
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
}
