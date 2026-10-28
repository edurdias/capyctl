//! ADR 0039 §14 (owner decision 2026-10-27, question 5) end to end on the
//! host agent: a launch of TensorFold's native line, resolved from the
//! host's own approved document, spawned through its durable journal and
//! made ready by the adapter's native readiness read and probe, has its
//! owned process's listeners checked after the probe and on every re-proof
//! (ADR 0031 §7's check). The engine is a small Python stand-in with the
//! native 1.0 surfaces (`/health` `status`/`model_ids`/`max_batch_size`,
//! `/v1/models`, chat, `/metrics`) that can open a listener on `0.0.0.0`.
//! CPU and fake-engine tests only: nothing here qualifies TensorFold native
//! (the TF rows do).

use capyctl_agent::{
    identity_storage::IdentityDirectory, ingress::Ingress, ingress_identity::IngressIdentities,
    journal::HostJournal, native_execution::NativeHostExecution, session::SessionExecution,
};
use capyctl_config::remote_roles::HostConfig;
use capyctl_domain::group::{CommandIdentity, MemberKey};
use capyctl_protocol::execution::{MemberAction, MemberCommand, SingleLaunchPlan};
use serde_json::{json, Value};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

const BINDING: &str = "01K00000000000000000000001";
const INCARNATION: &str = "01K00000000000000000000002";
const GATE: [u8; 32] = [7; 32];

fn directory() -> tempfile::TempDir {
    let d = tempfile::Builder::new()
        .prefix("capyctl-tf-native-")
        .tempdir_in(std::env::var("HOME").unwrap())
        .unwrap();
    std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    d
}

fn private(path: &Path) -> PathBuf {
    std::fs::create_dir_all(path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path.to_owned()
}

fn python3() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("python3"))
        .find(|candidate| candidate.is_file())
        .expect("python3 on PATH for the fake engine")
}

/// A stand-in with TensorFold native 1.0's observable shape: `serve <model>
/// --name N --host 127.0.0.1 --port P ... --parallel L`; `/health` 200 with
/// `status: ok`, the served name in `model_ids` and `max_batch_size` the
/// rendered `--parallel` (no `ok`, no `busy`); `/v1/models`; chat; and the
/// `/metrics` gauges. With an `expose` file beside it at start it also
/// listens on `0.0.0.0` before serving; an `expose-later` file makes it do
/// so later. Either way it writes `exposed` once it listens there. It ends
/// itself if the test process or the fixture directory disappears.
const FAKE_TENSORFOLD_NATIVE: &str = r#"
import json, os, signal, socket, sys, threading, time, http.server
args = sys.argv[1:]
here = os.path.dirname(os.path.abspath(__file__))
with open(os.path.join(here, "record.json"), "w") as f:
    json.dump({"argv": args}, f)
port = int(args[args.index("--port") + 1])
name = args[args.index("--name") + 1]
lanes = int(args[args.index("--parallel") + 1]) if "--parallel" in args else 8
parent = os.getppid()
def orphaned():
    while os.getppid() == parent and os.path.isdir(here):
        time.sleep(0.5)
    os.killpg(0, signal.SIGKILL)
threading.Thread(target=orphaned, daemon=True).start()
held = []
def expose():
    s = socket.socket()
    s.bind(("0.0.0.0", 0))
    s.listen()
    held.append(s)
    open(os.path.join(here, "exposed"), "w").close()
if os.path.exists(os.path.join(here, "expose")):
    expose()
def later():
    while not os.path.exists(os.path.join(here, "expose-later")):
        time.sleep(0.05)
    expose()
