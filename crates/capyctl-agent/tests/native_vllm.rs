//! Remote single-rank vLLM on the host agent (SPEC §§3, 6.1, 9.1, 13.3).
//!
//! The agent resolves the launch from its own approved host document, spawns
//! through its durable journal, proves readiness with the adapter's model probe,
//! forwards routed inference through private ingress, and proves the owned group
//! gone on Terminate. The engine is a small Python stand-in that behaves like
//! vLLM's OpenAI surface (keyed `/v1`, a worker child, SSE chat). Nothing here
//! qualifies a native engine recipe: CPU and fake-engine tests never establish
//! that vLLM runs on a Spark.

use capyctl_agent::{
    identity_storage::IdentityDirectory,
    ingress::{Ingress, IngressScope},
    ingress_identity::IngressIdentities,
    journal::HostJournal,
    native_execution::NativeHostExecution,
    session::{Provisioned, SessionExecution},
};
use capyctl_config::remote_roles::HostConfig;
use capyctl_domain::group::{CommandIdentity, MemberKey};
use capyctl_protocol::execution::{MemberAction, MemberCommand, SingleLaunchPlan};
use serde_json::{json, Value};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
};

const BINDING: &str = "01K00000000000000000000001";
const INCARNATION: &str = "01K00000000000000000000002";
const GATE: [u8; 32] = [7; 32];

fn directory() -> tempfile::TempDir {
    let d = tempfile::Builder::new()
        .prefix("capyctl-vllm-remote-")
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

fn now_ms() -> i64 {
    capyctl_protocol::now_unix_ms()
}

/// A stand-in with vLLM's observable shape: `serve <model> --port N
/// --served-model-name R`, the key only from `VLLM_API_KEY`, every `/v1` route
/// keyed, one worker child, streamed chat. It records its argv and whether the
/// key arrived, never the key itself.
///
/// With development mode on it also models vLLM's level-2 residency surface
/// (W4): `/sleep?level=2` drops weights and KV, `/wake_up?tags=weights|kv_cache`
/// wakes one class, `/collective_rpc reload_weights` reloads weights, and
/// `/reset_prefix_cache`, `/is_sleeping` and the running/waiting gauges on
/// `/metrics` answer as vLLM's do. Chat answers only when weights are awake and
/// reloaded and KV is awake; `/v1/models` keeps listing the model while asleep,
/// as vLLM does, so only a chat probe proves a restoration. Control calls are
/// appended to `residency.log`; a `fail` file names one call to fail with 500,
/// and a `busy` file makes the running gauge read 1. The worker is reaped when
/// it dies, so a killed worker reads gone. The stand-in's group ends itself if
/// the test process or the fixture directory disappears, so a test killed
/// outright cannot leave an engine running.
const FAKE_VLLM: &str = r#"
import json, os, signal, sys, time, http.server, urllib.parse
args = sys.argv[1:]
here = os.path.dirname(os.path.abspath(__file__))
key = os.environ.get("VLLM_API_KEY", "")
admin = os.environ.get("CAPYCTL_VLLM_ADMIN_KEY", "")
with open(os.path.join(here, "record.json"), "w") as f:
    json.dump({"argv": args, "has_key": bool(key),
               "has_admin_key": bool(admin) and admin != key,
               "env": sorted(os.environ),
               "cuda_visible_devices": os.environ.get("CUDA_VISIBLE_DEVICES"),
               "cuda_device_order": os.environ.get("CUDA_DEVICE_ORDER"),
               "dev_mode": os.environ.get("VLLM_SERVER_DEV_MODE"),
               "pythonpath": os.environ.get("PYTHONPATH")}, f)
port = int(args[args.index("--port") + 1])
served = args[args.index("--served-model-name") + 1]
signal.signal(signal.SIGCHLD, signal.SIG_IGN)
# A test process that is killed outright (SIGKILL, a CI timeout) runs no Drop,
# and the launch is durable by design, so the stand-in ends itself instead once
# the test that owns it, or that test's directory, is gone. Capyctl's own cleanup
# always acts first while the test is alive.
parent = os.getppid()
def alive():
    return os.getppid() == parent and os.path.isdir(here)
if os.fork() == 0:
    while os.path.isdir(here):
        time.sleep(0.5)
    os._exit(0)
def orphaned():
    while alive():
        time.sleep(0.5)
    os.killpg(0, signal.SIGKILL)
import threading
threading.Thread(target=orphaned, daemon=True).start()
state = {"sleeping": False, "weights": True, "loaded": True, "kv": True}
def logged(call):
    with open(os.path.join(here, "residency.log"), "a") as f:
        f.write(call + "\n")
    try:
        with open(os.path.join(here, "fail")) as f:
            return f.read().strip() != call
    except FileNotFoundError:
        return True
def usable():
    return state["weights"] and state["loaded"] and state["kv"]

class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass
    def keyed(self):
        # The guard's split (SPEC §9.1): with an admin key, control routes take
        # it alone; inference and metrics take the inference key.
        path = urllib.parse.urlparse(self.path).path
        inference = path.startswith("/v1/") or path == "/metrics"
        want = key if (inference or not admin) else admin
        if not want or self.headers.get("Authorization") != "Bearer " + want:
            self.send_response(401); self.end_headers(); return False
        return True
    def send_json(self, value):
        body = json.dumps(value).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers(); self.wfile.write(body)
    def do_GET(self):
        if not self.keyed(): return
        if self.path == "/v1/models":
            self.send_json({"object": "list", "data": [{"id": served, "object": "model"}]})
        elif self.path == "/is_sleeping":
            self.send_json({"is_sleeping": state["sleeping"]})
        elif self.path == "/metrics":
            busy = 1 if os.path.exists(os.path.join(here, "busy")) else 0
            text = ("vllm:num_requests_running{engine=\"0\"} %d.0\n"
                    "vllm:num_requests_waiting{engine=\"0\"} 0.0\n" % busy).encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.send_header("Content-Length", str(len(text)))
            self.end_headers(); self.wfile.write(text)
        else:
            self.send_response(404); self.end_headers()
    def control(self, url):
        path, query = url.path, urllib.parse.parse_qs(url.query)
        if path == "/sleep":
            call = "sleep:" + query.get("level", [""])[0]
        elif path == "/wake_up":
            call = "wake:" + query.get("tags", [""])[0]
        elif path == "/collective_rpc":
            body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
            call = "collective:" + body.get("method", "")
        else:
            call = "reset_prefix_cache"
        if not logged(call):
            self.send_response(500); self.end_headers(); return
        if call == "sleep:2":
            state.update(sleeping=True, weights=False, loaded=False, kv=False)
        elif call == "sleep:1":
            # Level 1 offloads the weights to host RAM; the weights wake
            # copies them back, so they stay loaded.
            state.update(sleeping=True, weights=False, kv=False)
        elif call == "wake:weights":
            state["weights"] = True
        elif call == "wake:kv_cache":
            state["kv"] = True
        elif call == "collective:reload_weights" and state["weights"]:
            state["loaded"] = True
        elif call == "reset_prefix_cache":
            self.send_json({"success": True}); return
        else:
            self.send_response(409); self.end_headers(); return
        state["sleeping"] = not usable()
        self.send_json({})
    def do_POST(self):
        if not self.keyed(): return
        url = urllib.parse.urlparse(self.path)
        if url.path in ("/sleep", "/wake_up", "/collective_rpc", "/reset_prefix_cache"):
            self.control(url); return
        if self.path != "/v1/chat/completions":
            self.send_response(404); self.end_headers(); return
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
        if body.get("model") != served:
            self.send_response(404); self.end_headers(); return
        if not usable():
            self.send_response(503); self.end_headers(); return
        if body.get("stream"):
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            for delta, finish in (({"role": "assistant", "content": "fake-vllm-answer"}, None), ({}, "stop")):
                chunk = {"id": "c1", "object": "chat.completion.chunk", "created": 1, "model": served,
                         "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}
                self.wfile.write(("data: " + json.dumps(chunk) + "\n\n").encode())
            self.wfile.write(b"data: [DONE]\n\n")
        else:
            self.send_json({"id": "c1", "object": "chat.completion", "created": 1, "model": served,
                            "choices": [{"index": 0, "finish_reason": "stop",
                                         "message": {"role": "assistant", "content": "fake-vllm-answer"}}]})

http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
"#;

/// A loopback port for one stand-in engine, below the kernel's ephemeral
/// range and never handed out twice by this binary.
///
/// Final review M19: the port was taken from `127.0.0.1:0`, the ephemeral
/// range, and released before the engine bound it; under a parallel run an
/// outbound connection could take it as its local port first, the stand-in
/// failed to bind and exited before readiness, and whichever test owned it
/// failed (1 run in 4 to 10).
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
    /// `deep_park` writes the host switch; `guard` places capyctl's guard module in
    /// the host runtime directory the way a prepared host carries it.
    fn new(deep_park: bool, guard: bool) -> Self {
        Self::with_switch(Some(deep_park), guard)
    }

    /// `None` leaves `deep_park` out of the host document, so the ADR 0012
    /// default (enabled) applies; the deployment then asks for `deep`.
    fn with_switch(switch: Option<bool>, guard: bool) -> Self {
        Self::build(switch, guard, |_, _| {})
    }

    /// As [`Fixture::with_switch`], with the host document and the deployment
    /// edited before the host document is parsed.
    fn build(switch: Option<bool>, guard: bool, edit: impl FnOnce(&mut Value, &mut Value)) -> Self {
        let deep_park = switch.unwrap_or(true);
        let root = directory();
        let path = root.path();
        let port = engine_port();
        let bin = path.join("venv/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let engine = bin.join("vllm");
        capyctl_config::test_support::write_executable(
            &engine,
            format!("#!{}\n{FAKE_VLLM}", python3().display()),
            0o755,
        )
        .unwrap();
        // Owner decision Q11: the launch runs the installation's interpreter on
        // capyctl's protected entry. The stand-ins: the interpreter beside the fake
        // engine, and an entry that runs the fake engine in process (its
        // `__file__` stays the engine's, so the record lands beside it).
        std::os::unix::fs::symlink(python3(), bin.join("python3")).unwrap();
        let runtime = private(&path.join("runtime"));
        std::fs::write(
            runtime.join("vllm_entry.py"),
            format!(
                "import runpy\nrunpy.run_path({:?}, run_name=\"__main__\")\n",
                engine.display().to_string()
            ),
        )
        .unwrap();
        if guard {
            std::fs::write(runtime.join("capyctl_vllm_guard.py"), "# test guard\n").unwrap();
        }
        // ADR 0008: a sleep-mode launch also imports the capability probes.
        std::fs::write(
            runtime.join("engine_capabilities.py"),
            "# stand-in probes\n",
        )
        .unwrap();
        // SPEC §9.1 / T21: a prepared host's runtime modules are the agent
        // user's and not group- or other-writable, whatever the umask.
        for module in [
            "vllm_entry.py",
            "capyctl_vllm_guard.py",
            "engine_capabilities.py",
        ] {
            let path = runtime.join(module);
            if path.exists() {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            }
        }
        let models = private(&path.join("models"));
        std::fs::create_dir_all(models.join("toy")).unwrap();

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
        // A budget any CI host can satisfy; the agent still checks it against
        // the host's measured MemAvailable (SPEC §7, T29).
        host["resource_policy"]["domains"]["unified"] = json!({
            "free_reserve": "16MiB", "host_kv_limit": "64MiB", "managed_limit": "1GiB",
            "memory": "unified", "parked_limit": "256MiB"
        });
        let profile = &mut host["runtime_profiles"]["local"];
        profile["executable"] = json!(engine);
        match switch {
            Some(on) => {
                profile["security"]["deep_park"] = json!(if on { "enabled" } else { "disabled" })
            }
            None => {
                profile["security"]
                    .as_object_mut()
                    .unwrap()
                    .remove("deep_park");
            }
        }

        let mut deployment = source["input"]["deployment"].clone();
        deployment["model"]["path"] = json!(models.join("toy"));
        // ADR 0014 §2: the KV cache is the deployment's, inside its Ready phase.
        deployment["engine_config"]["memory"]["kv_cache"] = json!("64MiB");
        deployment["residency"] = json!(if deep_park { "deep" } else { "restart_only" });
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
        edit(&mut host, &mut deployment);
        let config = HostConfig::parse(&host.to_string()).unwrap();
        Self {
            root,
            port,
            config,
            deployment,
        }
    }

    fn launch(&self) -> MemberCommand {
        let local =
            capyctl_config::remote_resources::local_host_document(&self.config.document).unwrap();
        let effective =
            capyctl_config::effective::resolve_effective(&self.deployment, &local).unwrap();
        // ADR 0014 §7 (WE3): the server records the digest the host measures.
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
                issued_at_ms: now_ms(),
                coordinator_session_id: "01K00000000000000000000004".into(),
                service_port: self.port,
                checkpoint_digest,
                checkpoint_weights_bytes: None,
                checkpoint_state_slot_bytes: None,
                startup_bytes: None,
            }),
        })
    }

    /// ADR 0028 §2.1: the resolved engine env names this fixture's launch
    /// carries (the golden profile's `RUST_LOG`).
    fn resolved_env_names(&self) -> Vec<String> {
        let local =
            capyctl_config::remote_resources::local_host_document(&self.config.document).unwrap();
        let effective =
            capyctl_config::effective::resolve_effective(&self.deployment, &local).unwrap();
        effective.engine_env.values().into_keys().collect()
    }

    fn stop(&self, launch: &MemberCommand) -> MemberCommand {
        sign(MemberCommand {
            identity: identity("stop", "retained", &launch.identity.profile_fingerprint),
            action: MemberAction::Terminate {
                owned_handle: "launch".into(),
                recorded: Vec::new(),
            },
        })
    }

    fn record(&self) -> Option<Value> {
        std::fs::read(self.root.path().join("venv/bin/record.json"))
            .ok()
            .map(|bytes| serde_json::from_slice(&bytes).unwrap())
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
        deadline_ms: now_ms() + 60_000,
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

fn scope(command: &MemberCommand) -> IngressScope {
    IngressScope {
        host_id: "host".into(),
        member_id: "head".into(),
        deployment_id: "deployment".into(),
        binding_id: BINDING.into(),
        incarnation: INCARNATION.into(),
        generation: command.identity.generation,
        revision: command.identity.revision,
        instance_index: command.identity.instance_index,
    }
}

struct Host {
    /// Declared first so it runs first on drop: whatever this host launched is
    /// reaped before anything else of the host goes away, on every exit path.
    _reap: Option<Reap>,
    journal: Arc<HostJournal>,
    ingress: Arc<Ingress>,
    identities: Arc<IngressIdentities>,
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
        identities.clone(),
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
        _reap: Some(Reap(journal.clone())),
        journal,
        ingress,
        identities,
        executor,
        session,
    }
}

