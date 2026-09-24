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
use mllm_controller::{agent_sessions::AgentSessions, ownership::SharedCoordinatorState};
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
        .layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .with_state(state)
}
async fn authenticate(
    State(state): State<Arc<HostState>>,
    request: Request,
    next: Next,
) -> Response {
    let mut response = if !state.credentials.accepts(&request) {
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
    Json(serde_json::json!({"api_version":"1","server_version":mllm_controller::agent_sessions::SERVER_VERSION,"hosts":hosts})).into_response()
}

/// SPEC §9.1 / T21 / ADR 0012 / P4: mark every runtime profile (engine
/// installation) of the host's published document whose launches enable vLLM
/// development mode. Derived from the document, never declared. The host is
/// `exposed` if any installation is, `unknown` if nothing was published or any
/// installation cannot be read, else `not_exposed`.
fn development_controls(document: Option<&serde_json::Value>) -> serde_json::Value {
    use mllm_store::development_controls::{for_host_profile, ExposureState};
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