threading.Thread(target=later, daemon=True).start()
class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass
    def send_body(self, ctype, body):
        self.send_response(200)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def do_GET(self):
        if self.path == "/health":
            self.send_body("application/json", json.dumps({
                "status": "ok", "model": name, "model_ids": [name], "max_batch_size": lanes,
                "warming": False, "memory": {"active": 0, "cache": 0, "peak": 0},
                "live": {"connections": 0, "waiting": 0, "decode_tokens_per_second": 0.0,
                         "prefill_tokens_per_second": 0.0}}).encode())
        elif self.path == "/v1/models":
            self.send_body("application/json", json.dumps({"object": "list", "data": [
                {"id": name, "object": "model", "created": 1, "owned_by": "tensorfold"}]}).encode())
        elif self.path == "/metrics":
            self.send_body("text/plain; version=0.0.4",
                b"tensorfold:requests_running 0\ntensorfold:requests_waiting 0\n"
                b"tensorfold:kv_cache_usage_ratio{pool=\"0\"} 0\n")
        else:
            self.send_response(404)
            self.end_headers()
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))) or b"{}")
        if self.path != "/v1/chat/completions" or body.get("model") != name:
            self.send_response(404)
            self.end_headers()
            return
        if body.get("stream"):
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            for delta, finish in (({"role": "assistant", "content": "ready"}, None), ({}, "stop")):
                chunk = {"id": "c1", "object": "chat.completion.chunk", "created": 1, "model": name,
                         "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}
                self.wfile.write(("data: " + json.dumps(chunk) + "\n\n").encode())
            self.wfile.write(b"data: [DONE]\n\n")
        else:
            self.send_body("application/json", json.dumps({
                "id": "c1", "object": "chat.completion", "created": 1, "model": name,
                "choices": [{"index": 0, "finish_reason": "stop",
                             "message": {"role": "assistant", "content": "ready"}}]}).encode())
http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
"#;

/// A loopback port for one stand-in engine, below the kernel's ephemeral
/// range and never handed out twice by this binary (as `native_vllm.rs`).
fn engine_port() -> u16 {
    use std::hash::{BuildHasher, Hasher};
    static TAKEN: std::sync::Mutex<Vec<u16>> = std::sync::Mutex::new(Vec::new());
    const LOW: u16 = 20_000;
    let high = std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range")
        .ok()
        .and_then(|text| text.split_whitespace().next()?.parse::<u16>().ok())
        .filter(|low| *low > LOW + 1_000)
        .unwrap_or(32_768);
    let mut taken = TAKEN.lock().unwrap_or_else(|error| error.into_inner());
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u32(std::process::id());
    let mut port = LOW + (hasher.finish() % u64::from(high - LOW)) as u16;
    for _ in 0..u32::from(high - LOW) {
        if !taken.contains(&port) && std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            taken.push(port);
            return port;
        }
        port = if port + 1 >= high { LOW } else { port + 1 };
    }
    panic!("no free loopback port below the ephemeral range");
}

struct Fixture {
    root: tempfile::TempDir,
    port: u16,
    config: HostConfig,
    deployment: Value,
}