/// Kills whatever the journal recorded if an assertion fails mid-test, so a
/// failing run cannot leave a stand-in engine behind. `host()` installs it before
/// anything is launched; a guard installed by each test after its launch left the
/// engine running whenever a launch helper's own assertion failed first.
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
            let _ = tools.terminate_owned(&identities, std::time::Duration::from_millis(200));
        }
    }
}

fn files_contain(dir: &Path, needle: &[u8]) -> bool {
    std::fs::read_dir(dir).unwrap().any(|entry| {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files_contain(&path, needle)
        } else {
            std::fs::read(&path)
                .map(|bytes| bytes.windows(needle.len()).any(|w| w == needle))
                .unwrap_or(false)
        }
    })
}

/// The whole single-rank remote lifecycle for one deep-park setting.
async fn initialize_serve_and_stop(deep_park: bool) {
    let fixture = Fixture::new(deep_park, true);
    let host = host(&fixture);
    let launch = fixture.launch();

    host.executor.provision(launch.clone(), GATE).await.unwrap();
    let ready = host
        .executor
        .execute(host.session, launch.clone())
        .await
        .expect("the agent launches vLLM and proves the model usable");
    // SPEC §6.1: Ready only on the adapter's model probe, with the owned group.
    assert!(ready.model_usable && ready.claim_retained, "{ready:?}");
    assert_eq!(ready.state, "launched");
    assert!(ready.processes.len() >= 2, "{ready:?}");
    assert_eq!(ready.processes[0].role, "api");
    assert!(ready.processes.iter().all(|p| p.presence == "alive"));

    // SPEC §13.3: the rendered launch is the approved local profile on the
    // controller-leased port; the engine key reached the child's environment
    // only, never argv, the journal or the engine log.
    let record = fixture.record().expect("the engine recorded its launch");
    let argv: Vec<String> = serde_json::from_value(record["argv"].clone()).unwrap();
    let port = fixture.port.to_string();
    let toy = fixture.root.path().join("models/toy");
    assert_eq!(argv[0], "serve");
    assert_eq!(argv[1], toy.to_str().unwrap());
    for pair in [
        ["--host", "127.0.0.1"],
        ["--port", port.as_str()],
        ["--served-model-name", "toy"],
    ] {
        assert!(argv.windows(2).any(|w| w == pair), "{pair:?} in {argv:?}");
    }
    assert_eq!(record["has_key"], true);
    // SPEC §9.1 / T21: the development routes are keyed apart from inference.
    assert_eq!(record["has_admin_key"], true);
    // SPEC §13.3 / T21: the engine saw only the closed allowlist (the
    // interpreter itself may add a few of its own); ADR 0028 §2.1: the
    // resolved engine env's names join the fixed list, and reach the engine.
    let env: Vec<String> = serde_json::from_value(record["env"].clone()).unwrap();
    let resolved = fixture.resolved_env_names();
    assert!(!resolved.is_empty());
    for name in &env {
        assert!(
            capyctl_adapters::vllm::ENGINE_ENV_ALLOWLIST.contains(&name.as_str())
                || resolved.contains(name)
                || ["PWD", "SHLVL", "_", "LC_CTYPE"].contains(&name.as_str()),
            "{name} reached the engine"
        );
    }
    for name in &resolved {
        assert!(env.contains(name), "{name} missing from the engine");
    }
    assert!(env.iter().any(|n| n == "VLLM_PLUGINS"));
    let keys = host
        .identities
        .load(&scope(&launch), launch.identity.payload_digest)
        .unwrap();
    let native = hex::encode(keys.inference);
    assert!(!argv.iter().any(|a| a.contains(&native)));
    assert!(!argv.iter().any(|a| a == "--api-key"));
    let path = fixture.root.path();
    for dir in ["journal", "logs"] {
        assert!(!files_contain(&path.join(dir), native.as_bytes()), "{dir}");
        assert!(!files_contain(&path.join(dir), &keys.inference), "{dir}");
    }
    // SPEC §9.1 / T21: development mode, the guard middleware and its module
    // path exist only when the host enabled deep park.
    let middleware = argv
        .windows(2)
        .any(|w| w == ["--middleware", "capyctl_vllm_guard.RequireEngineKey"]);
    let runtime = fixture.config.runtime_dir.to_str().unwrap();
    if deep_park {
        assert_eq!(record["dev_mode"], "1");
        assert!(middleware && argv.iter().any(|a| a == "--enable-sleep-mode"));
        assert_eq!(record["pythonpath"].as_str(), Some(runtime));
    } else {
        assert_eq!(record["dev_mode"], "0");
        assert!(!middleware && !argv.iter().any(|a| a == "--enable-sleep-mode"));
    }

    // SPEC §10: routed inference crosses private ingress with the gate key and
    // reaches the engine with the per-launch native key.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = host.ingress.clone().router();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let client = reqwest::Client::new();
    let answer = client
        .post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(hex::encode(GATE))
        .json(&json!({"model":"toy","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(answer.status(), reqwest::StatusCode::OK);
    assert!(answer.text().await.unwrap().contains("fake-vllm-answer"));

    // Stop: the gate closes before termination and cleanup proves every owned
    // process gone before the claim is released (SPEC §13, T10).
    let stopped = host
        .executor
        .execute(host.session, fixture.stop(&launch))
        .await
        .unwrap();
    assert_eq!(stopped.state, "completed");
    assert!(!stopped.claim_retained);
    assert!(!stopped.processes.is_empty());
    assert!(
        stopped.processes.iter().all(|p| p.presence == "gone"),
        "{stopped:?}"
    );
    // SPEC §13.3 / T37: a launch proven gone keeps no credentials behind.
    assert!(host
        .identities
        .load(&scope(&launch), launch.identity.payload_digest)
        .is_err());
    let refused = client
        .post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(hex::encode(GATE))
        .json(&json!({"model":"toy","messages":[]}))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), reqwest::StatusCode::FORBIDDEN);
    server.abort();
}

/// G01: a remote host initializes, serves and stops a restart-only vLLM
/// deployment through the same journal, ingress and cleanup as SGLang.
// T10 T13 T37
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_vllm_initializes_serves_through_ingress_and_stops() {
    initialize_serve_and_stop(false).await;
}

/// SPEC §9.1: with the host's deep park enabled the agent launches vLLM in
/// development mode with capyctl's guard middleware loaded from its runtime dir.
// T21 T37
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_vllm_with_deep_park_loads_the_guard_middleware() {
    initialize_serve_and_stop(true).await;
}

/// A test that fails while its engine runs leaves no engine behind. Stand-ins
/// were found orphaned on control-host: `ready_deep_park` asserted readiness before its
/// callers installed their reap guard, so a failed readiness dropped the host
/// with nothing reaping it. The guard now comes with the host. The fixture and
/// this test process both outlive the failure here, so only capyctl's own
/// termination of the journaled group, not the stand-in's orphan exit, can end it.
// T12
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_test_that_fails_while_its_engine_runs_leaves_no_engine() {
    use capyctl_launchers::process_absence::{presence, Presence};
    let fixture = Arc::new(Fixture::new(true, true));
    let owned = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (inner, seen) = (fixture.clone(), owned.clone());
    let failed = tokio::spawn(async move {
        let host = host(&inner);
        let launch = inner.launch();
        host.executor.provision(launch.clone(), GATE).await.unwrap();
        host.executor.execute(host.session, launch).await.unwrap();
        *seen.lock().unwrap() = host
            .journal
            .inspect_owned("launch")
            .unwrap()
            .into_iter()
            .map(|(identity, _)| identity)
            .collect::<Vec<_>>();
        panic!("an assertion fails while the engine is running");
    })
    .await;
    assert!(failed.unwrap_err().is_panic());
    let owned = owned.lock().unwrap().clone();
    assert!(
        owned.len() >= 2,
        "the API process and its worker: {owned:?}"
    );
    for identity in &owned {
        assert_eq!(presence(identity), Presence::Gone, "{identity:?}");
    }
    assert!(fixture.root.path().is_dir());
}

/// SPEC §9.1 / §13.3: a development-mode launch whose host runtime directory
/// does not carry the guard module is refused before any durable effect, so no
/// engine can serve its control routes unguarded and no claim is left behind.
// T21 T37
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_vllm_without_the_guard_module_is_refused_before_launch() {
    let fixture = Fixture::new(true, false);
    let host = host(&fixture);
    let launch = fixture.launch();
    refused_launch(&host.executor, host.session, &launch).await;
    assert!(fixture.record().is_none(), "no engine may start");
    assert!(host.journal.history(0, 100).unwrap().is_empty());
}

/// ADR 0014 §6 / owner decision Q11: every vLLM launch runs through capyctl's
/// protected entry, deep park or not. A host runtime directory without it (or
/// with a symlink in its place) is refused before any durable effect.
// T14 T21
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_vllm_without_the_protected_entry_is_refused_before_launch() {
    for replace_with_symlink in [false, true] {
        let fixture = Fixture::new(false, true);
        let entry = fixture.root.path().join("runtime/vllm_entry.py");
        let elsewhere = fixture.root.path().join("elsewhere.py");
        std::fs::rename(&entry, &elsewhere).unwrap();
        if replace_with_symlink {
            std::os::unix::fs::symlink(&elsewhere, &entry).unwrap();
        }
        let host = host(&fixture);
        let launch = fixture.launch();
        refused_launch(&host.executor, host.session, &launch).await;
        assert!(fixture.record().is_none(), "no engine may start");
        assert!(host.journal.history(0, 100).unwrap().is_empty());
    }
}

/// SPEC §9.1, §13.3: the engine imports capyctl's guard and entry from the host
/// runtime directory. A module another account could rewrite, or a runtime
/// directory writable by other, is refused with the closed `runtime_integrity`
/// reason before any durable effect. (Group write through the owner's private
/// group is trusted since the 2026-09-22 owner decision; the unit tests in
/// `runtime_integrity` cover private and shared groups.)
// T21 T37
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_writable_runtime_module_is_refused_before_launch() {
    for (target, mode) in [
        ("runtime/capyctl_vllm_guard.py", 0o666),
        ("runtime/vllm_entry.py", 0o646),
        ("runtime", 0o777),
    ] {
        let fixture = Fixture::new(true, true);
        std::fs::set_permissions(
            fixture.root.path().join(target),
            std::fs::Permissions::from_mode(mode),
        )
        .unwrap();
        let host = host(&fixture);
        let launch = fixture.launch();
        refused_launch_for(&host.executor, host.session, &launch, "runtime_integrity").await;
        assert!(fixture.record().is_none(), "no engine may start");
        assert!(host.journal.history(0, 100).unwrap().is_empty());
    }
}

/// SPEC §13.3 / T37: credentials stored when a launch was provisioned are
/// deleted when its delivery is refused before any effect, so a refused launch
/// leaves no keys behind.
// T37 T21
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_launch_refused_after_provisioning_keeps_no_credentials() {
    let fixture = Fixture::new(true, true);
    let host = host(&fixture);
    let launch = fixture.launch();
    host.executor.provision(launch.clone(), GATE).await.unwrap();
    assert!(host
        .identities
        .load(&scope(&launch), launch.identity.payload_digest)
        .is_ok());
    // The host changes between provisioning and delivery.
    std::fs::set_permissions(
        fixture.root.path().join("runtime/vllm_entry.py"),
        std::fs::Permissions::from_mode(0o666),
    )
    .unwrap();
    let refused = host
        .executor
        .execute(host.session, launch.clone())
        .await
        .unwrap();
    assert_eq!(refused.refused, "runtime_integrity");
    assert!(fixture.record().is_none(), "no engine may start");
    assert!(host
        .identities
        .load(&scope(&launch), launch.identity.payload_digest)
        .is_err());
}

/// SPEC §9.1 / ADR 0012: default-on deep parking does not relax the guard. A
/// host document that omits `deep_park` gets development mode, and without the
/// guard module in its runtime directory that launch is still refused before
/// any durable effect.
// T21 T37
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_on_deep_park_without_the_guard_module_is_still_refused() {
    let fixture = Fixture::with_switch(None, false);
    let host = host(&fixture);
    let launch = fixture.launch();
    refused_launch(&host.executor, host.session, &launch).await;
    assert!(fixture.record().is_none(), "no engine may start");
    assert!(host.journal.history(0, 100).unwrap().is_empty());
}

/// SPEC §9.1 / ADR 0012: a remote host's explicit opt-out is honored on the
/// agent's own resolution: the deployment's parking residency is refused from
/// the host document the agent holds, whatever the controller sent.
// T21
#[test]
fn a_remote_host_opt_out_refuses_a_parking_deployment() {
    let mut fixture = Fixture::with_switch(Some(false), true);
    fixture.deployment["residency"] = json!("deep");
    let local =
        capyctl_config::remote_resources::local_host_document(&fixture.config.document).unwrap();
    let error = capyctl_config::effective::resolve_effective(&fixture.deployment, &local)
        .expect_err("the host opted out");
    assert_eq!(error.path, "runtime_profiles.security.deep_park");
}

impl Fixture {
    fn probe(&self, launch: &MemberCommand, id: &str) -> MemberCommand {
        sign(MemberCommand {
            identity: identity(id, "ready", &launch.identity.profile_fingerprint),
            action: MemberAction::Probe {
                owned_handle: "launch".into(),
                max_tokens: None,
            },
        })
    }
}

async fn serve(ingress: Arc<Ingress>) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = ingress.router();
    (
        address,
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() }),
    )
}

