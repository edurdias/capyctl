//! ADR 0040 §5 / KTD6: sign-in links from the local CLI, never a browser bearer token.
mod support;

use axum::{http::HeaderMap, routing::post, Json, Router};
use serde_json::{json, Value};
use std::{path::PathBuf, process::Output};

const ADMIN: &str = "standalone-admin-token-0123456789";
const LINK: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const EXPIRES: i64 = 1_800_000_600_000;

struct Machine(tempfile::TempDir);
impl Machine {
    fn new() -> Self {
        let root = support::safe_state_dir();
        std::fs::create_dir_all(root.path().join("identity")).unwrap();
        std::fs::write(
            root.path().join("identity/credentials"),
            format!("admin_token: {ADMIN}\napi_key: test\n"),
        )
        .unwrap();
        Self(root)
    }
    fn document(&self, public: Option<&str>) -> PathBuf {
        let path = self.0.path().join("config/standalone.yaml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut management = json!({"bind": "127.0.0.1:7443"});
        if let Some(public) = public {
            management["public_address"] = json!(public);
        }
        std::fs::write(&path, json!({"schema_version": 1, "kind": "standalone", "name": "test", "server": {"listeners": {"management": management}}}).to_string()).unwrap();
        path
    }
    async fn api(&self) -> std::net::SocketAddr {
        let app = Router::new().route(
            "/management/v1/ui/sign-in-links",
            post(|headers: HeaderMap| async move {
                assert_eq!(headers["authorization"], format!("Bearer {ADMIN}"));
                Json(json!({"api_version": "1", "token": LINK, "expires_at_ms": EXPIRES}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        capyctl_cli::roles::record_management_address(self.0.path(), address);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        address
    }
    async fn run(&self, args: &[&str]) -> Output {
        let state = self.0.path().to_owned();
        let args: Vec<_> = args.iter().map(|s| s.to_string()).collect();
        tokio::task::spawn_blocking(move || {
            support::capyctl()
                .args(args)
                .env("CAPYCTL_STATE_DIR", state)
                .output()
                .unwrap()
        })
        .await
        .unwrap()
    }
}
fn stdout(out: &Output) -> String {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout.clone()).unwrap()
}

// U5 / R6 R9: the configured public origin wins without a command-line override.
#[tokio::test]
async fn public_address_url() {
    let machine = Machine::new();
    machine.document(Some("https://box.tail1234.ts.net"));
    machine.api().await;
    let text = stdout(&machine.run(&["open", "ui"]).await);
    assert!(text.contains(&format!("https://box.tail1234.ts.net/ui/sign-in#t={LINK}")));
    assert!(text.contains('▀') || text.contains('▄') || text.contains('█'));
    assert!(!text.contains("A phone needs"));
    assert!(!text.contains(ADMIN));
}

// U5 / R6 R9: loopback links explain the phone's off-box prerequisite.
#[tokio::test]
async fn loopback_url_and_off_box_note() {
    let machine = Machine::new();
    let address = machine.api().await;
    let text = stdout(&machine.run(&["open", "ui"]).await);
    assert!(text.contains(&format!("http://{address}/ui/sign-in#t={LINK}")));
    assert!(text.contains("A phone needs"));
    assert!(text.contains("remote: true"));
}

// U5 / KTD6: --address overrides public_address; an origin must have a scheme.
#[tokio::test]
async fn address_override_and_scheme_less_refusal() {
    let machine = Machine::new();
    machine.document(Some("https://box.tail1234.ts.net"));
    machine.api().await;
    let text = stdout(
        &machine
            .run(&["open", "ui", "--address", "http://100.64.1.2:7443"])
            .await,
    );
    assert!(text.contains(&format!("http://100.64.1.2:7443/ui/sign-in#t={LINK}")));
    assert!(!text.contains("box.tail1234.ts.net"));
    let out = machine
        .run(&["open", "ui", "--address", "100.64.1.2:7443"])
        .await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("http://"));
}

// U5 / ADR 0021: JSON is machine-readable URL and expiry, without rendered QR.
#[tokio::test]
async fn json_fields_without_qr() {
    let machine = Machine::new();
    let address = machine.api().await;
    let text = stdout(&machine.run(&["open", "ui", "--format", "json"]).await);
    let value: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        value["url"],
        format!("http://{address}/ui/sign-in#t={LINK}")
    );
    assert_eq!(value["expires_at_ms"], EXPIRES);
    assert!(value.get("qr").is_none());
    assert!(!text.contains(['▀', '▄', '█']));
    assert!(!text.contains(ADMIN));
}

// U5: resolution and a stopped listener give the same errors as other clients.
#[tokio::test]
async fn role_not_running_matches_other_client_commands() {
    let machine = Machine::new();
    std::fs::remove_file(machine.0.path().join("identity/credentials")).unwrap();
    let open = machine.run(&["open", "ui", "--format", "json"]).await;
    let list = machine
        .run(&["list", "deployments", "--format", "json"])
        .await;
    assert_eq!(open.status.code(), list.status.code());
    assert_eq!(open.stderr, list.stderr);
    std::fs::write(
        machine.0.path().join("identity/credentials"),
        format!("admin_token: {ADMIN}\n"),
    )
    .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    capyctl_cli::roles::record_management_address(machine.0.path(), listener.local_addr().unwrap());
    drop(listener);
    let open = machine.run(&["open", "ui", "--format", "json"]).await;
    let list = machine
        .run(&["list", "deployments", "--format", "json"])
        .await;
    assert_eq!(open.status.code(), list.status.code());
    assert_eq!(open.stderr, list.stderr);
}

// U5: a host directs the owner to the server that serves the UI.
#[tokio::test]
async fn host_says_ui_is_served_by_server() {
    let machine = Machine::new();
    let document = machine.0.path().join("host.yaml");
    std::fs::write(&document, "schema_version: 1\nkind: host\n").unwrap();
    let out = machine
        .run(&["open", "ui", "--config", document.to_str().unwrap()])
        .await;
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("UI is served by the server"));
}
