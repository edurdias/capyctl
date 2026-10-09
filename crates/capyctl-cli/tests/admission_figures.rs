//! SPEC §7.2 (found live 2026-10-09 on a standalone host): a deployment whose
//! startup charge fits the managed limit is still refused while the memory
//! the host has available now, less that charge, would not leave the free
//! reserve. The rule stays; the refusal now carries its figures through the
//! operation's failure, status and the CLI, and a role whose managed limit
//! plus free reserve exceeds the memory available at start warns so, with its
//! figures, in its log and in `list hosts`.
//!
//! In-process standalone on the testkit's Fake installation, driven through
//! the shipped binary. CPU only; not qualification of any native engine.
mod support;

use capyctl_cli::host_observation::fixed_memory;
use capyctl_config::effective::{Engine, ModelSource};
use serde_json::Value;

const GIB: i64 = 1 << 30;

/// The role under test, serving its management API on a free port.
struct Role {
    dir: tempfile::TempDir,
    address: std::net::SocketAddr,
    server: tokio::task::JoinHandle<()>,
    _app: capyctl_cli::roles::App,
}

impl Role {
    /// A standalone role on 32 GiB with `available` of it free. Its derived
    /// limits are a 16 GiB managed limit and a 6.4 GiB free reserve.
    async fn boot(available: i64) -> Self {
        let dir = support::safe_state_dir();
        let app = support::boot_with_memory(
            dir.path(),
            fixed_memory(support::TEST_CAPACITY_BYTES, available),
        )
        .await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = app.management_router();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            dir,
            address,
            server,
            _app: app,
        }
    }

    /// Runs the binary against this role: whether it succeeded, its standard
    /// output and its standard error.
    async fn run(&self, args: &[&str]) -> (bool, String, String) {
        let (state, address) = (self.dir.path().to_owned(), self.address.to_string());
        let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        tokio::task::spawn_blocking(move || {
            let out = support::capyctl()
                .env("CAPYCTL_STATE_DIR", state)
                .env(capyctl_cli::roles::MANAGEMENT_ADDR_ENV, address)
                .args(&args)
                .output()
                .unwrap();
            (
                out.status.success(),
                String::from_utf8_lossy(&out.stdout).into_owned(),
                String::from_utf8_lossy(&out.stderr).into_owned(),
            )
        })
        .await
        .unwrap()
    }

    async fn json(&self, args: &[&str]) -> Value {
        let mut args = args.to_vec();
        args.extend(["--format", "json"]);
        let (ok, out, err) = self.run(&args).await;
        assert!(ok, "{args:?}: {out}{err}");
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"))
    }

    /// Deploys the unified template sized from the role's 32 GiB: a cold
    /// charge of 20 %, 6.4 GiB. Returns its id.
    async fn deploy(&self) -> String {
        let config = capyctl_cli::standalone_config::deployment_document(
            "near-limit",
            "near-limit",
            &ModelSource::Local {
                path: "/models/near-limit".into(),
            },
            Engine::Vllm,
            &capyctl_cli::standalone_config::TemplateMemory::Unified {
                capacity_bytes: support::TEST_CAPACITY_BYTES,
            },
            capyctl_cli::standalone_config::DEFAULT_REQUEST_DEADLINE,
            false,
            "local",
        )
        .expect("the unified template");
        let path = self.dir.path().join("deployment.json");
        std::fs::write(&path, config.to_string()).unwrap();
        let deployed = self
            .json(&["deploy", "model", "--file", path.to_str().unwrap()])
            .await;
        deployed["deployment_id"].as_str().unwrap().to_owned()
    }
}

impl Drop for Role {
    fn drop(&mut self) {
        self.server.abort();
    }
}

// T29 (found live 2026-10-09): 10 GiB available of 32. The 6.4 GiB start
// fits the 16 GiB managed limit, so it is placed, but 10 − 6.4 leaves less
// than the 6.4 GiB free reserve: refused, uncertainty keeps accounting, and
// status (JSON and text) gives the available memory, the charge, the reserve
// and the shortfall instead of a bare `insufficient resources`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_refused_on_available_memory_reports_its_figures() {
    let role = Role::boot(10 * GIB).await;
    let id = role.deploy().await;
    let receipt = role.json(&["start", "deployment", &id]).await;
    assert!(receipt["operation_id"].is_string(), "{receipt}");
    const FIGURES: &str = "insufficient_memory: needs 6.4 GiB of unified memory, 10.0 GiB \
        available and a 6.4 GiB free reserve to keep, 2.8 GiB short";
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let status = loop {
        let status = role.json(&["status", "deployment", &id]).await;
        if status.to_string().contains("insufficient_memory") {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no refusal recorded: {status}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    };
    let latest = &status["instances"][0]["latest_operation"];
    let reason = latest["reason"].as_str().unwrap_or_default();
    assert!(reason.contains(FIGURES), "{status}");
    // The closed code selects the operator hint, as for a host's refusal.
    assert!(
        latest["hint"]
            .as_str()
            .is_some_and(|h| h.contains("cannot hold the launch now")),
        "{status}"
    );
    let (_, text, _) = role.run(&["status", "deployment", &id]).await;
    assert!(text.contains(FIGURES), "{text}");
}

// T29: the same role warns at start, with the figures, that a deployment
// near its 16 GiB limit cannot be admitted until memory is freed: 16 + 6.4
// is more than the 10 GiB available. `list hosts` shows it, JSON and text.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_role_warns_when_its_limits_exceed_the_memory_available() {
    let role = Role::boot(10 * GIB).await;
    let hosts = role.json(&["list", "hosts"]).await;
    let warnings = &hosts["hosts"][0]["session"]["memory_warnings"];
    const WARNING: &str = "unified memory has 10.0 GiB available, less than its 16.0 GiB \
        managed limit plus its 6.4 GiB free reserve (22.4 GiB): a deployment near the limit \
        cannot be admitted until 12.4 GiB more is free";
    assert_eq!(warnings, &serde_json::json!([WARNING]), "{hosts}");
    let (ok, text, _) = role.run(&["list", "hosts"]).await;
    assert!(ok, "{text}");
    assert!(
        text.contains(&format!("warning: standalone: {WARNING}")),
        "{text}"
    );
}

// T29: all 32 GiB available covers the limit plus the reserve: no warning.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_role_with_room_for_its_limits_does_not_warn() {
    let role = Role::boot(support::TEST_CAPACITY_BYTES).await;
    let hosts = role.json(&["list", "hosts"]).await;
    assert!(
        hosts["hosts"][0]["session"]
            .get("memory_warnings")
            .is_none(),
        "{hosts}"
    );
    let (ok, text, _) = role.run(&["list", "hosts"]).await;
    assert!(ok, "{text}");
    assert!(!text.contains("warning"), "{text}");
}
