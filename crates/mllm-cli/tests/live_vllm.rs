//! The S1 live suite: eleven scenarios driven against a real vLLM installation.
//!
//! Every other integration test in this crate runs on the embedded Fake engine and
//! proves only that the controller's own machinery holds together. Passing them is
//! never qualification of a native recipe (SPEC §18). This file is where a native
//! recipe is actually exercised: a real engine process is launched, served through
//! the router, stopped with its group proven empty, restarted, and driven into three
//! failure shapes.
//!
//! Nothing here runs unless `MLLM_LIVE=1`. Without it every scenario returns at its
//! first line, so the file compiles and passes on a developer machine and states
//! nothing at all about any engine. A green run without `MLLM_LIVE=1` is therefore
//! not evidence; only a run on the authorized host is, and the runner
//! (`scripts/live/run-on-spark.sh`) is what produces one.
//!
//! The suite must run with `--test-threads=1`. Three of the scenarios change process
//! environment variables in order to boot standalone against a different engine
//! installation, and the environment is process-global: a second test running beside
//! them would read the first one's exports and report a result about an engine it
//! never used.
//!
//! Timings and samples are appended to `target/live/current/results.md`, which the
//! runner copies back into the evidence entry in `docs/runbooks/spark-live-f2.md`.

use std::fmt::Display;
use std::io::Write as _;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs as _};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use mllm_cli::roles::{self, App, StartError};
use mllm_config::effective::ModelSource;
use mllm_controller::{LifecycleFault, LifecyclePort as _};
use mllm_domain::{LifecycleAction, LifecycleState};

/// The model the live run serves. Relative, so it resolves against the model store
/// the host declared through `MLLM_MODELS_ROOT` rather than against a path this file
/// would otherwise have to guess.
const LIVE_MODEL: &str = "qwen3-4b-instruct";

/// How long a cold start is given before the wait is reported as a hang. A start
/// that has not settled by then is a regression to investigate, not a suite to sit
/// in: giving up here never cancels the operation.
const SETTLE: Duration = Duration::from_secs(600);

/// How long a direct request to the engine or to a refused address may take. Short,
/// because every one of them is either answered immediately or is the refusal the
/// scenario is asserting.
const DIRECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Whether this process was asked to touch a real engine.
fn live() -> bool {
    std::env::var("MLLM_LIVE").as_deref() == Ok("1")
}

/// A state directory the controller lock will accept.
///
/// The lock refuses any ancestor that is group- or other-writable, because such an
/// ancestor lets another account replace the directory the lock guards. `/tmp` is
/// 1777, so the home directory is the root that satisfies the rule.
fn state_dir() -> tempfile::TempDir {
    let home = std::env::var("HOME").expect("HOME is set");
    tempfile::TempDir::new_in(home).expect("a state directory under an owner-only root")
}

/// Where the run's evidence is collected.
///
/// The workspace target directory, not the crate's: the runner copies
/// `target/live/current/` back off the host, and a per-crate path would leave it
/// copying an empty directory.
fn results_path() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("live")
        .join("current");
    std::fs::create_dir_all(&dir).expect("the evidence directory is creatable");
    dir.join("results.md")
}

/// Append one `key: value` line of evidence.
///
/// Appending rather than rewriting is deliberate: a scenario that fails half way
/// still leaves behind what it had measured up to that point, which is usually the
/// number that explains the failure.
fn record(key: &str, value: impl Display) {
    let line = format!("{key}: {value}\n");
    print!("RECORD {line}");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(results_path())
        .expect("the results file is writable");
    file.write_all(line.as_bytes())
        .expect("the results line is written");
}

fn seconds(elapsed: Duration) -> String {
    format!("{:.1} s", elapsed.as_secs_f64())
}

/// A process's argv, as the kernel recorded it at exec.
///
/// Read from `/proc/<pid>/cmdline` rather than from the launch plan, because the
/// question the scenario asks is what the engine was actually given — a plan that
/// says the right thing and an argv that does not is exactly the failure worth
/// catching.
fn engine_argv(pid: u32) -> String {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline"))
        .unwrap_or_else(|error| panic!("the engine process {pid} has an argv: {error}"));
    String::from_utf8_lossy(&raw).replace('\0', " ")
}