async fn chat(address: std::net::SocketAddr) -> reqwest::StatusCode {
    reqwest::Client::new()
        .post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(hex::encode(GATE))
        .json(&json!({"model":"toy","messages":[{"role":"user","content":"hi"}]}))
        .send()
        .await
        .unwrap()
        .status()
}

/// A host agent restart: the journal survives, the in-memory ingress gate and
/// readiness authority do not.
fn restarted(fixture: &Fixture, before: &Host) -> Host {
    let path = fixture.root.path();
    let ingress = Ingress::new().unwrap();
    let executor = NativeHostExecution::new(
        before.journal.clone(),
        ingress.clone(),
        before.identities.clone(),
        fixture.config.clone(),
        "host".into(),
        "controller".into(),
        fixture.config.runtime_dir.clone(),
        path.join("logs"),
        Default::default(),
    );
    before.executor.disconnected(before.session);
    let _ = before.journal.disconnect(before.session);
    let session = before.journal.connect().unwrap();
    executor.connected(session).unwrap();
    // The first host of this journal already reaps it.
    Host {
        _reap: None,
        journal: before.journal.clone(),
        ingress,
        identities: before.identities.clone(),
        executor,
        session,
    }
}

/// G2 (U5 live): after a host agent restart the retained engine is still
/// running, but no readiness survives the restart. The gate stays closed until a
/// fresh native probe of the exact journaled group passes, and that probe's
/// readiness belongs to the session that ran it.
// T33 T38 T13
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarted_host_serves_again_only_after_a_fresh_probe() {
    let fixture = Fixture::new(false, true);
    let first = host(&fixture);
    let launch = fixture.launch();
    first
        .executor
        .provision(launch.clone(), GATE)
        .await
        .unwrap();
    let ready = first
        .executor
        .execute(first.session, launch.clone())
        .await
        .unwrap();
    assert!(ready.model_usable, "{ready:?}");

    let host = restarted(&fixture, &first);
    let (address, server) = serve(host.ingress.clone()).await;
    assert_ne!(chat(address).await, reqwest::StatusCode::OK);

    // A probe that names this launch from another deployment proves nothing
    // and opens nothing, and one naming a launch never accepted observes nothing.
    let mut forged = fixture.probe(&launch, "probe-forged");
    forged.identity.deployment_id = "other-deployment".into();
    let forged = sign(forged);
    let refused = host
        .executor
        .execute(host.session, forged.clone())
        .await
        .unwrap();
    capyctl_protocol::execution::validate_result(&forged, &refused).unwrap();
    assert!(!refused.model_usable);
    assert_ne!(chat(address).await, reqwest::StatusCode::OK);
    let mut unknown = fixture.probe(&launch, "probe-unknown");
    unknown.action = MemberAction::Probe {
        owned_handle: "never-launched".into(),
        max_tokens: None,
    };
    let unknown = sign(unknown);
    let nothing = host
        .executor
        .execute(host.session, unknown.clone())
        .await
        .unwrap();
    assert!(!nothing.model_usable && !nothing.claim_retained && nothing.processes.is_empty());

    let probe = fixture.probe(&launch, "probe-1");
    let result = host
        .executor
        .execute(host.session, probe.clone())
        .await
        .unwrap();
    capyctl_protocol::execution::validate_result(&probe, &result).unwrap();
    assert!(result.model_usable && result.claim_retained, "{result:?}");
    assert_eq!(result.state, "completed");
    assert_eq!(result.owned_handle, "launch");
    assert_eq!(
        (result.binding_id.as_str(), result.incarnation.as_str()),
        (BINDING, INCARNATION)
    );
    let alive = |r: &capyctl_protocol::pb::MemberExecutionResult| {
        r.processes
            .iter()
            .filter(|p| p.presence == "alive")
            .map(|p| (p.role.clone(), p.pid, p.start_ticks))
            .collect::<Vec<_>>()
    };
    assert_eq!(alive(&result), alive(&ready));
    assert_eq!(chat(address).await, reqwest::StatusCode::OK);

    // A later session inherits nothing from this probe's readiness.
    let again = restarted(&fixture, &host);
    let replay = again
        .executor
        .execute(again.session, probe.clone())
        .await
        .unwrap();
    assert!(!replay.model_usable, "{replay:?}");
    server.abort();
    let stopped = again
        .executor
        .execute(again.session, fixture.stop(&launch))
        .await
        .unwrap();
    assert!(!stopped.claim_retained);
}

