//! ADR 0040 §§10–11, AE6: real standalone composition with fake installations.
//! These CPU tests are not native-engine qualification.
mod support;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

fn script(path: &Path, body: &str) {
    capyctl_config::test_support::write_executable(path, format!("#!/bin/sh\n{body}\n"), 0o755)
        .unwrap();
}
fn document(state: &Path) -> PathBuf {
    let path = state.join("standalone.yaml");
    std::fs::write(&path, "schema_version: 1\nkind: standalone\nname: local\nhost:\n  name: local\n  runtime_profiles: {}\n").unwrap();
    path
}
fn vllm(root: &Path) -> PathBuf {
    let site = root.join("lib/python3.12/site-packages");
    std::fs::create_dir_all(site.join("vllm")).unwrap();
    std::fs::create_dir_all(site.join("vllm-0.29.0.dist-info")).unwrap();
    std::fs::write(
        site.join("vllm-0.29.0.dist-info/METADATA"),
        "Name: vllm\nVersion: 0.29.0\n",
    )
    .unwrap();
    std::fs::write(root.join("pyvenv.cfg"), "home = /usr/bin\n").unwrap();
    std::fs::create_dir_all(root.join("bin")).unwrap();
    script(&root.join("bin/vllm"), "echo 0.29.0");
    script(
        &root.join("bin/python3"),
        &format!(
            "echo '{}'",
            json!({"schema":"capyctl/engine-capabilities/v1","engine":"vllm","capabilities":{"core":[],"deep_park":[],"metrics":[]}})
        ),
    );
    root.to_owned()
}
async fn serve(
    app: &capyctl_cli::roles::App,
    state: &Path,
) -> (String, String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = app.management_router();
    let server =
        tokio::spawn(async move { support::serve_management(listener, router).await.unwrap() });
    let credentials = std::fs::read_to_string(state.join("identity/credentials")).unwrap();
    let token = credentials
        .lines()
        .find_map(|line| line.strip_prefix("admin_token: "))
        .unwrap()
        .to_owned();
    (format!("http://{address}/management/v1"), token, server)
}
async fn call(
    base: &str,
    token: &str,
    path: &str,
    body: Option<Value>,
) -> (reqwest::StatusCode, Value) {
    let client = reqwest::Client::new();
    let request = match body {
        Some(body) => client.post(format!("{base}{path}")).json(&body),
        None => client.get(format!("{base}{path}")),
    };
    let response = request.bearer_auth(token).send().await.unwrap();
    (response.status(), response.json().await.unwrap())
}

// KTD11: detection, publication, inventory, named-path refusal and retirement
// all traverse the actual role provider, not an HTTP mock.
#[tokio::test]
async fn detected_builtin_add_is_published_and_remove_is_accepted() {
    let state = support::safe_state_dir();
    let config = document(state.path());
    let installation = vllm(&state.path().join("outside-search/vllm-env"));
    let app = support::boot_registered_only(state.path(), &config)
        .await
        .unwrap();
    let (base, token, server) = serve(&app, state.path()).await;
    let (status, found) = call(&base, &token, "/hosts/standalone/engines/detected", None).await;
    assert!(status.is_success(), "{found}");
    assert!(!found["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .any(|candidate| candidate["entry"]
            .as_str()
            .is_some_and(|entry| entry.starts_with(installation.to_str().unwrap()))));
    let query = reqwest::Url::parse_with_params(
        &format!("{base}/hosts/standalone/engines/detected"),
        &[("path", installation.to_str().unwrap())],
    )
    .unwrap();
    let found: Value = reqwest::Client::new()
        .get(query)
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let candidate = found["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|candidate| {
            candidate["entry"]
                .as_str()
                .is_some_and(|entry| entry.starts_with(installation.to_str().unwrap()))
        })
        .expect("extra-directory candidate");
    let (status, refused) = call(
        &base,
        &token,
        "/hosts/standalone/engines",
        Some(json!({"path":candidate["entry"],"name":"vllm-api"})),
    )
    .await;
    assert!(!status.is_success());
    assert_eq!(
        refused["error"]["code"], "confirmation_required",
        "{refused}"
    );
    let (status, added) = call(
        &base,
        &token,
        "/hosts/standalone/engines",
        Some(json!({"path":candidate["entry"],"paths":[installation],"name":"vllm-api"})),
    )
    .await;
    assert!(status.is_success(), "{status} {added}");
    assert!(app.profiles().contains(&"vllm-api".into()));
    let (status, engines) = call(&base, &token, "/engines", None).await;
    assert!(status.is_success());
    assert!(engines.to_string().contains("vllm-api"), "{engines}");
    let (status, removed) = call(
        &base,
        &token,
        "/hosts/standalone/engines/vllm-api/remove",
        Some(json!({})),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::ACCEPTED, "{removed}");
    assert!(!app.profiles().contains(&"vllm-api".into()));
    server.abort();
    let _ = app.shutdown().await;
}

// KTD11: the embedded host has no host.yaml. Compare the route with the
// same effective-config function used by inspect, over its running document.
#[tokio::test]
async fn host_config_is_the_running_embedded_host_effective_config() {
    let state = support::safe_state_dir();
    let config = document(state.path());
    let app = support::boot_registered_only(state.path(), &config)
        .await
        .unwrap();
    let (base, token, server) = serve(&app, state.path()).await;
    let (status, actual) = call(&base, &token, "/hosts/standalone/config", None).await;
    assert!(status.is_success(), "{actual}");
    let empty = capyctl_config::setting_overrides::NamedLayer::default();
    let overrides =
        capyctl_config::setting_overrides::SettingOverrides::none(capyctl_config::ConfigKind::Host);
    let expected = capyctl_agent::host_admin::effective_config(
        capyctl_config::ConfigKind::Host,
        &config,
        Some(&app.host_document()),
        capyctl_agent::host_admin::ConfigLayers {
            env: &empty,
            flags: &empty,
            overrides: &overrides,
        },
        std::env::var_os("HOME").as_deref().map(Path::new),
        state.path(),
    );
    assert_eq!(actual, expected);
    let (status, refused) = call(&base, &token, "/hosts/not-this-host/config", None).await;
    assert!(!status.is_success());
    assert_eq!(refused["error"]["code"], "host_not_found");
    server.abort();
    let _ = app.shutdown().await;
}