/// Whether a pid currently names a process at all.
///
/// A pid alone is not an identity — the kernel reuses them — so this is only ever
/// used to assert absence, where reuse would make the assertion stricter rather than
/// weaker.
fn pid_exists(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// Every local address a socket is listening on, as `(hex address, port)`.
///
/// Parsed from `/proc/net/tcp` and `/proc/net/tcp6` rather than shelled out to
/// `ss`, so the assertion needs no tool installed on the host and cannot be
/// defeated by a different `ss` output format. State `0A` is `TCP_LISTEN`.
fn listening_sockets() -> Vec<(String, u16)> {
    let mut found = Vec::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(table) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let mut fields = line.split_whitespace();
            let Some(local) = fields.nth(1) else { continue };
            let Some(state) = fields.nth(1) else { continue };
            if state != "0A" {
                continue;
            }
            let Some((address, port)) = local.split_once(':') else {
                continue;
            };
            let Ok(port) = u16::from_str_radix(port, 16) else {
                continue;
            };
            found.push((address.to_string(), port));
        }
    }
    found
}

/// The hex local addresses a loopback-only listener may legitimately show.
fn is_loopback_hex(address: &str) -> bool {
    // 127.0.0.1 little-endian, and ::1 as /proc renders it.
    address == "0100007F" || address == "00000000000000000000000001000000"
}

/// This host's first routable IPv4 address, or `None` when it has none.
///
/// Read from `/proc/net/fib_trie`, where every locally configured address appears as
/// a `/32 host LOCAL` entry under the address it belongs to. `hostname -I` is the
/// fallback for a kernel that does not expose the trie.
fn routable_ipv4() -> Option<String> {
    if let Ok(trie) = std::fs::read_to_string("/proc/net/fib_trie") {
        let lines: Vec<&str> = trie.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            let Some(address) = line.trim().strip_prefix("|-- ") else {
                continue;
            };
            let local = lines
                .get(index + 1)
                .is_some_and(|next| next.contains("/32 host LOCAL"));
            if local && !address.starts_with("127.") && address.parse::<std::net::Ipv4Addr>().is_ok()
            {
                return Some(address.to_string());
            }
        }
    }
    let printed = std::process::Command::new("hostname").arg("-I").output().ok()?;
    String::from_utf8_lossy(&printed.stdout)
        .split_whitespace()
        .find(|candidate| {
            !candidate.starts_with("127.") && candidate.parse::<std::net::Ipv4Addr>().is_ok()
        })
        .map(str::to_string)
}

/// `MemAvailable` from `/proc/meminfo`, in bytes.
///
/// `MemAvailable` rather than `MemFree`: on a unified-memory host the page cache the
/// weights were read through is reclaimable, and counting it as used would report a
/// leak on every run.
fn mem_available_bytes() -> i64 {
    let meminfo = std::fs::read_to_string("/proc/meminfo").expect("/proc/meminfo is readable");
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            let kib: i64 = rest
                .split_whitespace()
                .next()
                .and_then(|value| value.parse().ok())
                .expect("MemAvailable states a number");
            return kib * 1024;
        }
    }
    panic!("/proc/meminfo states no MemAvailable");
}