/// G2: if the engine is gone, or is not the journaled group, the probe proves
/// nothing. The result is ordinary retained evidence, the gate stays closed, and
/// the claim is kept until a Terminate proves the group gone.
// T33 T12 T32
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_probe_of_an_engine_that_died_keeps_the_gate_closed_and_the_claim() {
    use capyctl_adapters::traits::OwnedProcessLaunch;
    let fixture = Fixture::new(false, true);
    let first = host(&fixture);
    let launch = fixture.launch();
    first
        .executor
        .provision(launch.clone(), GATE)
        .await
        .unwrap();
    assert!(
        first
            .executor
            .execute(first.session, launch.clone())
            .await
            .unwrap()
            .model_usable
    );
    // The engine dies while the agent is down; nothing journals that.
    struct NoSpawn;
    impl capyctl_launchers::LaunchAssociation for NoSpawn {
        fn persist_api_identity(
            &self,
            _: &capyctl_domain::completion::ProcessIdentity,
        ) -> Result<(), capyctl_launchers::AssociationError> {
            panic!("cleanup cannot spawn")
        }
    }
    let owned: Vec<_> = first
        .journal
        .inspect_owned("launch")
        .unwrap()
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    capyctl_launchers::DurableProcessLaunch::new(Arc::new(NoSpawn))
        .terminate_owned(&owned, std::time::Duration::from_millis(500))
        .unwrap();

    let host = restarted(&fixture, &first);
    let (address, server) = serve(host.ingress.clone()).await;
    let probe = fixture.probe(&launch, "probe-dead");
    let result = host
        .executor
        .execute(host.session, probe.clone())
        .await
        .unwrap();
    capyctl_protocol::execution::validate_result(&probe, &result).unwrap();
    assert!(!result.model_usable, "{result:?}");
    assert!(result.claim_retained);
    assert!(result.processes.iter().all(|p| p.presence == "gone"));
    assert_ne!(chat(address).await, reqwest::StatusCode::OK);
    server.abort();
    // Terminate still settles it on the proof that the group is gone.
    let stopped = host
        .executor
        .execute(host.session, fixture.stop(&launch))
        .await
        .unwrap();
    assert!(!stopped.claim_retained);
    assert!(stopped.processes.iter().all(|p| p.presence == "gone"));
}