impl Fixture {
    /// A host document whose `local` profile is TensorFold's native line
    /// (a release root's `bin/tensorfold-native` at 1.0.0, ADR 0039 §2) and
    /// a `restart_only` deployment of it on one leased port. The host has no
    /// private engine cache root: the native line needs none.
    fn new() -> Self {
        let root = directory();
        let path = root.path();
        let port = engine_port();
        let bin = path.join("tensorfold-1.0.0/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let engine = bin.join("tensorfold-native");
        capyctl_config::test_support::write_executable(
            &engine,
            format!("#!{}\n{FAKE_TENSORFOLD_NATIVE}", python3().display()),
            0o755,
        )
        .unwrap();
        std::fs::write(path.join("tensorfold-1.0.0/VERSION"), "1.0.0\n").unwrap();
        let runtime = private(&path.join("runtime"));
        let models = private(&path.join("models"));
        std::fs::create_dir_all(models.join("toy")).unwrap();
        std::fs::write(models.join("toy/config.json"), "{}").unwrap();

        let source: Value = serde_json::from_str(include_str!(
            "../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
        ))
        .unwrap();
        let mut host = source["input"]["host"].clone();
        let state = path.join("state");
        host["state_dir"] = json!(state);
        host["identity_dir"] = json!(state.join("identity"));
        host["runtime_dir"] = json!(runtime);
        host["model_store"]["path"] = json!(models);
        host["resource_policy"]["endpoint_port_range"] = json!({"start": port, "end": port});
        host["resource_policy"]["domains"]["unified"] = json!({
            "free_reserve": "16MiB", "host_kv_limit": "64MiB", "managed_limit": "1GiB",
            "memory": "unified", "parked_limit": "256MiB"
        });
        let profile = &mut host["runtime_profiles"]["local"];
        profile["engine"] = json!("tensorfold");
        profile["line"] = json!("native");
        profile["executable"] = json!(engine);
        profile["build_fingerprint"] = json!("1.0.0");
        profile["args"] = json!([]);
        profile["security"]["deep_park"] = json!("disabled");

        let mut deployment = source["input"]["deployment"].clone();
        deployment["model"]["path"] = json!(models.join("toy"));
        deployment["engine_config"] = json!({"context_length": 8192});
        deployment["residency"] = json!("restart_only");
        for (phase, bytes) in [
            ("cold", "256MiB"),
            ("ready", "128MiB"),
            ("parking", "128MiB"),
            ("parked", "32MiB"),
            ("wake", "256MiB"),
        ] {
            deployment["resources"][phase]["allocations"][0]["bytes"] = json!(bytes);
            deployment["resources"][phase]["allocations"][0]["host_kv_bytes"] = json!("0B");
        }
        let config = HostConfig::parse(&host.to_string()).unwrap();
        Self {
            root,
            port,
            config,
            deployment,
        }
    }

    fn bin(&self) -> PathBuf {
        self.root.path().join("tensorfold-1.0.0/bin")
    }

    fn launch(&self) -> MemberCommand {
        let local =
            capyctl_config::remote_resources::local_host_document(&self.config.document).unwrap();
        let effective =
            capyctl_config::effective::resolve_effective(&self.deployment, &local).unwrap();
        assert!(effective.profile.line.is_native());
        let location =
            capyctl_config::effective::checkpoint_location(&self.deployment, &local).unwrap();
        let checkpoint_digest = capyctl_agent::checkpoint::CheckpointVerifier::in_memory()
            .measure(&location.model_store, &location.checkpoint)
            .unwrap()
            .manifest
            .digest;
        sign(MemberCommand {
            identity: identity("launch", "reserved", &effective.profile.build_fingerprint),
            action: MemberAction::LaunchSingle(SingleLaunchPlan {
                deployment_config: self.deployment.to_string(),
                profile_name: "local".into(),
                checkpoint_fingerprint: effective.model.content_fingerprint.clone(),
                host_policy_fingerprint: capyctl_config::remote_resources::policy_fingerprint(
                    &self.config.document,
                ),
                binding_id: BINDING.into(),
                incarnation: INCARNATION.into(),
                grant_id: "01K00000000000000000000003".into(),
                issued_at_ms: capyctl_protocol::now_unix_ms(),
                coordinator_session_id: "01K00000000000000000000004".into(),
                service_port: self.port,
                checkpoint_digest,
                checkpoint_weights_bytes: None,
                checkpoint_state_slot_bytes: None,
                checkpoint_layout: None,
                checkpoint_tables: None,
                checkpoint_gguf: None,
                startup_bytes: None,
                startup_host_bytes: None,
                lmcache: None,
                devices: Vec::new(),
            }),
        })
    }

    fn probe(&self, launch: &MemberCommand, id: &str) -> MemberCommand {
        sign(MemberCommand {
            identity: identity(id, "ready", &launch.identity.profile_fingerprint),
            action: MemberAction::Probe {
                owned_handle: "launch".into(),
                max_tokens: None,
                decision: false,
            },
        })
    }
}

fn identity(id: &str, expected_state: &str, fingerprint: &str) -> CommandIdentity {
    CommandIdentity {
        controller_id: "controller".into(),
        member: MemberKey {
            host_id: "host".into(),
            member_id: "head".into(),
        },
        deployment_id: "deployment".into(),
        operation_id: "operation".into(),
        command_id: id.into(),
        step_id: id.into(),
        generation: 1,
        revision: 1,
        deadline_ms: capyctl_protocol::now_unix_ms() + 60_000,
        payload_digest: [0; 32],
        expected_state: expected_state.into(),
        profile_fingerprint: fingerprint.into(),
        instance_index: 0,
    }
}

fn sign(mut command: MemberCommand) -> MemberCommand {
    command.identity.payload_digest = command.canonical_digest();
    command
}

struct Host {
    /// Declared first so it runs first on drop: whatever this host launched
    /// is reaped before anything else of the host goes away.
    _reap: Reap,
    journal: Arc<HostJournal>,
    ingress: Arc<Ingress>,
    executor: Arc<NativeHostExecution>,
    session: u64,
}

fn host(fixture: &Fixture) -> Host {
    let path = fixture.root.path();
    let journal = HostJournal::open(&private(&path.join("journal")), "controller", "host").unwrap();
    let ingress = Ingress::new().unwrap();
    let identities = IngressIdentities::new(
        IdentityDirectory::open(&private(&path.join("ingress-identity"))).unwrap(),
    );
    let executor = NativeHostExecution::new(
        journal.clone(),
        ingress.clone(),
        identities,
        fixture.config.clone(),
        "host".into(),
        "controller".into(),
        fixture.config.runtime_dir.clone(),
        private(&path.join("logs")),
        Default::default(),
    );
    let session = journal.connect().unwrap();
    executor.connected(session).unwrap();
    Host {
        _reap: Reap(journal.clone()),
        journal,
        ingress,
        executor,
        session,
    }
}

/// Kills whatever the journal recorded, so a failing test leaves no
/// stand-in engine behind.
struct Reap(Arc<HostJournal>);
impl Drop for Reap {
    fn drop(&mut self) {
        use capyctl_adapters::traits::OwnedProcessLaunch;
        struct NoSpawn;
        impl capyctl_launchers::LaunchAssociation for NoSpawn {
            fn persist_api_identity(
                &self,
                _: &capyctl_domain::completion::ProcessIdentity,
            ) -> Result<(), capyctl_launchers::AssociationError> {
                panic!("cleanup cannot spawn")
            }
        }
        if let Ok(owned) = self.0.inspect_owned("launch") {
            let identities: Vec<_> = owned.into_iter().map(|(p, _)| p).collect();
            let tools = capyctl_launchers::DurableProcessLaunch::new(Arc::new(NoSpawn));
            let _ = tools.terminate_owned(&identities, Duration::from_millis(200));
        }
    }
}

/// Every process the journal recorded for the launch, gone.
fn all_gone(host: &Host) -> bool {
    use capyctl_domain::completion::Presence;
    let owned = host.journal.inspect_owned("launch").unwrap();
    !owned.is_empty()
        && owned
            .iter()
            .all(|(_, presence)| *presence == Presence::Gone)
}

// T41 T21 (ADR 0039 §14, ADR 0031 §7): a native launch whose owned process
// listens on 0.0.0.0 once its probe passed is stopped with gone evidence and
// fails `tensorfold_listener_exposed` naming the address; nothing is ready.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_native_launch_listening_off_loopback_fails_after_its_probe() {
    let fixture = Fixture::new();
    std::fs::write(fixture.bin().join("expose"), "").unwrap();
    let host = host(&fixture);
    let launch = fixture.launch();
    host.executor.provision(launch.clone(), GATE).await.unwrap();
    let failed = host
        .executor
        .execute(host.session, launch.clone())
        .await
        .unwrap();
    assert!(fixture.bin().join("exposed").exists(), "the stand-in ran");
    assert!(!failed.model_usable, "{failed:?}");
    assert!(
        failed.launch_failure.starts_with(&format!(
            "{}: an owned process listens on 0.0.0.0:",
            capyctl_config::tensorfold_native::LISTENER_EXPOSED
        )),
        "{failed:?}"
    );
    assert!(
        failed.launch_failure.ends_with("; the engine was stopped"),
        "{failed:?}"
    );
    assert!(capyctl_protocol::execution::is_launch_failure_text(
        &failed.launch_failure
    ));
    assert!(
        failed.processes.iter().all(|p| p.presence == "gone"),
        "{failed:?}"
    );
    assert!(all_gone(&host));
}