fn gib(bytes: i64) -> String {
    format!("{:.2} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

/// Process-global environment restored when the scenario ends.
///
/// Three scenarios boot standalone against a different installation than the host's,
/// and the only way to say so is an exported variable. Restoring on drop is what
/// keeps a failure in the middle of one scenario from silently changing what the
/// next one tests.
struct EnvGuard {
    saved: Vec<(String, Option<String>)>,
}

impl EnvGuard {
    fn set(changes: &[(&str, Option<&str>)]) -> Self {
        let mut saved = Vec::new();
        for (name, value) in changes {
            saved.push(((*name).to_string(), std::env::var(name).ok()));
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
        Self { saved }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in &self.saved {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

/// Boot standalone against the engine installation this host's environment declares.
async fn boot_live(dir: &std::path::Path) -> App {
    roles::start_standalone(dir)
        .await
        .expect("standalone boots against the host's engine installation")
}

/// Serve the router over a real listener, as a user reaches it.
async fn serve(app: &App) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback listener");
    let address = listener.local_addr().expect("the listener has an address");
    let router = app.router();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (address, handle)
}

/// Drive a transition and wait for it to settle, bounded.
async fn transition(
    app: &App,
    id: &str,
    action: LifecycleAction,
) -> Result<LifecycleState, LifecycleFault> {
    let operation = app.controller.request_transition(id, action).await?;
    tokio::time::timeout(SETTLE, app.controller.wait_terminal(&operation))
        .await
        .expect("the transition settles rather than hanging")
}

/// Assert that a launch failure closed the deployment rather than leaving it
/// uncertain, and that everything the launch held was released.
///
/// SPEC §13.2 and ADR 0011: uncertainty must retain accounting, so `Closed` is a
/// claim about proof — the recorded processes were terminated and observed gone, the
/// reservation was released against that proof, and the deployment shut its own
/// admission. `Uncertain` is the opposite claim and must never be accepted here.
///
/// The closure is read on the public surface: the operation's own outcome, the
/// routes the authority still offers (admission is what gates them), the runtime it
/// still reports, the ledger's owners, and the journal. There is no public reader
/// for `deployments.admission_enabled` itself, and `list_enabled_route_ids` is gated
/// on exactly that column, so it is the honest projection of it.
fn assert_closed(app: &App, id: &str, route: &str, outcome: Result<LifecycleState, LifecycleFault>) {
    match outcome {
        Err(LifecycleFault::Uncertain(reason)) => panic!(
            "the launch was left uncertain rather than closed, so its accounting is \
             still charged: {reason}"
        ),
        Err(LifecycleFault::Failed(_)) | Err(LifecycleFault::Blocked(_)) => {}
        Err(other) => panic!("the launch failed in an unexpected way: {other}"),
        Ok(state) => panic!("the launch was expected to fail and reached {state:?}"),
    }
    assert!(
        !app.controller
            .list_enabled_route_ids()
            .expect("routes are readable")
            .contains(&route.to_string()),
        "a closed deployment must stop offering its route"
    );
    assert!(
        app.controller
            .runtime_endpoint(id)
            .expect("the runtime is readable")
            .is_none(),
        "a closed deployment must retain no runtime"
    );
    assert!(
        app.store
            .resource_snapshot()
            .expect("the ledger is readable")
            .owners
            .is_empty(),
        "a closed deployment must hold no reservation"
    );
    let evidence = app
        .store
        .journal_evidence_of(id)
        .expect("the journal is readable");
    assert!(
        evidence.iter().any(|entry| entry.contains(id)),
        "SPEC §17: the failure must be journaled naming the deployment: {evidence:?}"
    );
    record(
        &format!("journal {route}"),
        evidence.last().cloned().unwrap_or_default(),
    );
}

// T10  L1 launch, L2 serve, L3 access control, L4 stop, L5 restart.
//
// One ordered test rather than five, because the engine is the expensive part: a
// suite that booted a real vLLM once per assertion would spend its run loading
// weights. The ordering is the scenario, not an accident of convenience.
#[tokio::test]
async fn l1_to_l5_cycle() {
    if !live() {
        return;
    }
    let dir = state_dir();
    let app = boot_live(dir.path()).await;
    let id = app
        .deploy(
            "qwen3-4b",
            ModelSource::Local {
                path: LIVE_MODEL.into(),
            },
        )
        .expect("standalone creates its own deployment");

    // L1: a real engine comes up, and what it was given is what the profile said.
    let started = Instant::now();
    let state = transition(&app, &id, LifecycleAction::Start)
        .await
        .expect("the start settles");
    assert_eq!(
        state,
        LifecycleState::Ready,
        "SPEC §6.1: liveness of an HTTP server is not model readiness, so Ready here \
         is the engine's own readiness proof"
    );
    record("L1 ready_secs", seconds(started.elapsed()));

    let identities = app
        .controller
        .live_identities(&id)
        .expect("the recorded identities are readable");
    assert!(
        identities.iter().any(|identity| identity.role == "api"),
        "the launch records its api process: {identities:?}"
    );
    assert!(
        identities.iter().any(|identity| identity.role == "worker-0"),
        "the launch records its worker: {identities:?}"
    );
    let api = identities
        .iter()
        .find(|identity| identity.role == "api")
        .expect("the api process is recorded");
    let argv = engine_argv(api.pid);
    for flag in [
        "--host",
        "--served-model-name",
        "--tensor-parallel-size",
        "--kv-cache-dtype",
        "--block-size",
    ] {
        assert!(argv.contains(flag), "{flag} missing from argv: {argv}");
    }
    record("L1 argv", argv.trim());

    // L2: one inference through the router, plain and streaming.
    let (address, served) = serve(&app).await;
    let client = reqwest::Client::new();
    let key = app.api_key().to_string();

    let plain_started = Instant::now();
    let plain = client
        .post(format!("http://{address}/v1/chat/completions"))
        .header("Authorization", format!("Bearer {key}"))
        .json(&serde_json::json!({
            "model": "qwen3-4b",
            "messages": [{"role": "user", "content": "Reply with the single word: ready."}],
            "max_tokens": 32
        }))
        .send()
        .await
        .expect("the router answers");
    let status = plain.status();
    let completion: serde_json::Value = plain.json().await.expect("a JSON completion");
    assert_eq!(status, 200, "one inference is served: {completion}");
    let content = completion["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(
        !content.is_empty(),
        "the completion carries content from the engine: {completion}"
    );
    record("L2 plain_secs", seconds(plain_started.elapsed()));
    record("L2 plain_sample", content.replace('\n', " "));

    let stream_started = Instant::now();
    let streamed = client
        .post(format!("http://{address}/v1/chat/completions"))
        .header("Authorization", format!("Bearer {key}"))
        .json(&serde_json::json!({
            "model": "qwen3-4b",
            "messages": [{"role": "user", "content": "Count: one two three."}],
            "max_tokens": 32,
            "stream": true
        }))
        .send()
        .await
        .expect("the router answers");
    assert_eq!(streamed.status(), 200, "the streaming inference is served");
    let body = streamed.text().await.expect("the stream body");
    assert!(
        body.contains("chat.completion.chunk"),
        "the router forwards the engine's stream: {body}"
    );
    assert!(
        body.contains("[DONE]"),
        "the stream is terminated by the protocol's own marker: {body}"
    );
    record("L2 stream_secs", seconds(stream_started.elapsed()));

    // L3: the engine is reachable only through the router, and only with a key.
    let anonymous = client
        .get(format!("http://{address}/v1/models"))
        .send()
        .await
        .expect("the router answers");
    assert!(
        anonymous.status() == 401 || anonymous.status() == 403,
        "SPEC §9: the router must refuse an unauthenticated caller, not answer it"
    );

    // The router offers two paths and no path-through to the engine, so an engine
    // control route is not merely unauthorized through it — it does not exist.
    // `ChatForward` carries a chat body and no path, which is what makes an upstream
    // forward of `/metrics` unexpressible rather than merely refused.
    let metrics = client
        .get(format!("http://{address}/metrics"))
        .header("Authorization", format!("Bearer {key}"))
        .send()
        .await
        .expect("the router answers");
    assert_eq!(
        metrics.status(),
        404,
        "the router must offer no upstream path beyond the inference surface"
    );

    let runtime = app
        .controller
        .runtime_endpoint(&id)
        .expect("the runtime is readable")
        .expect("a ready deployment has a recorded runtime");
    let engine_url: reqwest::Url = runtime.endpoint.parse().expect("the endpoint is a URL");
    let engine_port = engine_url.port().expect("the endpoint names a port");
    let engine_key = runtime
        .engine_key
        .clone()
        .expect("SPEC §3: a launch mints its own engine key");
    let first_incarnation = runtime.incarnation.clone();

    let listening = listening_sockets();
    let bound: Vec<&(String, u16)> = listening
        .iter()
        .filter(|(_, port)| *port == engine_port)
        .collect();
    assert!(
        !bound.is_empty(),
        "the engine's port {engine_port} is not listening at all"
    );
    assert!(
        bound.iter().all(|(address, _)| is_loopback_hex(address)),
        "SPEC §9: the engine must listen on loopback only, found {bound:?}"
    );
    record("L3 engine_port", engine_port);

    if let Some(routable) = routable_ipv4() {
        let target = format!("{routable}:{engine_port}")
            .to_socket_addrs()
            .expect("the routable address parses")
            .next()
            .expect("the routable address resolves");
        let reached = TcpStream::connect_timeout(&target, DIRECT_TIMEOUT);
        assert!(
            reached.is_err(),
            "the engine answered on {target}, so it is reachable off the loopback"
        );
        record("L3 offhost_refused", target);
    } else {
        record("L3 offhost_refused", "skipped: the host has no routable IPv4");
    }

    let direct = reqwest::Client::builder()
        .timeout(DIRECT_TIMEOUT)
        .build()
        .expect("a direct client");
    let engine = |path: &str| format!("{}{path}", runtime.endpoint.trim_end_matches('/'));

    let unkeyed_models = direct
        .get(engine("/v1/models"))
        .send()
        .await
        .expect("the engine answers");
    assert_eq!(
        unkeyed_models.status(),
        401,
        "SPEC §9: the engine's inference surface requires its own key"
    );

    // SPEC §9.1 / T21: vLLM 0.29 authenticates only the `/v1`, `/v2`, `/inference`
    // and `/cohere` prefixes, which leaves the parking control routes open to any
    // local caller. mllm's guard middleware is what closes them, and this is the
    // assertion that it is actually loaded into the engine process.
    for control in ["/sleep", "/collective_rpc"] {
        let unkeyed = direct
            .post(engine(control))
            .json(&serde_json::json!({}))
            .send()
            .await
            .expect("the engine answers");
        assert_eq!(
            unkeyed.status(),
            401,
            "{control} is open to an unauthenticated local caller"
        );
    }
    let sleeping = direct
        .get(engine("/is_sleeping"))
        .header("Authorization", format!("Bearer {engine_key}"))
        .send()
        .await
        .expect("the engine answers");
    assert_eq!(
        sleeping.status(),
        200,
        "the key that launched the engine must reach its control routes (a 404 here \
         means deep park is disabled on this host, not that the guard refused)"
    );
    record("L3 guard", "control routes refuse an unkeyed caller");

    // L4: the stop leaves nothing behind.
    let stopped_at = Instant::now();
    let state = transition(&app, &id, LifecycleAction::Stop)
        .await
        .expect("the stop settles");
    assert_eq!(state, LifecycleState::Stopped);
    record("L4 stop_secs", seconds(stopped_at.elapsed()));
    for identity in &identities {
        assert!(
            !pid_exists(identity.pid),
            "SPEC §5: {} (pid {}) outlived the stop",
            identity.role,
            identity.pid
        );
    }
    assert!(
        app.controller
            .runtime_endpoint(&id)
            .expect("the runtime is readable")
            .is_none(),
        "the stop must release the binding, and with it the port it leased"
    );
    assert!(
        app.store
            .resource_snapshot()
            .expect("the ledger is readable")
            .owners
            .is_empty(),
        "the stop must release the reservation it held"
    );
    assert!(
        !listening_sockets()
            .iter()
            .any(|(_, port)| *port == engine_port),
        "the engine's port is still listening after the stop"
    );

    // L5: the restart is a new launch, not a resumed one.
    let restarted = Instant::now();
    let state = transition(&app, &id, LifecycleAction::Start)
        .await
        .expect("the restart settles");
    assert_eq!(state, LifecycleState::Ready);
    record("L5 restart_secs", seconds(restarted.elapsed()));
    let second = app
        .controller
        .runtime_endpoint(&id)
        .expect("the runtime is readable")
        .expect("the restarted deployment has a runtime");
    assert_ne!(
        second.incarnation, first_incarnation,
        "a restart is a new incarnation, not the previous one resumed"
    );
    assert_ne!(
        second.engine_key.as_deref(),
        Some(engine_key.as_str()),
        "SPEC §3: a key is minted per launch, so a retired one cannot reach the new \
         engine"
    );
    let restarted_identities = app
        .controller
        .live_identities(&id)
        .expect("the recorded identities are readable");
    assert!(
        restarted_identities
            .iter()
            .all(|identity| !identities.iter().any(|old| old.pid == identity.pid)),
        "the restart reused a pid from the launch it replaced"
    );

    let _ = transition(&app, &id, LifecycleAction::Stop).await;
    served.abort();
}

// T20  L6 a model directory with no model in it closes the deployment, L7 a sound
// source starts and answers on the same host afterwards.
#[tokio::test]
async fn l6_l7_failure_and_recovery() {
    if !live() {
        return;
    }
    let dir = state_dir();
    let app = boot_live(dir.path()).await;

    // L6: an empty directory is a path that exists and holds no checkpoint, which is
    // the failure a bad deployment actually looks like — a path that does not exist
    // at all would be caught before the engine ever ran.
    let empty = state_dir();
    let bad = app
        .deploy(
            "bad-source",
            ModelSource::Local {
                path: empty.path().to_string_lossy().into_owned(),
            },
        )
        .expect("the deployment is created");
    let failed_at = Instant::now();
    let outcome = transition(&app, &bad, LifecycleAction::Start).await;
    record("L6 close_secs", seconds(failed_at.elapsed()));
    assert_closed(&app, &bad, "bad-source", outcome);

    // SPEC §13.2: the reason is the engine's own, and it is redacted before it is
    // written anywhere the owner-only log is not.
    let evidence = app
        .store
        .journal_evidence_of(&bad)
        .expect("the journal is readable");
    assert!(
        evidence
            .iter()
            .any(|entry| entry.to_lowercase().contains("launch failed")),
        "the journal must name what failed, not only that something did: {evidence:?}"
    );

    // L7: the same host, the same controller, a sound source. A failure that closed
    // one deployment must not have cost the host its ability to serve.
    let good = app
        .deploy(
            "recovered",
            ModelSource::Local {
                path: LIVE_MODEL.into(),
            },
        )
        .expect("the deployment is created");
    let recovered_at = Instant::now();
    let state = transition(&app, &good, LifecycleAction::Start)
        .await
        .expect("the recovery start settles");
    assert_eq!(state, LifecycleState::Ready);
    record("L7 ready_secs", seconds(recovered_at.elapsed()));

    let (address, served) = serve(&app).await;
    let answer = reqwest::Client::new()
        .post(format!("http://{address}/v1/chat/completions"))
        .header("Authorization", format!("Bearer {}", app.api_key()))
        .json(&serde_json::json!({
            "model": "recovered",
            "messages": [{"role": "user", "content": "Reply with the single word: ready."}],
            "max_tokens": 32
        }))
        .send()
        .await
        .expect("the router answers");
    let status = answer.status();
    let completion: serde_json::Value = answer.json().await.expect("a JSON completion");
    assert_eq!(status, 200, "the recovered deployment serves: {completion}");
    let content = completion["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(!content.is_empty(), "the answer carries content: {completion}");
    record("L7 sample", content.replace('\n', " "));

    let _ = transition(&app, &good, LifecycleAction::Stop).await;
    served.abort();
}

// T20  L8 an executable that exits at once is a closed deployment, never an
// uncertain one.
#[tokio::test]
async fn l8_executable_that_exits_at_once() {
    if !live() {
        return;
    }
    // A fresh standalone against a declared installation that is not an engine. The
    // fingerprint is declared rather than probed, because `/bin/false` prints no
    // version and the provider would refuse to boot at all — which would test the
    // provider's refusal instead of the launch failure this scenario is about.
    let _env = EnvGuard::set(&[
        ("MLLM_VLLM_BIN", Some("/bin/false")),
        ("MLLM_ENGINE_FINGERPRINT", Some("false-v1")),
    ]);
    let dir = state_dir();
    let app = boot_live(dir.path()).await;
    let id = app
        .deploy(
            "exits-at-once",
            ModelSource::Local {
                path: LIVE_MODEL.into(),
            },
        )
        .expect("the deployment is created");

    let failed_at = Instant::now();
    let outcome = transition(&app, &id, LifecycleAction::Start).await;
    record("L8 close_secs", seconds(failed_at.elapsed()));
    let identities = app
        .controller
        .live_identities(&id)
        .expect("the recorded identities are readable");
    assert_closed(&app, &id, "exits-at-once", outcome);

    // Spec §6: the group is disposed of, including the shell the launcher used to
    // set the group up. A leftover `sh` is what an unproven release looks like.
    for identity in &identities {
        assert!(
            !pid_exists(identity.pid),
            "{} (pid {}) survived a launch that failed",
            identity.role,
            identity.pid
        );
    }
    record("L8 leftovers", identities.len());
}

// T20  L9 a deployment whose request deadline is shorter than the window an
// activation is given is refused before anything is launched.
//
// The scenario was written as "engine alive at its readiness deadline, terminated,
// gone, Closed". Reading the path shows it cannot be reached that way: the
// coordinator schedules an administrative start at a fixed ten-minute activation
// window (`ACTIVATION_WINDOW_MS`), and `accept_start` refuses any operation whose
// deadline is further out than the deployment's own `request_deadline`. A twenty
// second deadline therefore fails admission rather than expiring mid-load, and the
// readiness bound itself is `min(step deadline, initialize_timeout)` — a floor of
// thirty seconds and a standalone setting of nine hundred, neither reachable from a
// deployment document. What this asserts is the bound that does exist, and the fact
// that refusing it is definite: nothing was launched, so there is nothing to
// reconcile.
#[tokio::test]
async fn l9_readiness_deadline() {
    if !live() {
        return;
    }
    let dir = state_dir();
    let app = boot_live(dir.path()).await;
    let id = app
        .deploy_with_deadline(
            "short-deadline",
            ModelSource::Local {
                path: LIVE_MODEL.into(),
            },
            "20s",
        )
        .expect("a short deadline is a valid deployment document");

    let refused = app
        .controller
        .request_transition(&id, LifecycleAction::Start)
        .await;
    let reason = match refused {
        Err(LifecycleFault::Blocked(reason)) => reason,
        Err(LifecycleFault::Uncertain(reason)) => panic!(
            "a refusal that launched nothing must not be uncertain: {reason}"
        ),
        Err(other) => panic!("the start was refused in an unexpected way: {other}"),
        Ok(_) => panic!(
            "a deployment deadline shorter than the activation window must not admit \
             a start"
        ),
    };
    record("L9 refusal", reason);

    // Nothing was launched, so nothing is owed: no runtime, no reservation, and the
    // deployment still offers no route.
    assert!(
        app.controller
            .runtime_endpoint(&id)
            .expect("the runtime is readable")
            .is_none(),
        "a refused start must retain no runtime"
    );
    assert!(
        app.store
            .resource_snapshot()
            .expect("the ledger is readable")
            .owners
            .is_empty(),
        "a refused start must hold no reservation"
    );
    assert!(
        !app.controller
            .list_enabled_route_ids()
            .expect("routes are readable")
            .contains(&"short-deadline".to_string()),
        "a deployment that never started must not be offering its route"
    );
}

// L10 a host that declares no engine does not boot.
//
// The other half of this scenario — that the shipped release binary carries no test
// engine — is `scripts/check-release-clean.sh`, which the runner runs on the box:
// it reads the linked binary's own symbols, which no test inside the binary can do
// about itself.
#[tokio::test]
async fn l10_no_engine_no_boot() {
    if !live() {
        return;
    }
    let _env = EnvGuard::set(&[("MLLM_VLLM_BIN", None)]);
    let dir = state_dir();
    match roles::start_standalone(dir.path()).await {
        // Spec §8: no engine installation, no boot. A host that cannot start an
        // engine must say so rather than come up serving nothing.
        Err(StartError::NoEngineInstallation(what)) => record("L10 refusal", what),
        Err(other) => panic!("standalone refused for the wrong reason: {other}"),
        Ok(_) => panic!("standalone booted on a host that declares no engine"),
    }
}

// L11 the memory a run borrowed comes back.
#[tokio::test]
async fn l11_memory_returns() {
    if !live() {
        return;
    }
    let dir = state_dir();
    let app = boot_live(dir.path()).await;
    let id = app
        .deploy(
            "memory-cycle",
            ModelSource::Local {
                path: LIVE_MODEL.into(),
            },
        )
        .expect("the deployment is created");

    let before = mem_available_bytes();
    record("L11 before", gib(before));

    let state = transition(&app, &id, LifecycleAction::Start)
        .await
        .expect("the start settles");
    assert_eq!(state, LifecycleState::Ready);
    let ready = mem_available_bytes();
    record("L11 ready", gib(ready));
    assert!(
        ready < before,
        "a loaded engine must account for the memory it holds: {} then {}",
        gib(before),
        gib(ready)
    );

    let state = transition(&app, &id, LifecycleAction::Stop)
        .await
        .expect("the stop settles");
    assert_eq!(state, LifecycleState::Stopped);
    let after = mem_available_bytes();
    record("L11 after", gib(after));
    record("L11 delta", gib(before - after));

    // Two gibibytes of slack, because `MemAvailable` moves on its own: other work on
    // the host, reclaim that has not run yet, and the controller's own footprint all
    // land inside it. A leak of a model's weights does not.
    const SLACK: i64 = 2 * 1024 * 1024 * 1024;
    assert!(
        (before - after) < SLACK,
        "the stop did not return what the run borrowed: {} before, {} after",
        gib(before),
        gib(after)
    );
}