// ------------------------------------------------------------------------ W4
// Remote Park and Restore of a retained vLLM launch (SPEC §§6.1, 9.1, 10, 13).

impl Fixture {
    fn park(&self, launch: &MemberCommand, id: &str) -> MemberCommand {
        sign(MemberCommand {
            identity: identity(id, "ready", &launch.identity.profile_fingerprint),
            action: MemberAction::Park {
                owned_handle: "launch".into(),
            },
        })
    }

    fn restore(&self, launch: &MemberCommand, id: &str) -> MemberCommand {
        sign(MemberCommand {
            identity: identity(id, "parked", &launch.identity.profile_fingerprint),
            action: MemberAction::Restore {
                owned_handle: "launch".into(),
                checkpoint_digest: String::new(),
            },
        })
    }

    /// The engine control calls the stand-in received, in order.
    fn controls(&self) -> Vec<String> {
        std::fs::read_to_string(self.root.path().join("venv/bin/residency.log"))
            .map(|log| log.lines().map(str::to_string).collect())
            .unwrap_or_default()
    }

    fn engine_file(&self, name: &str, contents: Option<&str>) {
        let path = self.root.path().join("venv/bin").join(name);
        match contents {
            Some(contents) => std::fs::write(path, contents).unwrap(),
            None => std::fs::remove_file(path).unwrap(),
        }
    }
}

/// A launched, ready deep-park vLLM on a fresh host.
async fn ready_deep_park() -> (Fixture, Host, MemberCommand) {
    let fixture = Fixture::new(true, true);
    let host = host(&fixture);
    let launch = fixture.launch();
    host.executor.provision(launch.clone(), GATE).await.unwrap();
    let ready = host
        .executor
        .execute(host.session, launch.clone())
        .await
        .unwrap();
    assert!(ready.model_usable, "{ready:?}");
    (fixture, host, launch)
}

fn residency(result: &capyctl_protocol::pb::MemberExecutionResult) -> &str {
    result
        .residency
        .as_ref()
        .map(|r| r.state.as_str())
        .unwrap_or("none")
}

fn milestones(result: &capyctl_protocol::pb::MemberExecutionResult) -> Vec<String> {
    result.residency.clone().unwrap_or_default().milestones
}

/// SPEC §13: a Park or Restore host policy refused before journaling it is a
/// terminal `unchanged` answer with a closed reason, never a lost session.
fn refused_unchanged(
    command: &MemberCommand,
    result: &capyctl_protocol::pb::MemberExecutionResult,
) -> String {
    capyctl_protocol::execution::validate_result(command, result).unwrap();
    assert_eq!(residency(result), "unchanged", "{result:?}");
    assert!(milestones(result).is_empty() && !result.model_usable);
    result.refused.clone()
}

/// SPEC §13: a launch host policy refuses before any effect is answered, not
/// dropped. The provision stores nothing and says why, and a delivery of the
/// launch completes refused with no claim and no process.
async fn refused_launch(executor: &NativeHostExecution, session: u64, launch: &MemberCommand) {
    refused_launch_for(executor, session, launch, "unauthorized").await
}

async fn refused_launch_for(
    executor: &NativeHostExecution,
    session: u64,
    launch: &MemberCommand,
    reason: &'static str,
) {
    assert_eq!(
        executor.provision(launch.clone(), GATE).await.unwrap(),
        Provisioned::Refused(reason)
    );
    let refused = executor.execute(session, launch.clone()).await.unwrap();
    capyctl_protocol::execution::validate_result(launch, &refused).unwrap();
    assert_eq!(refused.refused, reason);
    assert!(refused.state == "completed" && !refused.claim_retained && !refused.model_usable);
    assert!(refused.processes.is_empty());
}

/// W4: a ready deep-park launch parks in place and is restored in place. The
/// park closes the gate before the sleep, keeps the claim and the same process
/// group, and never claims a usable model; the parked state survives a host
/// restart and blocks a probe; the restore runs weights wake, `reload_weights`,
/// KV wake and prefix reset in order and reopens the gate only after a fresh
/// model probe. Replays are answered from the journal, never re-executed.
// T16 T15 T33 T34 T13
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deep_park_launch_parks_and_restores_in_place() {
    let (fixture, first, launch) = ready_deep_park().await;
    let (address, server) = serve(first.ingress.clone()).await;
    assert_eq!(chat(address).await, reqwest::StatusCode::OK);
    let group = |r: &capyctl_protocol::pb::MemberExecutionResult| {
        r.processes
            .iter()
            .filter(|p| p.presence == "alive")
            .map(|p| (p.role.clone(), p.pid, p.boot_id.clone(), p.start_ticks))
            .collect::<std::collections::BTreeSet<_>>()
    };
    let launched = first
        .executor
        .execute(first.session, launch.clone())
        .await
        .unwrap();

    let park = fixture.park(&launch, "park-1");
    let parked = first
        .executor
        .execute(first.session, park.clone())
        .await
        .unwrap();
    capyctl_protocol::execution::validate_result(&park, &parked).unwrap();
    assert_eq!(
        (parked.state.as_str(), residency(&parked)),
        ("completed", "parked"),
        "{parked:?}"
    );
    assert!(parked.claim_retained && !parked.model_usable);
    assert_eq!(parked.owned_handle, "launch");
    assert_eq!(
        (parked.binding_id.as_str(), parked.incarnation.as_str()),
        (BINDING, INCARNATION)
    );
    // SPEC §13.2: the retained group is the journaled one, unchanged.
    assert_eq!(group(&parked), group(&launched));
    assert_eq!(
        milestones(&parked),
        [
            "gate_closed",
            "ingress_idle",
            "engine_quiescent",
            "memory_released",
            "identity_unchanged"
        ]
    );
    let evidence = parked.residency.clone().unwrap();
    assert!(evidence.mem_available_before_bytes > 0 && evidence.mem_available_after_bytes > 0);
    assert_eq!(fixture.controls(), ["sleep:2"]);
    assert_eq!(
        first.journal.residency_of("launch").unwrap().as_deref(),
        Some("parked")
    );
    // The gate closed before the sleep and stays closed while parked.
    assert_ne!(chat(address).await, reqwest::StatusCode::OK);

    // Dedupe: the same command is answered from the journal, not re-slept.
    let replay = first
        .executor
        .execute(first.session, park.clone())
        .await
        .unwrap();
    capyctl_protocol::execution::validate_result(&park, &replay).unwrap();
    assert_eq!(residency(&replay), "parked");
    assert_eq!(fixture.controls(), ["sleep:2"]);
    // A launch replay no longer claims the model usable.
    assert!(
        !first
            .executor
            .execute(first.session, launch.clone())
            .await
            .unwrap()
            .model_usable
    );
    // A second park of a parked launch is refused before it is journaled.
    let again = fixture.park(&launch, "park-2");
    let refused = first
        .executor
        .execute(first.session, again.clone())
        .await
        .unwrap();
    assert_eq!(refused_unchanged(&again, &refused), "unauthorized");
    server.abort();

    // The parked state is durable: a restarted host still refuses a probe.
    let host = restarted(&fixture, &first);
    let (address, server) = serve(host.ingress.clone()).await;
    let probe = fixture.probe(&launch, "probe-parked");
    let probed = host
        .executor
        .execute(host.session, probe.clone())
        .await
        .unwrap();
    capyctl_protocol::execution::validate_result(&probe, &probed).unwrap();
    assert!(!probed.model_usable && probed.claim_retained);
    assert_ne!(chat(address).await, reqwest::StatusCode::OK);

    let restore = fixture.restore(&launch, "restore-1");
    let restored = host
        .executor
        .execute(host.session, restore.clone())
        .await
        .unwrap();
    capyctl_protocol::execution::validate_result(&restore, &restored).unwrap();
    assert_eq!(residency(&restored), "restored", "{restored:?}");
    assert!(restored.model_usable && restored.claim_retained);
    assert_eq!(group(&restored), group(&launched));
    assert_eq!(
        milestones(&restored),
        // ADR 0014 §7 (WE3): the checkpoint is verified before any wake call.
        [
            "gate_closed",
            "checkpoint_verified",
            "allocations_restored",
            "weights_usable",
            "cache_valid",
            "model_usable",
            "identity_unchanged"
        ]
    );
    assert_eq!(
        fixture.controls(),
        [
            "sleep:2",
            "wake:weights",
            "collective:reload_weights",
            "wake:kv_cache",
            "reset_prefix_cache"
        ]
    );
    assert_eq!(host.journal.residency_of("launch").unwrap(), None);
    assert_eq!(chat(address).await, reqwest::StatusCode::OK);
    // A replayed restore never repeats the collective.
    let again = host
        .executor
        .execute(host.session, restore.clone())
        .await
        .unwrap();
    capyctl_protocol::execution::validate_result(&restore, &again).unwrap();
    assert_eq!(fixture.controls().len(), 5);
    server.abort();

    let stopped = host
        .executor
        .execute(host.session, fixture.stop(&launch))
        .await
        .unwrap();
    assert!(!stopped.claim_retained);
}

