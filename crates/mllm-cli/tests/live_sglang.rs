//! The SGLang live gate: launch to Ready, one routed inference, stop with the
//! group proven gone and the memory returned.
//!
//! Every other integration test in this crate runs on the embedded Fake engine
//! and proves only that the controller's own machinery holds together. Passing
//! them is never qualification of a native recipe (SPEC §18). This file is
//! where the SGLang native recipe is exercised: a real engine process is
//! launched through the protected sglang_entry wrapper — whose startup
//! composition includes the placement gate over the host's published
//! `device_inventory_digest` — served through the router, and stopped with its
//! group proven empty.
//!
//! Nothing here runs unless `MLLM_LIVE=1` and the host declares its SGLang
//! installation through `MLLM_SGLANG_BIN` (the venv interpreter: the launcher
//! execs it with `-IS` and the protected wrapper) and `MLLM_MODELS_ROOT`.
//! Without all three every scenario returns at its first line, so the file
//! compiles and passes on a developer machine and states nothing at all about
//! any engine. A green run without those variables is therefore not evidence;
//! only a run on the authorized host is, and the runner
//! (`scripts/live/run-on-spark.sh`) is what produces one.
//!
//! The suite must run with `--test-threads=1`. Each scenario boots the one
//! device this host publishes, and two engines beside each other would fight
//! for it; the ordering of a shared device is a scenario constraint, not an
//! accident of convenience.
//!
//! Timings and samples are appended to `target/live/current/results.md`, which
//! the runner copies back into the evidence entry in
//! `docs/runbooks/spark-live-f2.md`.

use std::fmt::Display;
use std::io::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use mllm_cli::roles::{self, App};
use mllm_config::effective::ModelSource;
use mllm_controller::{LifecycleFault, LifecyclePort as _};
use mllm_domain::{LifecycleAction, LifecycleState};

/// The model the live run serves. Relative, so it resolves against the model
/// store the host declared through `MLLM_MODELS_ROOT` rather than against a
/// path this file would otherwise have to guess.
const LIVE_MODEL: &str = "qwen3-4b-instruct";

/// How long a cold start is given before the wait is reported as a hang. A
/// start that has not settled by then is a regression to investigate, not a
/// suite to sit in: giving up here never cancels the operation.
const SETTLE: Duration = Duration::from_secs(900);

/// Whether this process was asked to touch a real engine, and the host has
/// declared the installation to touch it with.
fn live() -> bool {
    std::env::var("MLLM_LIVE").as_deref() == Ok("1")
        && std::env::var("MLLM_SGLANG_BIN").is_ok_and(|value| !value.is_empty())
        && std::env::var("MLLM_MODELS_ROOT").is_ok_and(|value| !value.is_empty())
}

/// A state directory the controller lock will accept.
///
/// The lock refuses any ancestor that is group- or other-writable, because such
/// an ancestor lets another account replace the directory the lock guards.
/// `/tmp` is 1777, so the home directory is the root that satisfies the rule.
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
/// still leaves behind what it had measured up to that point, which is usually
/// the number that explains the failure.
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

/// One environment variable of a running process, read from
/// `/proc/<pid>/environ`. This is where the guarded launcher's
/// `CUDA_VISIBLE_DEVICES` is actually corroborated: what the child inherited,
/// not what a plan said it would.
fn engine_env(pid: u32, name: &str) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    String::from_utf8_lossy(&raw)
        .split('\0')
        .find_map(|entry| entry.strip_prefix(&format!("{name}=")).map(str::to_string))
}

/// Whether a pid currently names a process at all.
///
/// A pid alone is not an identity — the kernel reuses them — so this is only ever
/// used to assert absence, where reuse would make the assertion stricter rather
/// than weaker.
fn pid_exists(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// Every process whose argv names a running SGLang engine, as `(pid, argv)`.
///
/// The obvious way to ask this is `pgrep -af sglang`, and the obvious way is
/// wrong: pgrep matches its pattern against the argv of every process including
/// the shell that is asking, so the question answers itself and reports an
/// engine on an empty host. Reading `/proc` and skipping this process asks the
/// same question with nothing that can match its own text — none of the markers
/// below appear in this binary's own argv.
fn engine_processes() -> Vec<(u32, String)> {
    let mine = std::process::id();
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return found;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if pid == mine {
            continue;
        }
        let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        let argv = String::from_utf8_lossy(&raw).replace('\0', " ");
        if ["sglang_entry", "sglang.launch_server", "sglang::scheduler"]
            .iter()
            .any(|marker| argv.contains(marker))
        {
            found.push((pid, argv.trim().to_string()));
        }
    }
    found
}

/// `MemAvailable` from `/proc/meminfo`, in bytes.
///
/// `MemAvailable` rather than `MemFree`: on a unified-memory host the page cache
/// the weights were read through is reclaimable, and counting it as used would
/// report a leak on every run.
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

/// Boot standalone against the SGLang installation this host's environment
/// declares.
async fn boot_live(dir: &std::path::Path) -> App {
    roles::start_standalone(dir)
        .await
        .expect("standalone boots against the host's SGLang installation")
}

/// Serve the router over a real listener, as a user reaches it.
async fn serve(app: &App) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
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