// T41 T21 (ADR 0039 §14): a native launch is ready when its owned process
// listens on loopback only (no engine cache root, ADR 0039 §2); once it
// opens a listener on 0.0.0.0, the host's next re-proof finds it after the
// probe, stops the launch with gone evidence and reports it not usable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_native_launch_exposed_after_readiness_is_stopped_on_its_reproof() {
    let fixture = Fixture::new();
    let host = host(&fixture);
    let launch = fixture.launch();
    host.executor.provision(launch.clone(), GATE).await.unwrap();
    let ready = host
        .executor
        .execute(host.session, launch.clone())
        .await
        .unwrap();
    assert!(ready.model_usable && ready.claim_retained, "{ready:?}");
    assert!(ready.processes.iter().all(|p| p.presence == "alive"));
    let record: Value =
        serde_json::from_slice(&std::fs::read(fixture.bin().join("record.json")).unwrap()).unwrap();
    let argv: Vec<String> = serde_json::from_value(record["argv"].clone()).unwrap();
    assert_eq!(argv[0], "serve");
    assert!(
        argv.windows(2).any(|w| w == ["--host", "127.0.0.1"]),
        "{argv:?}"
    );
    // ADR 0039 §11: the load reporter reads it by its `/metrics` alone.
    let targets = host.ingress.load_targets().unwrap();
    assert_eq!(
        targets[0].signal,
        capyctl_agent::load::LaunchSignal::TensorfoldNative
    );

    // A clean re-proof keeps it ready.
    let probe = fixture.probe(&launch, "probe-clean");
    let proven = host
        .executor
        .execute(host.session, probe.clone())
        .await
        .unwrap();
    capyctl_protocol::execution::validate_result(&probe, &proven).unwrap();
    assert!(proven.model_usable, "{proven:?}");

    std::fs::write(fixture.bin().join("expose-later"), "").unwrap();
    let by = Instant::now() + Duration::from_secs(10);
    while !fixture.bin().join("exposed").exists() {
        assert!(Instant::now() < by, "the stand-in opens its listener");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let probe = fixture.probe(&launch, "probe-exposed");
    let refused = host
        .executor
        .execute(host.session, probe.clone())
        .await
        .unwrap();
    capyctl_protocol::execution::validate_result(&probe, &refused).unwrap();
    assert!(!refused.model_usable, "{refused:?}");
    assert!(all_gone(&host));
}