/// Owner decision (ADR 0012): `restart_only` never parks. The Park is refused
/// before anything is journaled and no sleep reaches the engine.
// T21 T34
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_only_launch_is_never_parked() {
    let fixture = Fixture::new(false, true);
    let host = host(&fixture);
    let launch = fixture.launch();
    host.executor.provision(launch.clone(), GATE).await.unwrap();
    assert!(
        host.executor
            .execute(host.session, launch.clone())
            .await
            .unwrap()
            .model_usable
    );
    let park = fixture.park(&launch, "park");
    let refused = host
        .executor
        .execute(host.session, park.clone())
        .await
        .unwrap();
    assert_eq!(refused_unchanged(&park, &refused), "residency_tier");
    assert!(fixture.controls().is_empty());
    assert_eq!(host.journal.history(0, 100).unwrap().len(), 1);
    assert_eq!(host.journal.residency_of("launch").unwrap(), None);
}

/// Discrete GPU design §5: a `host_backed` launch (distinct host and device
/// memory) is admitted to Park, sleeps at level 1, and is restored by the
/// weights wake with no `reload_weights` collective, then the KV wake, the
/// prefix-cache reset and a fresh model probe.
// T16 T20 T21
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_backed_launch_parks_at_level_one_and_restores_without_a_reload() {
    let fixture = Fixture::build(Some(true), true, |host, deployment| {
        // ADR 0010 decision 5: host_backed needs distinct pools.
        host["resource_policy"]["domains"]["unified"]["memory"] = json!("distinct");
        deployment["residency"] = json!("host_backed");
    });
    let host = host(&fixture);
    let launch = fixture.launch();
    host.executor.provision(launch.clone(), GATE).await.unwrap();
    assert!(
        host.executor
            .execute(host.session, launch.clone())
            .await
            .unwrap()
            .model_usable
    );
    let park = fixture.park(&launch, "park");
    let parked = host
        .executor
        .execute(host.session, park.clone())
        .await
        .unwrap();
    capyctl_protocol::execution::validate_result(&park, &parked).unwrap();
    assert_eq!(residency(&parked), "parked", "{parked:?}");
    assert_eq!(fixture.controls(), ["sleep:1"]);
    let restore = fixture.restore(&launch, "restore");
    let restored = host
        .executor
        .execute(host.session, restore.clone())
        .await
        .unwrap();
    capyctl_protocol::execution::validate_result(&restore, &restored).unwrap();
    assert_eq!(residency(&restored), "restored", "{restored:?}");
    assert!(restored.model_usable && restored.claim_retained);
    assert_eq!(
        fixture.controls(),
        [
            "sleep:1",
            "wake:weights",
            "wake:kv_cache",
            "reset_prefix_cache"
        ]
    );
    let stopped = host
        .executor
        .execute(host.session, fixture.stop(&launch))
        .await
        .unwrap();
    assert!(!stopped.claim_retained);
}

/// SPEC §10 step 4: work the engine still reports is not quiescence. The park
/// is refused without a sleep and the launch stays resident; a Restore of it is
/// refused. SPEC §§9.1, 13.2: a refusal is `unchanged`, so the gate the park
/// attempt closed reopens for the same unchanged group in the same session;
/// the launch keeps serving without waiting for a probe.
// T16 T20
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_busy_engine_is_not_parked() {
    let (fixture, host, launch) = ready_deep_park().await;
    let (address, server) = serve(host.ingress.clone()).await;
    fixture.engine_file("busy", Some("1"));
    let park = fixture.park(&launch, "park-busy");
    let refused = host
        .executor
        .execute(host.session, park.clone())
        .await
        .unwrap();
    capyctl_protocol::execution::validate_result(&park, &refused).unwrap();
    assert_eq!(residency(&refused), "unchanged", "{refused:?}");
    assert_eq!(milestones(&refused), ["gate_closed", "ingress_idle"]);
    assert!(refused.claim_retained && !refused.model_usable);
    assert!(fixture.controls().is_empty(), "no sleep may be sent");
    assert_eq!(host.journal.residency_of("launch").unwrap(), None);
    // The refusal left the launch as it was: ready, and forwarding again.
    assert_eq!(chat(address).await, reqwest::StatusCode::OK);
    let restore = fixture.restore(&launch, "restore");
    let refused = host
        .executor
        .execute(host.session, restore.clone())
        .await
        .unwrap();
    assert_eq!(refused_unchanged(&restore, &refused), "unauthorized");
    assert_eq!(chat(address).await, reqwest::StatusCode::OK);
    fixture.engine_file("busy", None);
    let probe = fixture.probe(&launch, "probe-after");
    assert!(
        host.executor
            .execute(host.session, probe)
            .await
            .unwrap()
            .model_usable
    );
    assert_eq!(chat(address).await, reqwest::StatusCode::OK);
    server.abort();
}

/// SPEC §13.2 / T20: a restoration that fails after an engine call is
/// uncertain. The claim is retained, the gate stays closed, the collective is
/// never repeated, no further Park, Restore or probe is admitted, and only a
/// Terminate settles the launch.
// T20 T32 T12
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_restore_is_quarantined_until_terminate() {
    let (fixture, host, launch) = ready_deep_park().await;
    let (address, server) = serve(host.ingress.clone()).await;
    let parked = host
        .executor
        .execute(host.session, fixture.park(&launch, "park"))
        .await
        .unwrap();
    assert_eq!(residency(&parked), "parked");
    fixture.engine_file("fail", Some("collective:reload_weights"));
    let restore = fixture.restore(&launch, "restore-1");
    let failed = host
        .executor
        .execute(host.session, restore.clone())
        .await
        .unwrap();
    capyctl_protocol::execution::validate_result(&restore, &failed).unwrap();
    assert_eq!(residency(&failed), "unknown", "{failed:?}");
    assert_eq!(
        milestones(&failed),
        ["gate_closed", "checkpoint_verified", "allocations_restored"]
    );
    assert!(failed.claim_retained && !failed.model_usable);
    assert_eq!(
        host.journal.residency_of("launch").unwrap().as_deref(),
        Some("uncertain")
    );
    assert_ne!(chat(address).await, reqwest::StatusCode::OK);
    fixture.engine_file("fail", None);
    for command in [
        fixture.restore(&launch, "restore-2"),
        fixture.park(&launch, "park-2"),
    ] {
        let refused = host
            .executor
            .execute(host.session, command.clone())
            .await
            .unwrap();
        assert_eq!(refused_unchanged(&command, &refused), "unauthorized");
    }
    let probe = fixture.probe(&launch, "probe");
    assert!(
        !host
            .executor
            .execute(host.session, probe)
            .await
            .unwrap()
            .model_usable
    );
    assert_eq!(
        fixture.controls(),
        ["sleep:2", "wake:weights", "collective:reload_weights"],
        "the collective is never repeated"
    );
    server.abort();
    let stopped = host
        .executor
        .execute(host.session, fixture.stop(&launch))
        .await
        .unwrap();
    assert!(!stopped.claim_retained);
    assert!(stopped.processes.iter().all(|p| p.presence == "gone"));
    assert_eq!(host.journal.residency_of("launch").unwrap(), None);
}

/// ADR 0014 §7 (WE3): waking reloads weights from disk, so a checkpoint
/// changed under a parked engine is refused before any wake call; the launch
/// stays parked and owned. With the recorded bytes back, it wakes.
// T34 T16
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_checkpoint_changed_while_parked_is_not_woken() {
    let (fixture, host, launch) = ready_deep_park().await;
    let parked = host
        .executor
        .execute(host.session, fixture.park(&launch, "park"))
        .await
        .unwrap();
    assert_eq!(residency(&parked), "parked");
    let swapped = fixture.root.path().join("models/toy/model.safetensors");
    std::fs::write(&swapped, "substituted weights").unwrap();
    let restore = fixture.restore(&launch, "restore-1");
    let refused = host
        .executor
        .execute(host.session, restore.clone())
        .await
        .unwrap();
    capyctl_protocol::execution::validate_result(&restore, &refused).unwrap();
    assert_eq!(residency(&refused), "unchanged", "{refused:?}");
    assert_eq!(milestones(&refused), ["gate_closed"]);
    assert!(refused.claim_retained && !refused.model_usable);
    assert_eq!(
        fixture.controls(),
        ["sleep:2"],
        "no wake reached the engine"
    );
    assert_eq!(
        host.journal.residency_of("launch").unwrap().as_deref(),
        Some("parked")
    );
    std::fs::remove_file(&swapped).unwrap();
    let restore = fixture.restore(&launch, "restore-2");
    let restored = host
        .executor
        .execute(host.session, restore.clone())
        .await
        .unwrap();
    capyctl_protocol::execution::validate_result(&restore, &restored).unwrap();
    assert_eq!(residency(&restored), "restored", "{restored:?}");
    let stopped = host
        .executor
        .execute(host.session, fixture.stop(&launch))
        .await
        .unwrap();
    assert!(!stopped.claim_retained);
}