// T10  SGL1 launch to Ready, SGL2 one routed inference, SGL3 stop with the
// group proven gone and the memory returned.
//
// One ordered test rather than three, because the engine is the expensive
// part: a suite that booted a real SGLang once per assertion would spend its
// run staging weights. The ordering is the scenario, not an accident of
// convenience; each stage records its own evidence key.
#[tokio::test]
async fn sgl1_to_sgl3_gate() {
    if !live() {
        return;
    }
    // Keep the journal and private logs available when a native gate fails.
    // The state directory remains owner-only; evidence records only its path.
    let dir = state_dir().keep();
    record("SGL state_dir", dir.display());
    let app = boot_live(&dir).await;
    let id = app
        .deploy(
            "qwen3-4b",
            ModelSource::Local {
                path: LIVE_MODEL.into(),
            },
        )
        .expect("standalone creates its own deployment");

    // SGL3's baseline: the memory the host has before the engine borrows any.
    let before = mem_available_bytes();
    record("SGL3 before", gib(before));

    // SGL1: a real engine comes up through the protected wrapper, and the
    // child inherited the device namespace the host policy published.
    let started = Instant::now();
    let state = transition(&app, &id, LifecycleAction::Start)
        .await
        .expect("the start settles");
    assert_eq!(
        state,
        LifecycleState::Ready,
        "SPEC §6.1: liveness of an HTTP server is not model readiness, so Ready here \
         is the engine's own readiness proof — and for SGLang that proof includes \
         the native placement gate over the host's published inventory digest"
    );
    record("SGL1 ready_secs", seconds(started.elapsed()));

    let identities = app
        .controller
        .live_identities(&id)
        .expect("the recorded identities are readable");
    assert!(
        identities.iter().any(|identity| identity.role == "api"),
        "the launch records its api process: {identities:?}"
    );
    assert!(
        identities
            .iter()
            .any(|identity| identity.role == "worker-0"),
        "the launch records its worker: {identities:?}"
    );
    let api = identities
        .iter()
        .find(|identity| identity.role == "api")
        .expect("the api process is recorded");
    let argv = engine_argv(api.pid);
    // The launch is the protected wrapper, not a rendered engine CLI: the
    // public settings travel as one JSON argument and the credentials ride
    // protected descriptor fds.
    assert!(
        argv.contains("sglang_entry.py") && argv.contains("--public-settings-json"),
        "the launch went through the protected wrapper: {argv}"
    );
    assert!(
        !argv.contains("--api-key"),
        "no credential reached argv: {argv}"
    );
    record("SGL1 argv", argv.trim());

    let namespace = engine_env(api.pid, "CUDA_VISIBLE_DEVICES").unwrap_or_default();
    assert!(
        namespace.starts_with("GPU-") && namespace.len() == 40,
        "the guarded launcher set the child's CUDA namespace to a complete physical \
         UUID, found {namespace:?}"
    );
    record("SGL1 cuda_visible_devices", &namespace);

    // The ready footprint, for SGL3's delta: what a loaded engine accounts for.
    let ready_memory = mem_available_bytes();
    record("SGL3 ready", gib(ready_memory));
    assert!(
        ready_memory < before,
        "a loaded engine must account for the memory it holds: {} then {}",
        gib(before),
        gib(ready_memory)
    );

    // SGL2: one inference through the router. The probe inside the launch
    // already proved the model answers; this proves the routed surface does.
    let (address, served) = serve(&app).await;
    let inference_started = Instant::now();
    let plain = reqwest::Client::new()
        .post(format!("http://{address}/v1/chat/completions"))
        .header("Authorization", format!("Bearer {}", app.api_key()))
        .json(&serde_json::json!({
            "model": "qwen3-4b",
            "messages": [{"role": "user", "content": "Say ready."}],
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
    record("SGL2 inference_secs", seconds(inference_started.elapsed()));
    record("SGL2 sample", content.replace('\n', " "));

    // SGL3: the stop leaves nothing behind, and the memory comes back.
    let stopped_at = Instant::now();
    let state = transition(&app, &id, LifecycleAction::Stop)
        .await
        .expect("the stop settles");
    assert_eq!(state, LifecycleState::Stopped);
    record("SGL3 stop_secs", seconds(stopped_at.elapsed()));
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
    // Stronger than the recorded pids being gone: a process the launch never
    // recorded would survive that check and is exactly what an incomplete
    // group teardown leaves behind.
    let survivors = engine_processes();
    assert!(
        survivors.is_empty(),
        "an SGLang process outlived the stop: {survivors:?}"
    );
    let after = mem_available_bytes();
    record("SGL3 after", gib(after));
    record("SGL3 delta", gib(before - after));
    // Two gibibytes of slack, because `MemAvailable` moves on its own: other
    // work on the host, reclaim that has not run yet, and the controller's own
    // footprint all land inside it. A leak of a model's weights does not.
    const SLACK: i64 = 2 * 1024 * 1024 * 1024;
    assert!(
        (before - after) < SLACK,
        "the stop did not return what the run borrowed: {} before, {} after",
        gib(before),
        gib(after)
    );

    served.abort();
}
