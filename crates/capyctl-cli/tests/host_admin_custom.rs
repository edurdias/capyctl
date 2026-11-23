//! ADR 0040 §11, AE6: custom preview and confirmation through standalone.
mod support;
use serde_json::{json, Value};

// AE6: pins the previewed installation without executing the custom program.
#[tokio::test]
async fn custom_preview_confirm_pins_and_stale_or_wrong_name_refuses() {
    let state = support::safe_state_dir();
    let config = state.path().join("standalone.yaml");
    std::fs::write(&config, "schema_version: 1\nkind: standalone\nname: local\nhost:\n  name: local\n  runtime_profiles: {}\n").unwrap();
    let root = state.path().join("serve-venv");
    let dependency = root.join("lib/python3.12/site-packages/serve_dep/__init__.py");
    std::fs::create_dir_all(dependency.parent().unwrap()).unwrap();
    std::fs::write(&dependency, "VERSION = 1\n").unwrap();
    std::fs::create_dir_all(root.join("bin")).unwrap();
    std::fs::write(root.join("pyvenv.cfg"), "home = /usr/bin\n").unwrap();
    let executable = root.join("bin/serve");
    let marker = state.path().join("executed");
    capyctl_config::test_support::write_executable(
        &executable,
        format!("#!/bin/sh\ntouch {}\n", marker.display()),
        0o755,
    )
    .unwrap();
    let app = support::boot_registered_only(state.path(), &config)
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = app.management_router();
    let server =
        tokio::spawn(async move { support::serve_management(listener, router).await.unwrap() });
    let credentials = std::fs::read_to_string(state.path().join("identity/credentials")).unwrap();
    let token = credentials
        .lines()
        .find_map(|line| line.strip_prefix("admin_token: "))
        .unwrap();
    let client = reqwest::Client::new();
    let body = json!({"operation":"add","name":"serve-api","executable":executable,
        "manifest":"kind: custom_engine\nschema_version: 1\napi: decisions\nversion: '2.2.0'\nargs: [--host, '{host}', --port, '{port}', --model, '{model_dir}']\nhealth: /health\n",
        "env":{"SERVE_MODE":"fast"}});
    let preview_url =
        format!("http://{address}/management/v1/hosts/standalone/custom-engines/preview");
    let confirm_url = format!("http://{address}/management/v1/hosts/standalone/custom-engines");
    let response = client
        .post(&preview_url)
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let preview: Value = response.json().await.unwrap();
    assert!(status.is_success(), "{status} {preview}");
    assert_eq!(preview["command"][0], executable.to_str().unwrap());
    assert!(!preview["summary"].as_str().unwrap().is_empty());
    assert!(preview["digest"].as_str().unwrap().starts_with("sha256:"));
    let mut confirm = body.clone();
    confirm["digest"] = preview["digest"].clone();
    confirm["confirm_name"] = json!("wrong");
    let refused: Value = client
        .post(&confirm_url)
        .bearer_auth(token)
        .json(&confirm)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(refused["error"]["code"], "confirmation_stale", "{refused}");
    confirm["confirm_name"] = json!("serve-api");
    confirm["env"]["SERVE_MODE"] = json!("slow");
    let refused: Value = client
        .post(&confirm_url)
        .bearer_auth(token)
        .json(&confirm)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(refused["error"]["code"], "confirmation_stale", "{refused}");
    confirm["env"]["SERVE_MODE"] = json!("fast");
    std::fs::write(&dependency, "VERSION = 2\n").unwrap();
    let refused: Value = client
        .post(&confirm_url)
        .bearer_auth(token)
        .json(&confirm)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(refused["error"]["code"], "confirmation_stale", "{refused}");
    let preview: Value = client
        .post(&preview_url)
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    confirm["digest"] = preview["digest"].clone();
    let response = client
        .post(&confirm_url)
        .bearer_auth(token)
        .json(&confirm)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let added: Value = response.json().await.unwrap();
    assert!(status.is_success(), "{status} {added}");
    assert!(app.profiles().contains(&"serve-api".into()));
    let file = capyctl_config::registration::EnginesFile::load(
        &capyctl_config::registration::custom_engines_beside(
            &capyctl_config::registration::engines_path(state.path()),
        ),
    )
    .unwrap();
    assert!(file.profiles["serve-api"].to_string().contains("sha256:"));
    assert!(
        !marker.exists(),
        "preview and confirm never execute the program"
    );
    server.abort();
    let _ = app.shutdown().await;
}