/// SPEC §13.2: a group that is no longer the journaled one (a worker died) is
/// not parked. The Park completes `unchanged` without a sleep, and reports the
/// group as it now is.
// T33 T12
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_changed_group_is_not_parked() {
    let (fixture, host, launch) = ready_deep_park().await;
    let ready = host
        .executor
        .execute(host.session, launch.clone())
        .await
        .unwrap();
    let worker = ready
        .processes
        .iter()
        .find(|p| p.role == "worker-0")
        .unwrap()
        .pid;
    unsafe { libc::kill(worker as i32, libc::SIGKILL) };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while host
        .journal
        .inspect_owned("launch")
        .unwrap()
        .iter()
        .any(|(p, presence)| {
            p.pid == worker && *presence == capyctl_domain::completion::Presence::Alive
        })
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the worker must be reaped"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let park = fixture.park(&launch, "park");
    let refused = host
        .executor
        .execute(host.session, park.clone())
        .await
        .unwrap();
    capyctl_protocol::execution::validate_result(&park, &refused).unwrap();
    assert_eq!(residency(&refused), "unchanged", "{refused:?}");
    assert!(refused.claim_retained && !refused.model_usable);
    assert!(refused
        .processes
        .iter()
        .any(|p| p.pid == worker && p.presence == "gone"));
    assert!(fixture.controls().is_empty());
    assert_eq!(host.journal.residency_of("launch").unwrap(), None);
}

/// SPEC §13.1 / T34: Park is authorized only from `ready`, Restore only from
/// `parked`, and neither names a launch of another deployment or one this host
/// never accepted.
// T34
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn park_and_restore_are_fenced_by_expected_state_and_owner() {
    let (fixture, host, launch) = ready_deep_park().await;
    let mut wrong_state = fixture.park(&launch, "park-retained");
    wrong_state.identity.expected_state = "retained".into();
    let mut foreign = fixture.park(&launch, "park-foreign");
    foreign.identity.deployment_id = "other-deployment".into();
    let mut unknown = fixture.park(&launch, "park-unknown");
    unknown.action = MemberAction::Park {
        owned_handle: "never-launched".into(),
    };
    let restore_resident = fixture.restore(&launch, "restore-resident");
    for command in [
        sign(wrong_state),
        sign(foreign),
        sign(unknown),
        restore_resident,
    ] {
        let refused = host
            .executor
            .execute(host.session, command.clone())
            .await
            .unwrap();
        assert_eq!(refused_unchanged(&command, &refused), "unauthorized");
    }
    assert!(fixture.controls().is_empty());
    assert_eq!(host.journal.history(0, 100).unwrap().len(), 1);
}

/// Owner decision 5 (2026-09-22): a Restore may carry the digest the server
/// recorded. The host wakes against the plan's digest; a Restore naming any
/// other digest is refused `unchanged` before any wake call and the launch
/// stays parked, while one naming the same digest wakes. (A plan journaled
/// before WE3, with no digest of its own, is woken against the one sent; the
/// executor's unit test covers that choice.)
// T14 T15 T34
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restore_naming_another_digest_is_not_woken() {
    let (fixture, host, launch) = ready_deep_park().await;
    let parked = host
        .executor
        .execute(host.session, fixture.park(&launch, "park"))
        .await
        .unwrap();
    assert_eq!(residency(&parked), "parked");
    let MemberAction::LaunchSingle(plan) = &launch.action else {
        unreachable!()
    };
    let restore_with = |id: &str, digest: String| {
        let mut command = fixture.restore(&launch, id);
        command.action = MemberAction::Restore {
            owned_handle: "launch".into(),
            checkpoint_digest: digest,
        };
        sign(command)
    };
    let other = restore_with("restore-other", format!("sha256:{}", "0".repeat(64)));
    let refused = host
        .executor
        .execute(host.session, other.clone())
        .await
        .unwrap();
    capyctl_protocol::execution::validate_result(&other, &refused).unwrap();
    assert_eq!(residency(&refused), "unchanged", "{refused:?}");
    assert_eq!(milestones(&refused), ["gate_closed"]);
    assert_eq!(
        fixture.controls(),
        ["sleep:2"],
        "no wake reached the engine"
    );
    assert_eq!(
        host.journal.residency_of("launch").unwrap().as_deref(),
        Some("parked")
    );
    let same = restore_with("restore-same", plan.checkpoint_digest.clone());
    let restored = host
        .executor
        .execute(host.session, same.clone())
        .await
        .unwrap();
    capyctl_protocol::execution::validate_result(&same, &restored).unwrap();
    assert_eq!(residency(&restored), "restored", "{restored:?}");
    let stopped = host
        .executor
        .execute(host.session, fixture.stop(&launch))
        .await
        .unwrap();
    assert!(!stopped.claim_retained);
}

/// M16 live run (2026-09-23, host-b): an engine that exits before
/// readiness is reported to the controller as a result, promptly, with the
/// model unusable, the claim retained and the recorded process gone. Before
/// the fix the agent ended its control session instead, so the controller
/// waited out the whole Initialize deadline and every other effect on the
/// host was torn down with the session. Terminate then settles it on gone
/// evidence (SPEC §§6.1, 13.2).
// T10 T13
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_engine_that_exits_before_readiness_is_reported_not_a_lost_session() {
    let fixture = Fixture::new(false, true);
    let entry = fixture.config.runtime_dir.join("vllm_entry.py");
    std::fs::write(&entry, "import sys\nsys.exit(3)\n").unwrap();
    std::fs::set_permissions(&entry, std::fs::Permissions::from_mode(0o644)).unwrap();
    let host = host(&fixture);
    let launch = fixture.launch();
    host.executor.provision(launch.clone(), GATE).await.unwrap();

    let started = std::time::Instant::now();
    let result = host
        .executor
        .execute(host.session, launch.clone())
        .await
        .expect("a failed launch is a result, not a lost session");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "reported after {:?}, the command deadline is 60 s",
        started.elapsed()
    );
    capyctl_protocol::execution::validate_result(&launch, &result).unwrap();
    assert!(!result.model_usable, "{result:?}");
    assert!(result.claim_retained, "{result:?}");
    assert!(!result.processes.is_empty(), "{result:?}");
    assert!(
        result.processes.iter().all(|p| p.presence == "gone"),
        "{result:?}"
    );
    assert!(fixture.record().is_none(), "the engine never ran");

    let stopped = host
        .executor
        .execute(host.session, fixture.stop(&launch))
        .await
        .unwrap();
    assert_eq!(stopped.state, "completed");
    assert!(!stopped.claim_retained, "{stopped:?}");
    assert!(stopped.processes.iter().all(|p| p.presence == "gone"));
}

/// SPEC §13.2 (W13) / T20 T33: a parked engine that loses a member is
/// reported too, so the controller never wakes a group that is no longer the
/// one it parked. Report only: the claim and the parked state stay until a
/// Terminate settles the launch.
// T20 T33
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_parked_launch_that_loses_a_member_is_reported() {
    let (fixture, host, launch) = ready_deep_park().await;
    let parked = host
        .executor
        .execute(host.session, fixture.park(&launch, "park"))
        .await
        .unwrap();
    assert_eq!(residency(&parked), "parked", "{parked:?}");
    let worker = parked
        .processes
        .iter()
        .find(|p| p.role == "worker-0")
        .unwrap()
        .pid;
    unsafe { libc::kill(worker as i32, libc::SIGKILL) };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let exit = loop {
        let exited =
            capyctl_agent::exits::scan(&host.journal, "host", capyctl_protocol::now_unix_ms());
        if let Some(launch) = exited.into_iter().next() {
            break launch.exit;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no exit reported for a parked launch"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert_eq!(exit.owned_handle, "launch");
    assert_eq!(exit.process.pid, worker);
    assert_eq!(
        host.journal.residency_of("launch").unwrap().as_deref(),
        Some("parked")
    );
}

const GPU0_UUID: &str = "GPU-00000000-0000-0000-0000-000000000000";
const GPU1_UUID: &str = "GPU-11111111-1111-1111-1111-111111111111";

/// A host with two GPUs, each published with its physical UUID, and a
/// deployment document naming `device` (what the server sends for the GPU
/// placement chose).
fn two_gpus_launching_on(device: &'static str) -> Fixture {
    two_gpus_named(
        json!({
            "gpu0": {"domain": "unified", "sharing": "shared", "physical_gpu_uuid": GPU0_UUID},
            "gpu1": {"domain": "unified", "sharing": "shared", "physical_gpu_uuid": GPU1_UUID}
        }),
        device,
    )
}

/// A host with the GPUs `devices` declares, launching on `device`.
fn two_gpus_named(devices: Value, device: &'static str) -> Fixture {
    Fixture::build(Some(false), true, move |host, deployment| {
        host["resource_policy"]["devices"] = devices;
        name_device(deployment, device);
    })
}

fn name_device(deployment: &mut Value, device: &str) {
    let claim = json!([{"id": device, "sharing": "shared"}]);
    deployment["devices"] = claim.clone();
    for phase in ["cold", "ready", "parking", "wake"] {
        deployment["resources"][phase]["devices"] = claim.clone();
    }
}

/// Discrete GPU design §7: the launch names the GPU placement chose, and the
/// agent sets the engine child's `CUDA_VISIBLE_DEVICES` to that GPU's
/// physical UUID from its own approved policy, so the engine sees only it.
// T27 T21
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_chosen_gpu_reaches_the_engine_by_its_published_uuid() {
    let fixture = two_gpus_launching_on("gpu1");
    let host = host(&fixture);
    let launch = fixture.launch();
    host.executor.provision(launch.clone(), GATE).await.unwrap();
    let ready = host
        .executor
        .execute(host.session, launch.clone())
        .await
        .expect("the agent launches on the chosen GPU");
    assert!(ready.model_usable && ready.claim_retained, "{ready:?}");
    let record = fixture.record().expect("the engine recorded its launch");
    assert_eq!(record["cuda_visible_devices"], GPU1_UUID);
    let stopped = host
        .executor
        .execute(host.session, fixture.stop(&launch))
        .await
        .unwrap();
    assert_eq!(stopped.state, "completed");
}

/// Discrete GPU design §7: a GPU the host's own policy does not publish is
/// never launched on: the launch is refused `unauthorized` and nothing runs.
// T27 T21
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gpu_the_host_does_not_publish_refuses_the_launch() {
    let fixture = two_gpus_launching_on("gpu1");
    let host = host(&fixture);
    let mut launch = fixture.launch();
    if let MemberAction::LaunchSingle(plan) = &mut launch.action {
        let mut deployment: Value = serde_json::from_str(&plan.deployment_config).unwrap();
        name_device(&mut deployment, "gpu7");
        plan.deployment_config = deployment.to_string();
    }
    let launch = sign(launch);
    refused_launch(&host.executor, host.session, &launch).await;
    assert!(fixture.record().is_none(), "no engine was started");
}

/// Discrete GPU design §7 (review decision): a host with two GPUs that
/// published no UUIDs still pins the chosen one, by its index in PCI bus
/// order; the engine never inherits every GPU.
// T27 T21
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_chosen_gpu_without_a_uuid_is_pinned_by_its_pci_index() {
    let fixture = two_gpus_named(
        json!({
            "gpu0": {"domain": "unified", "sharing": "shared"},
            "gpu1": {"domain": "unified", "sharing": "shared"}
        }),
        "gpu1",
    );
    let host = host(&fixture);
    let launch = fixture.launch();
    host.executor.provision(launch.clone(), GATE).await.unwrap();
    let ready = host
        .executor
        .execute(host.session, launch.clone())
        .await
        .expect("the agent launches on the chosen GPU");
    assert!(ready.model_usable && ready.claim_retained, "{ready:?}");
    let record = fixture.record().expect("the engine recorded its launch");
    assert_eq!(record["cuda_visible_devices"], "1");
    assert_eq!(record["cuda_device_order"], "PCI_BUS_ID");
    let stopped = host
        .executor
        .execute(host.session, fixture.stop(&launch))
        .await
        .unwrap();
    assert_eq!(stopped.state, "completed");
}

/// Discrete GPU design §7 (review decision): with two GPUs, one the host
/// names neither by a UUID nor by a `gpuN` index cannot be pinned, so the
/// launch is refused `unauthorized` and nothing runs.
// T27 T21
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gpu_that_cannot_be_pinned_refuses_the_launch() {
    let fixture = two_gpus_named(
        json!({
            "left": {"domain": "unified", "sharing": "shared"},
            "right": {"domain": "unified", "sharing": "shared"}
        }),
        "right",
    );
    let host = host(&fixture);
    let launch = fixture.launch();
    refused_launch(&host.executor, host.session, &launch).await;
    assert!(fixture.record().is_none(), "no engine was started");
}

/// One 16 GB discrete card (`gpu0`, a device domain) beside host RAM
/// (`system`), and a restart-only deployment holding 12 GiB of the card and a
/// little host RAM in every active phase.
fn discrete_fixture() -> Fixture {
    let mut fixture = Fixture::build(Some(false), true, |_, deployment| {
        let both = |device: &str, system: &str| {
            json!([
                {"domain": "gpu0", "bytes": device, "host_kv_bytes": "0B"},
                {"domain": "system", "bytes": system, "host_kv_bytes": "0B"}
            ])
        };
        for phase in ["cold", "ready", "parking", "wake"] {
            deployment["resources"][phase]["allocations"] = both("12GiB", "64MiB");
        }
        deployment["resources"]["parked"]["allocations"] = both("0B", "0B");
    });
    // The strict remote-role schema learns `device` with the remote device
    // capability; the agent resolves from the approved document it is handed.
    fixture.config.document["resource_policy"]["domains"] = json!({
        "system": {"memory": "distinct", "managed_limit": "1GiB", "free_reserve": "16MiB",
                   "parked_limit": "256MiB", "host_kv_limit": "64MiB"},
        "gpu0": {"memory": "device", "device": "gpu0", "managed_limit": "14848MiB",
                 "free_reserve": "1536MiB", "parked_limit": "2GiB"}
    });
    fixture.config.document["resource_policy"]["devices"] =
        json!({"gpu0": {"domain": "gpu0", "sharing": "shared"}});
    fixture
}

/// SPEC §13.2, §7.2 (review decision, discrete GPU design §4, §6): the GPU
/// collector is bounded at seconds, so a launch samples the card before the
/// journal is locked and never while holding it; the locked rechecks read that
/// sample. A slow sampler that looks at the journal from inside its own run
/// always finds both locks free. The engine is sized from the same sample:
/// its utilization is the device request's share of the card (12 GiB of
/// 16376 MiB rounds up to 0.76), with the grant's KV bytes.
// T26 T16 T13
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_gpu_sample_never_holds_the_journal_lock() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let fixture = discrete_fixture();
    let mut host = host(&fixture);
    let journal = host.journal.clone();
    let runs = Arc::new(AtomicUsize::new(0));
    let locked = Arc::new(AtomicUsize::new(0));
    let (counted, seen) = (runs.clone(), locked.clone());
    let sampler: Arc<capyctl_agent::gpu_memory::GpuSampler> = Arc::new(move || {
        if !journal.locks_free() {
            seen.fetch_add(1, Ordering::SeqCst);
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        counted.fetch_add(1, Ordering::SeqCst);
        capyctl_agent::gpu_memory::parse_query_gpu(
            "0, GPU-11111111-2222-3333-4444-555555555555, 00000000:01:00.0, RTX, 16376, 0, 16376\n",
            now_ms(),
        )
    });
    host.executor = host.executor.clone().with_gpu_sampler(sampler);
    let launch = fixture.launch();
    host.executor.provision(launch.clone(), GATE).await.unwrap();
    let ready = host
        .executor
        .execute(host.session, launch.clone())
        .await
        .expect("the agent launches on the discrete card");
    // The locked rechecks (acceptance, the durable attempt, the spawn) read
    // the device from a sample: without one the device is unobserved and the
    // launch would be uncertain, never ready.
    assert!(ready.model_usable && ready.claim_retained, "{ready:?}");
    assert!(runs.load(Ordering::SeqCst) >= 1);
    assert_eq!(
        locked.load(Ordering::SeqCst),
        0,
        "a sample ran under a lock"
    );
    let record = fixture.record().expect("the engine recorded its launch");
    let argv: Vec<String> = serde_json::from_value(record["argv"].clone()).unwrap();
    assert!(
        argv.windows(2)
            .any(|w| w == ["--gpu-memory-utilization", "0.76"]),
        "{argv:?}"
    );
    assert!(
        argv.windows(2)
            .any(|w| w == ["--kv-cache-memory-bytes", "67108864"]),
        "{argv:?}"
    );
    // Discrete GPU design §7: the discrete card is pinned by its index.
    assert_eq!(record["cuda_visible_devices"], "0");
    let stopped = host
        .executor
        .execute(host.session, fixture.stop(&launch))
        .await
        .unwrap();
    assert_eq!(stopped.state, "completed");
}
