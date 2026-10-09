//! ADR 0028 §§8, 9, 10, 11: a host agent launches and terminates its own
//! member of a multi-node engine group, for vLLM, SGLang and TensorFold.
//!
//! Each host here is one real agent executor (journal, admission, ingress,
//! durable launcher) whose engine is a small Python stand-in with the engine's
//! observable launch shape. The host's interfaces and the probes of its peer
//! ports are substituted, so the plans use documentation addresses no machine
//! holds. CPU and fake-engine tests are not qualification: nothing here shows
//! that a group of any engine runs across two hosts; the live MN rows do.

use capyctl_agent::{
    host_checks::HostProbes,
    identity_storage::IdentityDirectory,
    ingress::Ingress,
    ingress_identity::IngressIdentities,
    journal::HostJournal,
    native_execution::{
        EnrolledSaver, NativeHostExecution, SaverMapped, SaverResidency, SaverScope,
        SaverUnavailable,
    },
    process_residency::{GpuCollector, ResidencySampler},
    session::SessionExecution,
};
use capyctl_config::remote_roles::HostConfig;
use capyctl_domain::group::{
    member_id, CommandIdentity, GroupEngine, GroupPlan, GroupTopology, MemberKey, MemberPlan,
    MemberRole,
};
use capyctl_protocol::{
    execution::{MemberAction, MemberCommand, SingleLaunchPlan},
    pb,
};
use serde_json::{json, Value};
use std::{
    net::IpAddr,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

const HEAD_PEER: &str = "192.0.2.10";
const WORKER_PEER: &str = "192.0.2.11";
/// R11: the interface the substituted host reports holding its peer address.
const IFNAME: &str = "eth9";
const RENDEZVOUS: u16 = 25000;
const GATE: [u8; 32] = [7; 32];

/// One stand-in for the three engines, chosen by the file it runs as: vLLM
/// through capyctl's protected entry (`serve <model> ...`, `--headless` on a
/// worker), SGLang as the protected `sglang_entry.py` (public settings on argv,
/// keys on sealed descriptors), TensorFold as its own binary (`--rank r`). It
/// records its argv, its environment's names, the group variables and each
/// start, never a key. A worker forks one child and waits; the head serves the
/// keyed model list and chat, and the engine's residency controls (ADR 0028
/// §12: the collective the head's agent invokes). With `ignore-sigterm` beside
/// it the whole tree ignores SIGTERM, as SGLang's rank > 0 does. It ends itself
/// when the test process or the fixture directory goes away.
const FAKE_ENGINE: &str = r#"
import json, os, signal, sys, threading, time, http.server, urllib.parse
args = sys.argv[1:]
here = os.path.dirname(os.path.abspath(__file__))
name = os.path.basename(__file__)
if os.path.exists(os.path.join(here, "ignore-sigterm")):
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
if name == "sglang_entry.py":
    engine = "sglang"
    settings = json.loads(args[args.index("--public-settings-json") + 1])
    group = settings.get("settings", {}).get("group") or {}
    worker = group.get("node_rank", 0) > 0
elif name == "tensorfold":
    engine = "tensorfold"
    worker = "--rank" in args and args[args.index("--rank") + 1] != "0"
else:
    engine = "vllm"
    worker = "--headless" in args
with open(os.path.join(here, "spawns"), "a") as f:
    f.write("spawn\n")
with open(os.path.join(here, "record.tmp"), "w") as f:
    json.dump({"engine": engine, "worker": worker, "argv": args,
               "env": sorted(os.environ),
               "gloo": os.environ.get("GLOO_SOCKET_IFNAME"),
               "host_ip": os.environ.get("VLLM_HOST_IP") or os.environ.get("SGLANG_HOST_IP"),
               "observation_dir": "CAPYCTL_OBSERVATION_DIR" in os.environ,
               "observation_fd": "--observation-credential-fd" in args}, f)
os.replace(os.path.join(here, "record.tmp"), os.path.join(here, "record.json"))
signal.signal(signal.SIGCHLD, signal.SIG_IGN)
parent = os.getppid()
if os.fork() == 0:
    while os.path.isdir(here):
        time.sleep(0.5)
    os._exit(0)
def orphaned():
    while os.getppid() == parent and os.path.isdir(here):
        time.sleep(0.5)
    os.killpg(0, signal.SIGKILL)
threading.Thread(target=orphaned, daemon=True).start()
if os.path.exists(os.path.join(here, "late-children")):
    # A child started just after the spawn, and one long after the tree settled.
    def late(delay, label):
        time.sleep(delay)
        pid = os.fork()
        if pid == 0:
            while os.path.isdir(here):
                time.sleep(0.5)
            os._exit(0)
        with open(os.path.join(here, label + ".tmp"), "w") as f:
            f.write(str(pid))
        os.replace(os.path.join(here, label + ".tmp"), os.path.join(here, label))
    threading.Thread(target=late, args=(0.4, "late-1"), daemon=True).start()
    threading.Thread(target=late, args=(3.0, "late-2"), daemon=True).start()
if worker or engine == "tensorfold":
    while True:
        time.sleep(0.5)
if engine == "sglang":
    port = int(settings["endpoint"].rsplit(":", 1)[1])
    served = settings["served_name"]
    fd = int(args[args.index("--inference-credential-fd") + 1])
    key = os.pread(fd, 4096, 0).decode().strip()
    admin = ""
    if "--admin-credential-fd" in args:
        admin = os.pread(int(args[args.index("--admin-credential-fd") + 1]), 4096, 0).decode().strip()
else:
    port = int(args[args.index("--port") + 1])
    served = args[args.index("--served-model-name") + 1]
    key = os.environ.get("VLLM_API_KEY", "")
    admin = os.environ.get("CAPYCTL_VLLM_ADMIN_KEY", "")
# ADR 0028 §12: the head's residency surface. vLLM (development mode): sleep,
# wake one class, reload weights, reset the prefix cache, `/is_sleeping`, the
# running and waiting gauges. SGLang: release and resume memory occupation
# (which writes or removes the `released` marker the test's saver reads), disk
# reload, cache flush, the gauges. Control calls are keyed with the admin key
# and appended to `residency.log`; chat answers only while usable.
state = {"sleeping": False, "weights": True, "loaded": True, "kv": True}
def logged(call):
    with open(os.path.join(here, "residency.log"), "a") as f:
        f.write(call + "\n")
def usable():
    return state["weights"] and state["loaded"] and state["kv"]
FLUSH = "Cache flushed.\nPlease check backend logs for more details. (When there are running or waiting requests, the operation will not be performed.)\n"
# ADR 0028 §9: the completion probe's answer, when the test installs one.
def completion():
    try:
        with open(os.path.join(here, "completion")) as f:
            return json.load(f)
    except OSError:
        return None

class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"
    def log_message(self, *a):
        pass
    def keyed(self):
        if not key or self.headers.get("Authorization") != "Bearer " + key:
            self.send_response(401); self.send_header("Content-Length", "0"); self.end_headers(); return False
        return True
    def admin_keyed(self):
        want = admin or key
        if not want or self.headers.get("Authorization") != "Bearer " + want:
            self.send_response(401); self.send_header("Content-Length", "0"); self.end_headers(); return False
        return True
    def send_text(self, text):
        body = text.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers(); self.wfile.write(body)
    def control(self, path, query, body):
        if engine == "vllm":
            if path == "/sleep":
                call = "sleep:" + query.get("level", [""])[0]
            elif path == "/wake_up":
                call = "wake:" + query.get("tags", [""])[0]
            elif path == "/collective_rpc":
                call = "collective:" + json.loads(body or b"{}").get("method", "")
            else:
                call = "reset_prefix_cache"
            logged(call)
            if call == "sleep:2":
                state.update(sleeping=True, weights=False, loaded=False, kv=False)
            elif call == "sleep:1":
                state.update(sleeping=True, weights=False, kv=False)
            elif call == "wake:weights":
                state["weights"] = True
            elif call == "wake:kv_cache":
                state["kv"] = True
            elif call == "collective:reload_weights" and state["weights"]:
                state["loaded"] = True
            elif call == "reset_prefix_cache":
                self.send_body(json.dumps({"success": True}).encode()); return
            else:
                self.send_response(409); self.send_header("Content-Length", "0"); self.end_headers(); return
            state["sleeping"] = not usable()
            self.send_body(b"{}")
            return
        logged(path)
        marker = os.path.join(here, "released")
        if path == "/release_memory_occupation":
            open(marker, "w").close()
            self.send_body(b"null")
        elif path == "/resume_memory_occupation":
            if os.path.exists(marker):
                os.remove(marker)
            self.send_body(b"null")
        elif path == "/update_weights_from_disk":
            self.send_body(json.dumps({"success": True}).encode())
        else:
            self.send_text(FLUSH)
    def send_body(self, body):
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers(); self.wfile.write(body)
    def do_GET(self):
        if self.path == "/is_sleeping":
            if not self.admin_keyed(): return
            self.send_body(json.dumps({"is_sleeping": state["sleeping"]}).encode()); return
        if not self.keyed(): return
        if self.path == "/v1/models":
            self.send_body(json.dumps({"object": "list", "data": [{"id": served, "object": "model"}]}).encode())
        elif self.path == "/metrics":
            if engine == "vllm":
                self.send_text("vllm:num_requests_running{engine=\"0\"} 0.0\nvllm:num_requests_waiting{engine=\"0\"} 0.0\n")
            else:
                self.send_text("sglang:num_running_reqs 0.0\nsglang:num_queue_reqs 0.0\n")
        else:
            self.send_response(404); self.send_header("Content-Length", "0"); self.end_headers()
    def do_POST(self):
        url = urllib.parse.urlparse(self.path)
        controls = ("/sleep", "/wake_up", "/collective_rpc", "/reset_prefix_cache",
                    "/release_memory_occupation", "/resume_memory_occupation",
                    "/update_weights_from_disk", "/flush_cache")
        if url.path in controls:
            if not self.admin_keyed(): return
            body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
            self.control(url.path, urllib.parse.parse_qs(url.query), body); return
        keyed = bool(key) and self.headers.get("Authorization") == "Bearer " + key
        if self.path in ("/v1/completions", "/generate"):
            with open(os.path.join(here, "last-request.tmp"), "w") as f:
                json.dump({"path": self.path, "client": self.client_address[0], "keyed": keyed}, f)
            os.replace(os.path.join(here, "last-request.tmp"), os.path.join(here, "last-request.json"))
        if not self.keyed(): return
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
        if not usable() or os.path.exists(os.path.join(here, "released")):
            self.send_response(503); self.send_header("Content-Length", "0"); self.end_headers(); return
        ids = completion()
        if ids is not None and engine == "vllm" and self.path == "/v1/completions" and body.get("model") == served:
            self.send_body(json.dumps({"choices": [{"index": 0, "text": "ok", "token_ids": ids[:body["max_tokens"]]}]}).encode()); return
        if ids is not None and engine == "sglang" and self.path == "/generate":
            self.send_body(json.dumps({"text": "ok", "output_ids": ids[:body["sampling_params"]["max_new_tokens"]]}).encode()); return
        if self.path != "/v1/chat/completions" or body.get("model") != served:
            self.send_response(404); self.send_header("Content-Length", "0"); self.end_headers(); return
        if body.get("stream"):
            def chunk(delta, finish):
                return {"id": "c1", "object": "chat.completion.chunk", "created": 1, "model": served,
                        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            for part in (chunk({"role": "assistant", "content": ""}, None), chunk({"content": "group head"}, None), chunk({}, "stop")):
                self.wfile.write(("data: " + json.dumps(part) + "\n\n").encode())
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
            return
        self.send_body(json.dumps({"id": "c1", "object": "chat.completion", "created": 1, "model": served,
                                   "choices": [{"index": 0, "finish_reason": "stop",
                                                "message": {"role": "assistant", "content": "group head"}}]}).encode())

http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
"#;

fn directory() -> tempfile::TempDir {
    let d = tempfile::Builder::new()
        .prefix("capyctl-group-launch-")
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

fn module(path: &Path, contents: &str) {
    std::fs::write(path, contents).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
}

fn python3() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("python3"))
        .find(|candidate| candidate.is_file())
        .expect("python3 on PATH for the fake engines")
}

fn now_ms() -> i64 {
    capyctl_protocol::now_unix_ms()
}

fn golden(engine: &str) -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(format!(
            "{}/../capyctl-config/tests/fixtures/effective-{engine}-golden.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap(),
    )
    .unwrap()
}

/// One enrolled host of a group, as the tests configure it before first use.
struct FakeHost {
    host_id: String,
    peer: Option<String>,
    engine: GroupEngine,
    ignore_sigterm: bool,
    late_children: bool,
    deep: bool,
    /// Whether the substituted host holds its own peer address.
    holds_peer: bool,
    /// Peer ports held outside CapyCTL.
    held: Vec<u16>,
    /// ADR 0028 §9: the token ids the engine answers a completion probe with.
    completion: Option<Vec<u32>>,
    /// ADR 0028 §12 (R12): the host reads its SGLang saver map through
    /// [`StubSaver`] instead of an enrolled scheduler.
    stub_saver: bool,
    /// ADR 0007: the host's GPU processes, as `nvidia-smi` would list them.
    gpu: Option<Arc<GpuCollector>>,
    built: OnceLock<Built>,
}

/// SPEC §9.2, ADR 0028 §12 (R12): the saver map of the fake SGLang engine on
/// this host: every allocation mapped until the engine released its memory
/// occupation (the `released` marker beside it), none after. It answers
/// only a scope naming a credential and recorded processes, and keeps every
/// scope it was asked about.
struct StubSaver {
    dir: PathBuf,
    marker: PathBuf,
    scopes: std::sync::Mutex<Vec<SaverScope>>,
}
impl SaverResidency for StubSaver {
    fn mapped(&self, scope: &SaverScope) -> Result<SaverMapped, SaverUnavailable> {
        self.scopes.lock().unwrap().push(scope.clone());
        if scope.admin_key.is_empty() || scope.members.as_ref().is_none_or(Vec::is_empty) {
            return Err(SaverUnavailable);
        }
        let mapped = if self.marker.exists() { 0 } else { 1 << 20 };
        Ok(SaverMapped {
            real_saver: true,
            weight_bytes: mapped,
            kv_bytes: mapped,
            weight_virtual_bytes: 1 << 20,
            kv_virtual_bytes: 1 << 20,
        })
    }
    fn observation_dir(&self) -> Option<&Path> {
        Some(&self.dir)
    }
}

/// The host once built: its fixture, its executor and its open session.
struct Built {
    /// Declared first so it runs first on drop: whatever the host launched is
    /// reaped before anything else of it goes away.
    _reap: Reap,
    root: tempfile::TempDir,
    config: HostConfig,
    deployment: Value,
    service_port: u16,
    worker_port: u16,
    journal: Arc<HostJournal>,
    ingress: Arc<Ingress>,
    executor: Arc<NativeHostExecution>,
    session: u64,
    saver: Option<Arc<StubSaver>>,
    sampler: Option<Arc<ResidencySampler>>,
}

/// Kills whatever the journal recorded if a test fails mid way.
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
        let tools = capyctl_launchers::DurableProcessLaunch::new(Arc::new(NoSpawn));
        for claimed in self.0.claimed_launches("").unwrap_or_default() {
            if let Ok(owned) = self.0.inspect_owned(&claimed.command.identity.command_id) {
                let identities: Vec<_> = owned.into_iter().map(|(p, _)| p).collect();
                let _ = tools.terminate_owned(&identities, std::time::Duration::from_millis(200));
            }
        }
    }
}

/// What a member's Launch answered.
struct Launched {
    result: pb::MemberExecutionResult,
    owned_handle: String,
    processes: Vec<pb::OwnedProcessObservation>,
    /// The open, handle-bound ingress entry of the launch, if it has one.
    ingress: Option<capyctl_agent::ingress::IngressScope>,
}

/// ADR 0028 §9, ADR 0012: how a completion probe reached the head's engine.
struct EngineRequest {
    client: String,
    keyed: bool,
}
impl EngineRequest {
    /// From loopback, never ingress or the router, with the launch's own key.
    fn is_loopback_with_launch_key(&self) -> bool {
        self.keyed
            && self
                .client
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    }
}

/// What a member's Terminate answered.
struct Gone {
    result: pb::MemberExecutionResult,
    escalated: bool,
}
impl Gone {
    fn all_gone(&self) -> bool {
        self.result.state == "completed"
            && !self.result.claim_retained
            && !self.result.processes.is_empty()
            && self.result.processes.iter().all(|p| p.presence == "gone")
    }
}

impl FakeHost {
    fn new(host_id: &str) -> Self {
        Self {
            host_id: host_id.into(),
            peer: None,
            engine: GroupEngine::Vllm,
            ignore_sigterm: false,
            late_children: false,
            deep: false,
            holds_peer: true,
            held: Vec::new(),
            completion: None,
            stub_saver: false,
            gpu: None,
            built: OnceLock::new(),
        }
    }
    /// ADR 0028 §3: the host's declared peer address.
    fn with_groups_policy(mut self, peer: &str) -> Self {
        self.peer = Some(peer.into());
        self
    }
    fn with_engine(mut self, engine: &str) -> Self {
        self.engine = match engine {
            "vllm" => GroupEngine::Vllm,
            "sglang" => GroupEngine::Sglang,
            "tensorfold" => GroupEngine::Tensorfold,
            other => panic!("unknown engine {other}"),
        };
        self
    }
    /// `residency: deep` on a host whose deep park is enabled, its SGLang
    /// saver map read through [`StubSaver`].
    fn with_deep_park_and_stub_saver(mut self) -> Self {
        self.deep = true;
        self.stub_saver = true;
        self
    }
    /// ADR 0007: the host's `process_residency` reads its GPU processes
    /// through `gpu`.
    fn with_gpu_processes(mut self, gpu: Arc<GpuCollector>) -> Self {
        self.gpu = Some(gpu);
        self
    }
    /// The worker forks a child 0.4 s after it starts and another after 3 s.
    fn with_late_children(mut self) -> Self {
        self.late_children = true;
        self
    }
    /// The engine's whole tree ignores SIGTERM (SGLang rank > 0 does).
    fn with_fake_ignoring_sigterm(mut self) -> Self {
        self.ignore_sigterm = true;
        self
    }
    /// SGLang `residency: deep` on a host whose deep park is enabled.
    fn with_deep_park(mut self) -> Self {
        self.deep = true;
        self
    }
    /// The declared peer address is on no interface of this host.
    fn without_its_peer_address(mut self) -> Self {
        self.holds_peer = false;
        self
    }
    /// `port` is held on the peer address by something outside CapyCTL.
    fn with_held_peer_port(mut self, port: u16) -> Self {
        self.held.push(port);
        self
    }
    /// ADR 0028 §9: the head's engine answers a completion probe with `ids`.
    fn with_fake_completion(mut self, ids: Vec<u32>) -> Self {
        self.completion = Some(ids);
        self
    }

    fn host(&self) -> &Built {
        self.built.get_or_init(|| self.build())
    }

    fn build(&self) -> Built {
        let root = directory();
        let path = root.path();
        // A port learned from `127.0.0.1:0` and released sits in the ephemeral
        // range, where a concurrent test's outbound connection can take it
        // before the Launch checks it (`service_port_in_use`); the testkit
        // pool hands out ports below that range, never twice.
        let base_port = capyctl_testkit::ports::free_ports(3, true)[0];
        let bin = path.join("venv/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(python3(), bin.join("python3")).unwrap();
        let runtime = private(&path.join("runtime"));
        let fake = |file: &Path| {
            capyctl_config::test_support::write_executable(
                file,
                format!("#!{}\n{FAKE_ENGINE}", python3().display()),
                0o755,
            )
            .unwrap();
        };
        // SPEC §9.1, §13.3: the runtime directory holds capyctl's protected
        // entries, owned by the agent user and writable by it alone.
        let vllm = bin.join("vllm");
        let tensorfold = bin.join("tensorfold");
        fake(&vllm);
        fake(&tensorfold);
        module(
            &runtime.join("vllm_entry.py"),
            &format!(
                "import runpy\nrunpy.run_path({:?}, run_name=\"__main__\")\n",
                vllm.display().to_string()
            ),
        );
        module(&runtime.join("capyctl_vllm_guard.py"), "# stand-in guard\n");
        module(&runtime.join("sglang_entry.py"), FAKE_ENGINE);
        module(&runtime.join("pinned_file_observation.py"), "# stand-in\n");
        module(
            &runtime.join("engine_capabilities.py"),
            "# stand-in probes\n",
        );
        for (marker, on) in [
            ("ignore-sigterm", self.ignore_sigterm),
            ("late-children", self.late_children),
        ] {
            if on {
                for dir in [&bin, &runtime] {
                    std::fs::write(dir.join(marker), "").unwrap();
                }
            }
        }
        if let Some(ids) = &self.completion {
            for dir in [&bin, &runtime] {
                std::fs::write(dir.join("completion"), json!(ids).to_string()).unwrap();
            }
        }
        let models = private(&path.join("models"));
        std::fs::create_dir_all(models.join("toy")).unwrap();
        std::fs::write(models.join("toy/config.json"), "{}").unwrap();

        let source = golden(if self.engine == GroupEngine::Sglang {
            "sglang"
        } else {
            "vllm"
        });
        let mut host = source["input"]["host"].clone();
        let state = path.join("state");
        host["state_dir"] = json!(state);
        host["identity_dir"] = json!(state.join("identity"));
        host["runtime_dir"] = json!(runtime);
        host["model_store"]["path"] = json!(models);
        host["resource_policy"]["endpoint_port_range"] =
            json!({"start": base_port, "end": base_port + 2});
        host["resource_policy"]["domains"]["unified"] = json!({
            "free_reserve": "16MiB", "host_kv_limit": "64MiB", "managed_limit": "1GiB",
            "memory": "unified", "parked_limit": "256MiB"
        });
        if let Some(peer) = &self.peer {
            host["resource_policy"]["groups"] = json!({"peer_address": peer});
        }
        let profile = &mut host["runtime_profiles"]["local"];
        profile["security"]["deep_park"] = json!(if self.deep { "enabled" } else { "disabled" });
        match self.engine {
            GroupEngine::Vllm => profile["executable"] = json!(vllm),
            GroupEngine::Sglang => profile["executable"] = json!(bin.join("python3")),
            GroupEngine::Tensorfold => {
                profile["engine"] = json!("tensorfold");
                profile["executable"] = json!(tensorfold);
                profile["build_fingerprint"] = json!("0.6.0");
                profile["args"] = json!([]);
                profile["security"]
                    .as_object_mut()
                    .unwrap()
                    .remove("admin_credential_ref");
            }
        }

        let mut deployment = source["input"]["deployment"].clone();
        deployment["model"]["path"] = json!(models.join("toy"));
        deployment["engine_config"] = match self.engine {
            GroupEngine::Tensorfold => json!({"context_length": 8192}),
            _ => json!({"memory": {"kv_cache": "64MiB"}}),
        };
        deployment["residency"] = json!(if self.deep { "deep" } else { "restart_only" });
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

        let journal =
            HostJournal::open(&private(&path.join("journal")), "controller", &self.host_id)
                .unwrap();
        let ingress = Ingress::new().unwrap();
        let identities = IngressIdentities::new(
            IdentityDirectory::open(&private(&path.join("ingress-identity"))).unwrap(),
        );
        let peer: Option<IpAddr> = self.peer.as_ref().map(|peer| peer.parse().unwrap());
        let interfaces = peer
            .filter(|_| self.holds_peer)
            .map(|peer| vec![(IFNAME.to_owned(), peer)])
            .unwrap_or_default();
        let held = self.held.clone();
        let probes = HostProbes::with(
            move || interfaces.clone(),
            move |address, port| {
                if address.is_loopback() {
                    capyctl_agent::host_checks::port_free(address, port)
                } else {
                    !held.contains(&port)
                }
            },
        );
        let mut executor = NativeHostExecution::new(
            journal.clone(),
            ingress.clone(),
            identities,
            config.clone(),
            self.host_id.clone(),
            "controller".into(),
            config.runtime_dir.clone(),
            private(&path.join("logs")),
            // ADR 0007: the role's startup inventory names the host's one
            // memory domain, which every availability refresh reports (with
            // the processes its sampler found and its members' saver maps).
            pb::ReportInventory {
                domains: vec![pb::DomainObservation {
                    domain_id: "unified".into(),
                    kind: "system".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        )
        .with_host_probes(probes)
        // SPEC §8.2: a single SGLang launch would keep its rendezvous here;
        // a group member never does.
        .with_rendezvous_root(private(&state.join("rendezvous")))
        .with_engine_cache_root(private(&path.join("engines")));
        let mut saver = None;
        if self.stub_saver {
            let stub = Arc::new(StubSaver {
                dir: private(&state.join("observation")),
                marker: runtime.join("released"),
                scopes: std::sync::Mutex::new(Vec::new()),
            });
            executor = executor.with_saver_residency(stub.clone());
            saver = Some(stub);
        } else if self.deep {
            executor = executor.with_saver_residency(Arc::new(EnrolledSaver::new(private(
                &state.join("observation"),
            ))));
        }
        let sampler = self.gpu.clone().map(ResidencySampler::with_collector);
        if let Some(sampler) = &sampler {
            executor = executor.with_process_residency(sampler.clone());
        }
        let session = journal.connect().unwrap();
        executor.connected(session).unwrap();
        Built {
            _reap: Reap(journal.clone()),
            root,
            config,
            deployment,
            service_port: base_port,
            worker_port: base_port + 1,
            journal,
            ingress,
            executor,
            session,
            saver,
            sampler,
        }
    }

    fn journal(&self) -> &Arc<HostJournal> {
        &self.host().journal
    }

    /// The engine's own record of its last start. A worker's Launch answers
    /// once its process is recorded, which may be before the interpreter has
    /// run far enough to write it, so it is awaited (bounded).
    fn record(&self) -> Value {
        let path = self.host().root.path();
        let files = [
            path.join("venv/bin/record.json"),
            path.join("runtime/record.json"),
        ];
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            if let Some(file) = files.iter().find(|file| file.exists()) {
                return serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the engine recorded its start"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    fn last_argv(&self) -> Vec<String> {
        serde_json::from_value(self.record()["argv"].clone()).unwrap()
    }

    /// How many times an engine started on this host.
    fn spawns(&self) -> usize {
        let path = self.host().root.path();
        [path.join("venv/bin/spawns"), path.join("runtime/spawns")]
            .iter()
            .filter_map(|file| std::fs::read_to_string(file).ok())
            .map(|text| text.lines().count())
            .sum()
    }

    /// ADR 0028 §9: what the engine last saw of a completion probe request.
    fn last_engine_request(&self) -> EngineRequest {
        let path = self.host().root.path();
        let file = [
            path.join("venv/bin/last-request.json"),
            path.join("runtime/last-request.json"),
        ]
        .into_iter()
        .find(|file| file.exists())
        .expect("the engine saw a completion probe");
        let seen: Value = serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
        EngineRequest {
            client: seen["client"].as_str().unwrap().into(),
            keyed: seen["keyed"] == true,
        }
    }

    /// The checkpoint digest this host measures, as the server records it.
    fn digest(&self) -> String {
        let built = self.host();
        let local =
            capyctl_config::remote_resources::local_host_document(&built.config.document).unwrap();
        let location =
            capyctl_config::effective::checkpoint_location(&built.deployment, &local).unwrap();
        capyctl_agent::checkpoint::CheckpointVerifier::in_memory()
            .measure(&location.model_store, &location.checkpoint)
            .unwrap()
            .manifest
            .digest
    }

    fn effective(&self) -> capyctl_config::effective::EffectiveDeployment {
        let built = self.host();
        let local =
            capyctl_config::remote_resources::local_host_document(&built.config.document).unwrap();
        capyctl_config::effective::resolve_effective(&built.deployment, &local).unwrap()
    }

    /// ADR 0028 §8 (ruling R29): this host's Launch of its member of `plan`,
    /// with its own member launch, as the server sends it. A host the plan
    /// does not name is addressed as `worker-1`.
    fn launch_command(&self, id: &str, plan: &GroupPlan) -> MemberCommand {
        let built = self.host();
        let member = plan
            .members()
            .iter()
            .find(|m| m.member.host_id == self.host_id);
        let effective = self.effective();
        let mut command = MemberCommand {
            identity: identity(
                id,
                MemberKey {
                    host_id: self.host_id.clone(),
                    member_id: member.map_or_else(|| member_id(1), |m| m.member.member_id.clone()),
                },
                "reserved",
                plan.generation(),
                &effective.profile.build_fingerprint,
            ),
            action: MemberAction::Launch {
                plan: plan.clone(),
                member: SingleLaunchPlan {
                    deployment_config: built.deployment.to_string(),
                    profile_name: "local".into(),
                    checkpoint_fingerprint: effective.model.content_fingerprint.clone(),
                    host_policy_fingerprint: capyctl_config::remote_resources::policy_fingerprint(
                        &built.config.document,
                    ),
                    binding_id: "01K00000000000000000000001".into(),
                    incarnation: "01K00000000000000000000002".into(),
                    grant_id: "01K00000000000000000000003".into(),
                    service_port: member.and_then(|m| m.service_port).unwrap_or(0),
                    issued_at_ms: now_ms(),
                    coordinator_session_id: "01K00000000000000000000004".into(),
                    checkpoint_digest: self.digest(),
                    checkpoint_weights_bytes: None,
                    checkpoint_state_slot_bytes: None,
                    checkpoint_layout: None,
                    checkpoint_tables: None,
                    checkpoint_gguf: None,
                    startup_bytes: None,
                },
            },
        };
        command.identity.payload_digest = command.canonical_digest();
        command
    }

    /// Send `action` (a group Launch) to this host: a head is provisioned
    /// with its gate key first, as the server does. A refusal is the `Err`.
    async fn execute(&self, action: MemberAction) -> Result<Launched, String> {
        match action {
            MemberAction::Launch { plan, .. } => {
                self.send(&self.launch_command("launch", &plan)).await
            }
            // ADR 0028 §9: the completion probe the server sends the head,
            // naming its retained launch.
            MemberAction::Probe {
                owned_handle,
                max_tokens,
            } => {
                self.send_probe(&self.probe_command(owned_handle, max_tokens))
                    .await
            }
            _ => panic!("only a group Launch or a Probe is sent here"),
        }
    }

    /// ADR 0028 §9: the Probe the server sends the head, naming its retained
    /// launch `owned_handle`.
    fn probe_command(&self, owned_handle: String, max_tokens: Option<u32>) -> MemberCommand {
        let built = self.host();
        let owner = built.journal.retained_command(&owned_handle).unwrap();
        let mut command = MemberCommand {
            identity: identity(
                &format!("probe-{}", max_tokens.unwrap_or(0)),
                owner.identity.member.clone(),
                "ready",
                owner.identity.generation,
                &owner.identity.profile_fingerprint,
            ),
            action: MemberAction::Probe {
                owned_handle,
                max_tokens,
            },
        };
        command.identity.payload_digest = command.canonical_digest();
        command
    }

    /// Send the Probe `command` to this host, exactly as given.
    async fn send_probe(&self, command: &MemberCommand) -> Result<Launched, String> {
        let built = self.host();
        let result = built
            .executor
            .execute(built.session, command.clone())
            .await
            .map_err(|_| "session".to_owned())?;
        capyctl_protocol::execution::validate_result(command, &result)
            .expect("the result is a valid answer to its Probe");
        Ok(Launched {
            owned_handle: result.owned_handle.clone(),
            processes: result.processes.clone(),
            ingress: None,
            result,
        })
    }

    async fn send(&self, command: &MemberCommand) -> Result<Launched, String> {
        let built = self.host();
        // ADR 0028 §6, §7: the server has every host measure its checkpoint
        // (DigestCheckpoint) before Prepare and Launch; the Launch's checks
        // read that measurement and never hash anything themselves.
        if let MemberAction::Launch { member, .. } = &command.action {
            let mut digest = MemberCommand {
                identity: identity(
                    "digest",
                    command.identity.member.clone(),
                    "checkpoint",
                    command.identity.generation,
                    &command.identity.profile_fingerprint,
                ),
                action: MemberAction::DigestCheckpoint(
                    capyctl_protocol::execution::DigestCheckpointPlan {
                        deployment_config: member.deployment_config.clone(),
                        host_policy_fingerprint: member.host_policy_fingerprint.clone(),
                        expected_digest: None,
                        size_only: false,
                    },
                ),
            };
            digest.identity.payload_digest = digest.canonical_digest();
            let measured = built
                .executor
                .execute(built.session, digest)
                .await
                .map_err(|_| "digest".to_owned())?;
            assert_eq!(measured.checkpoint.unwrap().state, "computed");
        }
        if command
            .action
            .launch_plan()
            .is_some_and(|plan| plan.service_port != 0)
        {
            built
                .executor
                .provision(command.clone(), GATE)
                .await
                .map_err(|_| "provision".to_owned())?;
        }
        let result = built
            .executor
            .execute(built.session, command.clone())
            .await
            .map_err(|_| "session".to_owned())?;
        capyctl_protocol::execution::validate_result(command, &result)
            .expect("the result is a valid answer to its Launch");
        if !result.refused.is_empty() {
            return Err(result.refused);
        }
        let ingress = built
            .ingress
            .load_targets()
            .unwrap()
            .into_iter()
            .find(|target| target.owned_handle == command.identity.command_id)
            .map(|target| target.scope);
        Ok(Launched {
            owned_handle: result.owned_handle.clone(),
            processes: result.processes.clone(),
            ingress,
            result,
        })
    }

    /// ADR 0028 §11: Terminate this host's member launch `owned_handle`,
    /// naming the identities its Launch reported.
    async fn terminate(
        &self,
        owned_handle: &str,
        recorded: &[pb::OwnedProcessObservation],
    ) -> Result<Gone, String> {
        let built = self.host();
        let owner = built.journal.retained_command(owned_handle).unwrap();
        let mut command = MemberCommand {
            identity: identity(
                "stop",
                owner.identity.member.clone(),
                "retained",
                owner.identity.generation,
                &owner.identity.profile_fingerprint,
            ),
            action: MemberAction::Terminate {
                owned_handle: owned_handle.into(),
                recorded: recorded
                    .iter()
                    .map(|p| capyctl_domain::completion::ProcessIdentity {
                        role: p.role.clone(),
                        pid: p.pid,
                        boot_id: p.boot_id.clone(),
                        start_ticks: p.start_ticks,
                    })
                    .collect(),
            },
        };
        command.identity.payload_digest = command.canonical_digest();
        command.verify_digest().unwrap();
        let result = built
            .executor
            .execute(built.session, command.clone())
            .await
            .map_err(|_| "session".to_owned())?;
        capyctl_protocol::execution::validate_result(&command, &result)
            .expect("the result is a valid answer to its Terminate");
        Ok(Gone {
            escalated: result.escalated,
            result,
        })
    }

    /// ADR 0028 §12: the Park (or Restore) the server sends a group's head,
    /// naming its retained launch `owned_handle`, under command id `id`. A
    /// Restore carries the checkpoint digest every member measured.
    fn residency_command(&self, id: &str, owned_handle: &str, park: bool) -> MemberCommand {
        let owner = self.journal().retained_command(owned_handle).unwrap();
        let mut command = MemberCommand {
            identity: identity(
                id,
                owner.identity.member.clone(),
                if park { "ready" } else { "parked" },
                owner.identity.generation,
                &owner.identity.profile_fingerprint,
            ),
            action: if park {
                MemberAction::Park {
                    owned_handle: owned_handle.into(),
                }
            } else {
                MemberAction::Restore {
                    owned_handle: owned_handle.into(),
                    checkpoint_digest: self.digest(),
                }
            },
        };
        command.identity.payload_digest = command.canonical_digest();
        command
    }

    /// Send the Park or Restore `command` to this host: its answer, checked
    /// as a valid one.
    async fn send_residency(&self, command: &MemberCommand) -> pb::MemberExecutionResult {
        let built = self.host();
        let result = built
            .executor
            .execute(built.session, command.clone())
            .await
            .expect("a Park or Restore is answered");
        capyctl_protocol::execution::validate_result(command, &result)
            .expect("the result is a valid answer to its Park or Restore");
        result
    }

    /// The residency controls the engine received, in order.
    fn controls(&self) -> Vec<String> {
        let path = self.host().root.path();
        [
            path.join("venv/bin/residency.log"),
            path.join("runtime/residency.log"),
        ]
        .iter()
        .filter_map(|file| std::fs::read_to_string(file).ok())
        .flat_map(|text| text.lines().map(str::to_owned).collect::<Vec<_>>())
        .collect()
    }
}

/// How a Park or Restore result reports the launch's residency.
fn residency_state(result: &pb::MemberExecutionResult) -> &str {
    result
        .residency
        .as_ref()
        .map_or("none", |residency| residency.state.as_str())
}

/// ADR 0028 §8 (R23): whether `host` journaled a Launch of `plan`'s rank
/// `rank`, keyed by deployment, instance, generation and member.
fn has_launch_for(host: &FakeHost, plan: &GroupPlan, rank: u32) -> bool {
    host.journal()
        .group_launch("deployment", 0, plan.generation(), &member_id(rank))
        .unwrap()
        .is_some()
}

fn identity(
    id: &str,
    member: MemberKey,
    expected_state: &str,
    generation: i64,
    fingerprint: &str,
) -> CommandIdentity {
    CommandIdentity {
        controller_id: "controller".into(),
        member,
        deployment_id: "deployment".into(),
        operation_id: "operation".into(),
        command_id: id.into(),
        step_id: id.into(),
        generation,
        revision: 1,
        deadline_ms: now_ms() + 60_000,
        payload_digest: [0; 32],
        expected_state: expected_state.into(),
        profile_fingerprint: fingerprint.into(),
        instance_index: 0,
    }
}

/// ADR 0028 §4: host A heads at 192.0.2.10 and host B works at 192.0.2.11,
/// TP 2 with one rank per host. Every member's local facts (profile, model
/// path, checkpoint, devices, ports) are those of `host`, the host the plan
/// is sent to; the other member's are the same values, as a peer would have.
fn two_member_plan(host: &FakeHost) -> GroupPlan {
    let built = host.host();
    let effective = host.effective();
    let model_path = built
        .root
        .path()
        .join("models/toy")
        .to_string_lossy()
        .into_owned();
    let digest = host.digest();
    let devices: Vec<String> = effective
        .selected_devices
        .iter()
        .map(|device| device.id.clone())
        .collect();
    let member = |rank: u32, host_id: &str, peer: &str| MemberPlan {
        member: MemberKey {
            host_id: host_id.into(),
            member_id: member_id(rank),
        },
        rank,
        role: if rank == 0 {
            MemberRole::Head
        } else {
            MemberRole::Worker
        },
        profile_name: "local".into(),
        profile_fingerprint: effective.profile.build_fingerprint.clone(),
        checkpoint_fingerprint: digest.clone(),
        model_path: model_path.clone(),
        devices: devices.clone(),
        peer_address: peer.parse().unwrap(),
        service_port: (rank == 0).then_some(built.service_port),
        worker_port: (rank > 0 && host.engine == GroupEngine::Sglang).then_some(built.worker_port),
    };
    GroupPlan::new(
        host.engine,
        vec![
            member(0, "host-a", HEAD_PEER),
            member(1, "host-b", WORKER_PEER),
        ],
        GroupTopology {
            tensor_parallel: 2,
            pipeline_parallel: 1,
            local_ranks: 1,
        },
        RENDEZVOUS,
        1,
    )
    .unwrap()
}

fn launch(plan: GroupPlan) -> MemberAction {
    // The member launch is the host's own; `FakeHost::execute` builds it.
    MemberAction::Launch {
        member: SingleLaunchPlan {
            deployment_config: String::new(),
            profile_name: String::new(),
            checkpoint_fingerprint: String::new(),
            host_policy_fingerprint: String::new(),
            binding_id: String::new(),
            incarnation: String::new(),
            grant_id: String::new(),
            service_port: 0,
            issued_at_ms: 0,
            coordinator_session_id: String::new(),
            checkpoint_digest: String::new(),
            checkpoint_weights_bytes: None,
            checkpoint_state_slot_bytes: None,
            checkpoint_layout: None,
            checkpoint_tables: None,
            checkpoint_gguf: None,
            startup_bytes: None,
        },
        plan,
    }
}

// T30: a worker launches, journals first, reports identities, opens no ingress (each engine).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_member_launches_and_journals_first() {
    for engine in ["vllm", "sglang", "tensorfold"] {
        let host = FakeHost::new("host-b")
            .with_groups_policy(WORKER_PEER)
            .with_engine(engine);
        let plan = two_member_plan(&host);
        let out = host.execute(launch(plan.clone())).await.unwrap();
        assert!(has_launch_for(&host, &plan, 1), "{engine}");
        assert!(out.ingress.is_none(), "{engine}");
        assert_eq!(out.result.state, "launched", "{engine}");
        assert!(
            out.result.claim_retained && !out.result.model_usable,
            "{engine}"
        );
        assert!(!out.processes.is_empty(), "{engine}");
        assert!(
            out.processes.iter().all(|p| p.role.starts_with("worker-1")),
            "{engine}: {:?}",
            out.processes
        );
        assert_eq!(out.processes[0].role, "worker-1", "{engine}");
        assert!(
            out.processes.iter().all(|p| p.presence == "alive"),
            "{engine}"
        );
        // ADR 0012: a worker holds no API key and has no file rendezvous.
        let record = host.record();
        assert_eq!(record["worker"], true, "{engine}: {record}");
        let env: Vec<String> = serde_json::from_value(record["env"].clone()).unwrap();
        for name in [
            "VLLM_API_KEY",
            "CAPYCTL_VLLM_ADMIN_KEY",
            "CAPYCTL_RENDEZVOUS_DIR",
        ] {
            assert!(!env.iter().any(|n| n == name), "{engine}: {name}");
        }
        assert!(!host.last_argv().iter().any(|a| a.contains("credential-fd")
            && a != "--launch-descriptor-fd"
            && a != "--observation-credential-fd"));
        let gone = host
            .terminate(&out.owned_handle, &out.processes)
            .await
            .unwrap();
        assert!(gone.all_gone(), "{engine}: {:?}", gone.result);
    }
}

// T30: the head launches with its loopback API and ingress.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn head_member_launches_with_ingress() {
    let host = FakeHost::new("host-a")
        .with_groups_policy(HEAD_PEER)
        .with_engine("sglang");
    let plan = two_member_plan(&host);
    let out = host.execute(launch(plan.clone())).await.unwrap();
    assert!(out.ingress.is_some(), "{:?}", out.result);
    assert!(out.result.model_usable && out.result.claim_retained);
    assert_eq!(out.processes[0].role, "api");
    assert!(has_launch_for(&host, &plan, 0));
    // ADR 0028 §10: SGLang takes its rank on the public settings the entry
    // checks, not as a flag (the brief's `--node-rank` is vLLM's spelling).
    let argv = host.last_argv();
    let public: Value = serde_json::from_str(
        &argv[argv
            .iter()
            .position(|a| a == "--public-settings-json")
            .unwrap()
            + 1],
    )
    .unwrap();
    let group = &public["settings"]["group"];
    assert_eq!(group["node_rank"], 0);
    assert_eq!(group["nnodes"], 2);
    assert_eq!(group["dist_init_addr"], format!("{HEAD_PEER}:{RENDEZVOUS}"));
    assert_eq!(group["host_ip"], HEAD_PEER);
    let gone = host
        .terminate(&out.owned_handle, &out.processes)
        .await
        .unwrap();
    assert!(gone.all_gone(), "{:?}", gone.result);
}

// T30: a plan that does not name this host, or whose Prepare checks fail now, spawns nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_without_this_host_is_refused() {
    let host = FakeHost::new("host-c").with_groups_policy("192.0.2.12");
    let other = FakeHost::new("host-b").with_groups_policy(WORKER_PEER);
    let plan = two_member_plan(&other);
    assert!(host.execute(launch(plan)).await.is_err());
    assert!(host.journal().history(0, 100).unwrap().is_empty());
    assert_eq!(host.spawns(), 0);

    // R7, R11: the member's peer address is on no local interface now.
    let host = FakeHost::new("host-b")
        .with_groups_policy(WORKER_PEER)
        .without_its_peer_address();
    let plan = two_member_plan(&host);
    assert_eq!(
        host.execute(launch(plan)).await.err().as_deref(),
        Some("peer_address_not_local")
    );
    assert!(host.journal().history(0, 100).unwrap().is_empty());
    assert_eq!(host.spawns(), 0);

    // Review Focus 2, R7: the head's rendezvous port is held outside CapyCTL.
    let host = FakeHost::new("host-a")
        .with_groups_policy(HEAD_PEER)
        .with_engine("sglang")
        .with_held_peer_port(RENDEZVOUS);
    let plan = two_member_plan(&host);
    assert_eq!(
        host.execute(launch(plan)).await.err().as_deref(),
        Some(format!("rendezvous_port_in_use:{RENDEZVOUS}").as_str())
    );
    assert!(host.journal().history(0, 100).unwrap().is_empty());
    assert_eq!(host.spawns(), 0);

    // T03, R35 (ADR 0028 §2): a plan giving a member more than one rank is
    // refused typed, before anything is journaled or spawned.
    let host = FakeHost::new("host-b").with_groups_policy(WORKER_PEER);
    let one = two_member_plan(&host);
    let mut members = one.members().to_vec();
    for member in &mut members {
        member.devices.push("gpu1".into());
    }
    let two = GroupPlan::new(
        one.engine(),
        members,
        GroupTopology {
            tensor_parallel: 4,
            pipeline_parallel: 1,
            local_ranks: 2,
        },
        one.rendezvous_port(),
        one.generation(),
    )
    .unwrap();
    assert_eq!(
        host.execute(launch(two)).await.err().as_deref(),
        Some("group_topology_invalid")
    );
    assert!(host.journal().history(0, 100).unwrap().is_empty());
    assert_eq!(host.spawns(), 0);
}

// Review Focus 6, T31: a SGLang worker that ignores SIGTERM is killed and settles, escalation recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sglang_worker_ignoring_sigterm_is_escalated() {
    let host = FakeHost::new("host-b")
        .with_groups_policy(WORKER_PEER)
        .with_engine("sglang")
        .with_fake_ignoring_sigterm();
    let plan = two_member_plan(&host);
    let out = host.execute(launch(plan)).await.unwrap();
    let started = std::time::Instant::now();
    let gone = host
        .terminate(&out.owned_handle, &out.processes)
        .await
        .unwrap();
    assert!(gone.all_gone(), "{:?}", gone.result);
    assert!(gone.escalated);
    // Bounded: the grace, then SIGKILL and its proof, never an endless wait.
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
}

/// The pid a late child of the fake engine recorded under `label`.
fn late_pid(host: &FakeHost, label: &str) -> u32 {
    let path = host.host().root.path();
    let files = [
        path.join("venv/bin").join(label),
        path.join("runtime").join(label),
    ];
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if let Some(file) = files.iter().find(|file| file.exists()) {
            return std::fs::read_to_string(file)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
        }
        assert!(std::time::Instant::now() < deadline, "{label} was started");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Whether `pid` has ended (gone, or a zombie nobody reaped yet).
fn ended(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
    }
}

// T31, T33 (ADR 0028 §8, §11): a child the worker starts just after its spawn
// is in the Launch reply and the journal. With the leader dead and reaped,
// Terminate signals recorded identities only: the recorded child ends, and a
// child started after the tree settled (never recorded) is never signalled;
// it keeps the stop uncertain, with the claim retained.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_worker_children_are_recorded_and_unrecorded_survivors_stay_uncertain() {
    let host = FakeHost::new("host-b")
        .with_groups_policy(WORKER_PEER)
        .with_late_children();
    let out = host.execute(launch(two_member_plan(&host))).await.unwrap();
    let early = late_pid(&host, "late-1");
    assert!(
        out.processes
            .iter()
            .any(|p| p.pid == early && p.role.starts_with("worker-1/")),
        "{:?}",
        out.processes
    );
    assert!(host
        .journal()
        .inspect_owned(&out.owned_handle)
        .unwrap()
        .iter()
        .any(|(p, _)| p.pid == early));
    let later = late_pid(&host, "late-2");
    assert!(!out.processes.iter().any(|p| p.pid == later));
    // Fault injection on the recorded leader: it dies and leaves its tree.
    let leader = out.processes.iter().find(|p| p.role == "worker-1").unwrap();
    let leader = capyctl_domain::completion::ProcessIdentity {
        role: "api".into(),
        pid: leader.pid,
        boot_id: leader.boot_id.clone(),
        start_ticks: leader.start_ticks,
    };
    // SAFETY: kill has no memory preconditions; the pid is the recorded leader.
    assert_eq!(unsafe { libc::kill(leader.pid as i32, libc::SIGKILL) }, 0);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while capyctl_launchers::process_absence::presence(&leader)
        != capyctl_domain::completion::Presence::Gone
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the leader was never reaped"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    // (b) The unrecorded survivor makes the stop uncertain: no gone report.
    assert!(host
        .terminate(&out.owned_handle, &out.processes)
        .await
        .is_err());
    // (a) The recorded child was signalled and ended.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !ended(early) {
        assert!(
            std::time::Instant::now() < deadline,
            "a recorded child survived"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    // A leaderless group scan never authorizes a signal: it is still running.
    assert!(!ended(later), "an unrecorded process was signalled");
    assert!(host
        .journal()
        .claimed_launches("")
        .unwrap()
        .iter()
        .any(|claim| claim.command.identity.command_id == out.owned_handle));
    // The test ends the process it injected the fault into.
    // SAFETY: kill has no memory preconditions; the pid is the fake's child.
    unsafe { libc::kill(later as i32, libc::SIGKILL) };
}

// T31, T33: a worker terminates its own recorded tree only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_member_terminates_its_recorded_tree() {
    let host = FakeHost::new("host-b").with_groups_policy(WORKER_PEER);
    let plan = two_member_plan(&host);
    let out = host.execute(launch(plan)).await.unwrap();
    let gone = host
        .terminate(&out.owned_handle, &out.processes)
        .await
        .unwrap();
    assert!(gone.all_gone(), "{:?}", gone.result);
    assert!(!gone.escalated);
    // The gone report names exactly the identities the Launch reported.
    let named = |processes: &[pb::OwnedProcessObservation]| {
        let mut named: Vec<_> = processes
            .iter()
            .map(|p| (p.role.clone(), p.pid, p.start_ticks))
            .collect();
        named.sort();
        named
    };
    assert_eq!(named(&gone.result.processes), named(&out.processes));
}

// T31 (ADR 0028 §11): a worker never records readiness, yet its exit is
// watched from its Launch on and reported as a `MemberExit` naming its own
// Launch and the member role its Launch reported; a live worker reports none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_exit_is_reported_under_its_member_role() {
    let host = FakeHost::new("host-b").with_groups_policy(WORKER_PEER);
    let plan = two_member_plan(&host);
    let out = host.execute(launch(plan.clone())).await.unwrap();
    let scan = || capyctl_agent::exits::scan(host.journal(), "host-b", now_ms());
    assert!(scan().is_empty(), "a live worker has not exited");
    let leader = out.processes.iter().find(|p| p.role == "worker-1").unwrap();
    // SAFETY: kill has no memory preconditions; the pid is the recorded leader.
    assert_eq!(unsafe { libc::kill(leader.pid as i32, libc::SIGKILL) }, 0);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let exited = loop {
        let exited = scan();
        if !exited.is_empty() {
            break exited;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the exit was never reported"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let exit = &exited[0].exit;
    assert_eq!(exit.owned_handle, out.owned_handle);
    assert_eq!(exit.generation, plan.generation());
    assert_eq!(exit.process.role, "worker-1");
    assert_eq!(exit.process.pid, leader.pid);
    // An exit report releases nothing: the claim stays until Terminate.
    assert!(host
        .journal()
        .claimed_launches("")
        .unwrap()
        .iter()
        .any(|claim| claim.command.identity.command_id == out.owned_handle));
    let _ = host.terminate(&out.owned_handle, &out.processes).await;
}

// T21 (R11): a vLLM or SGLang member renders the interface holding its peer
// address as GLOO_SOCKET_IFNAME and its own address, and no NCCL_* or
// MASTER_* name; TensorFold renders none of them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn members_render_the_interface_of_their_peer_address() {
    for engine in ["vllm", "sglang", "tensorfold"] {
        let host = FakeHost::new("host-b")
            .with_groups_policy(WORKER_PEER)
            .with_engine(engine);
        let out = host.execute(launch(two_member_plan(&host))).await.unwrap();
        let record = host.record();
        let env: Vec<String> = serde_json::from_value(record["env"].clone()).unwrap();
        assert!(
            !env.iter()
                .any(|name| name.starts_with("NCCL_") || name.starts_with("MASTER_")),
            "{engine}: {env:?}"
        );
        if engine == "tensorfold" {
            assert!(record["gloo"].is_null(), "{engine}");
            assert!(record["host_ip"].is_null(), "{engine}");
        } else {
            assert_eq!(record["gloo"], IFNAME, "{engine}");
            assert_eq!(record["host_ip"], WORKER_PEER, "{engine}");
        }
        host.terminate(&out.owned_handle, &out.processes)
            .await
            .unwrap();
    }
}

// T22 (R12): a deep SGLang worker launches with this host's saver
// observation directory and its own observation credential.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deep_sglang_worker_carries_its_observation_target() {
    let host = FakeHost::new("host-b")
        .with_groups_policy(WORKER_PEER)
        .with_engine("sglang")
        .with_deep_park();
    let out = host.execute(launch(two_member_plan(&host))).await.unwrap();
    let record = host.record();
    assert_eq!(record["worker"], true, "{record}");
    assert_eq!(record["observation_dir"], true);
    assert_eq!(record["observation_fd"], true);
    let gone = host
        .terminate(&out.owned_handle, &out.processes)
        .await
        .unwrap();
    assert!(gone.all_gone(), "{:?}", gone.result);
}

// T30, T33 (R23): a Launch sent again for the same member (deployment,
// instance, generation, rank) never starts a second tree; it answers with
// the identities recorded the first time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_launch_sent_again_replays_its_recorded_identities() {
    let host = FakeHost::new("host-b").with_groups_policy(WORKER_PEER);
    let plan = two_member_plan(&host);
    let command = host.launch_command("launch", &plan);
    let first = host.send(&command).await.unwrap();
    let again = host.send(&command).await.unwrap();
    host.record();
    assert_eq!(host.spawns(), 1);
    assert_eq!(again.processes, first.processes);
    assert_eq!(again.owned_handle, first.owned_handle);
    let gone = host
        .terminate(&first.owned_handle, &first.processes)
        .await
        .unwrap();
    assert!(gone.all_gone(), "{:?}", gone.result);
}

// T30 (decided 2026-10-06): the head's agent answers a completion probe with the generated token ids.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn head_answers_a_completion_probe_with_token_ids() {
    // Each engine's own request form (vLLM `/v1/completions`, SGLang
    // `/generate`); TensorFold's is pinned by the adapter test.
    for engine in ["vllm", "sglang"] {
        let host = FakeHost::new("host-a")
            .with_groups_policy(HEAD_PEER)
            .with_engine(engine)
            .with_fake_completion(vec![7, 8]);
        let plan = two_member_plan(&host);
        let out = host.execute(launch(plan)).await.unwrap();
        assert!(out.result.model_usable, "{engine}: {:?}", out.result);
        let reply = host
            .execute(MemberAction::Probe {
                owned_handle: out.owned_handle.clone(),
                max_tokens: Some(2),
            })
            .await
            .unwrap();
        assert_eq!(reply.result.probe_tokens, vec![7, 8], "{engine}");
        assert!(
            host.last_engine_request().is_loopback_with_launch_key(),
            "{engine}"
        );
        // The bound is honoured: one token asked, one answered.
        let one = host
            .execute(MemberAction::Probe {
                owned_handle: out.owned_handle.clone(),
                max_tokens: Some(1),
            })
            .await
            .unwrap();
        assert_eq!(one.result.probe_tokens, vec![7], "{engine}");
        // A readiness probe carries no tokens.
        let ready = host
            .execute(MemberAction::Probe {
                owned_handle: out.owned_handle.clone(),
                max_tokens: None,
            })
            .await
            .unwrap();
        assert!(ready.result.probe_tokens.is_empty(), "{engine}");
        let gone = host
            .terminate(&out.owned_handle, &out.processes)
            .await
            .unwrap();
        assert!(gone.all_gone(), "{engine}: {:?}", gone.result);
    }
}

// T30 (decided 2026-10-06): a worker is never probed: a completion probe of a
// worker's launch answers no tokens.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_answers_no_completion_probe() {
    let host = FakeHost::new("host-b")
        .with_groups_policy(WORKER_PEER)
        .with_engine("sglang")
        .with_fake_completion(vec![7, 8]);
    let plan = two_member_plan(&host);
    let out = host.execute(launch(plan)).await.unwrap();
    let reply = host
        .execute(MemberAction::Probe {
            owned_handle: out.owned_handle.clone(),
            max_tokens: Some(2),
        })
        .await
        .unwrap();
    assert!(reply.result.probe_tokens.is_empty());
    let gone = host
        .terminate(&out.owned_handle, &out.processes)
        .await
        .unwrap();
    assert!(gone.all_gone(), "{:?}", gone.result);
}

// T30 (decided 2026-10-06, OD6): a completion probe sent again after a lost
// reply answers its token ids again, so a lost reply never reads as a failed
// probe of a healthy group.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replayed_completion_probe_answers_its_tokens() {
    let host = FakeHost::new("host-a")
        .with_groups_policy(HEAD_PEER)
        .with_engine("vllm")
        .with_fake_completion(vec![7, 8]);
    let plan = two_member_plan(&host);
    let out = host.execute(launch(plan)).await.unwrap();
    assert!(out.result.model_usable, "{:?}", out.result);
    let probe = host.probe_command(out.owned_handle.clone(), Some(2));
    let first = host.send_probe(&probe).await.unwrap();
    assert_eq!(first.result.probe_tokens, vec![7, 8]);
    // The exact command again: the journal answers it as a replay.
    let again = host.send_probe(&probe).await.unwrap();
    assert_eq!(again.result.state, "completed", "{:?}", again.result);
    assert_eq!(again.result.probe_tokens, vec![7, 8]);
    let gone = host
        .terminate(&out.owned_handle, &out.processes)
        .await
        .unwrap();
    assert!(gone.all_gone(), "{:?}", gone.result);
}

// T20, T22 (ADR 0028 §12, R41): a deep group's head parks and restores in place
// through the agent's own admission and journal, exactly as a single launch:
// the collective is invoked once on its loopback control endpoint, the claim
// and the process group are kept, a park never claims a usable model, and the
// restore reopens the head only after a fresh model probe. A replay is answered
// from the journal and never repeats the collective.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deep_group_head_parks_and_restores_in_place() {
    for engine in ["vllm", "sglang"] {
        let host = FakeHost::new("host-a")
            .with_groups_policy(HEAD_PEER)
            .with_engine(engine)
            .with_deep_park_and_stub_saver();
        let out = host.execute(launch(two_member_plan(&host))).await.unwrap();
        assert!(out.result.model_usable, "{engine}: {:?}", out.result);
        let handle = out.owned_handle.clone();

        let park = host.residency_command("park", &handle, true);
        let parked = host.send_residency(&park).await;
        assert_eq!(
            (parked.state.as_str(), residency_state(&parked)),
            ("completed", "parked"),
            "{engine}: {parked:?}"
        );
        assert!(parked.claim_retained && !parked.model_usable, "{engine}");
        assert_eq!(parked.owned_handle, handle, "{engine}");
        assert!(
            parked.processes.iter().all(|p| p.presence == "alive"),
            "{engine}"
        );
        assert_eq!(
            host.journal().residency_of(&handle).unwrap().as_deref(),
            Some("parked"),
            "{engine}"
        );
        let collective = match engine {
            "vllm" => vec!["sleep:2"],
            _ => vec!["/release_memory_occupation"],
        };
        assert_eq!(host.controls(), collective, "{engine}");
        // An exact resend is answered from the journal; nothing is slept again.
        let replay = host.send_residency(&park).await;
        assert_eq!(residency_state(&replay), "parked", "{engine}");
        assert_eq!(host.controls(), collective, "{engine}");

        let restore = host.residency_command("restore", &handle, false);
        let restored = host.send_residency(&restore).await;
        assert_eq!(
            residency_state(&restored),
            "restored",
            "{engine}: {restored:?}"
        );
        assert!(restored.claim_retained && restored.model_usable, "{engine}");
        assert_eq!(
            host.controls(),
            match engine {
                "vllm" => vec![
                    "sleep:2",
                    "wake:weights",
                    "collective:reload_weights",
                    "wake:kv_cache",
                    "reset_prefix_cache",
                ],
                _ => vec![
                    "/release_memory_occupation",
                    "/resume_memory_occupation",
                    "/update_weights_from_disk",
                    "/flush_cache",
                ],
            },
            "{engine}"
        );
        assert_eq!(host.journal().residency_of(&handle).unwrap(), None);
        if engine == "sglang" {
            // SPEC §9.2: the head's own saver map, for its own launch, proved
            // the release and the resume.
            let scopes = host.host().saver.as_ref().unwrap().scopes.lock().unwrap();
            assert!(!scopes.is_empty());
            assert!(scopes
                .iter()
                .all(|scope| scope.binding_id == "01K00000000000000000000001"));
        }
        let gone = host.terminate(&handle, &out.processes).await.unwrap();
        assert!(gone.all_gone(), "{engine}: {:?}", gone.result);
    }
}

// T20 (ADR 0028 §12, OD3): the head's agent alone takes Park and Restore. A
// worker's own Park or Restore is refused before anything is journaled, and
// its engine is never touched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_group_worker_never_parks_on_its_own() {
    for engine in ["vllm", "sglang"] {
        let host = FakeHost::new("host-b")
            .with_groups_policy(WORKER_PEER)
            .with_engine(engine)
            .with_deep_park_and_stub_saver();
        let out = host.execute(launch(two_member_plan(&host))).await.unwrap();
        let handle = out.owned_handle.clone();
        for (id, park) in [("park", true), ("restore", false)] {
            let command = host.residency_command(id, &handle, park);
            let refused = host.send_residency(&command).await;
            assert_eq!(residency_state(&refused), "unchanged", "{engine} {id}");
            assert_eq!(refused.refused, "unauthorized", "{engine} {id}");
            assert!(!refused.model_usable, "{engine} {id}");
        }
        assert_eq!(host.journal().residency_of(&handle).unwrap(), None);
        assert!(host.controls().is_empty(), "{engine}");
        let gone = host.terminate(&handle, &out.processes).await.unwrap();
        assert!(gone.all_gone(), "{engine}: {:?}", gone.result);
    }
}

// T22 (ADR 0028 §2, §12, OD3): a TensorFold group is restart-only. Its head
// refuses a Park with the closed `residency_tier` before anything is
// journaled; the group parks by a group stop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tensorfold_group_head_refuses_park_as_restart_only() {
    let host = FakeHost::new("host-a")
        .with_groups_policy(HEAD_PEER)
        .with_engine("tensorfold");
    let out = host.execute(launch(two_member_plan(&host))).await.unwrap();
    let handle = out.owned_handle.clone();
    let park = host.residency_command("park", &handle, true);
    let refused = host.send_residency(&park).await;
    assert_eq!(residency_state(&refused), "unchanged", "{refused:?}");
    assert_eq!(refused.refused, "residency_tier");
    assert_eq!(host.journal().residency_of(&handle).unwrap(), None);
    let gone = host.terminate(&handle, &out.processes).await.unwrap();
    assert!(gone.all_gone(), "{:?}", gone.result);
}

/// Every process in process group `group` now, from `/proc`.
fn group_pids(group: u32) -> Vec<u32> {
    std::fs::read_dir("/proc")
        .unwrap()
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| {
                    let fields: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
                    fields.get(2)?.parse::<u32>().ok()
                })
                == Some(group)
        })
        .collect()
}

// T20 (ADR 0007, ADR 0028 §12): a vLLM worker member's processes are in its own
// host's `process_residency` report under the identities its Launch reported
// (pid, boot, start), so that report is the member's own park evidence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_vllm_worker_members_processes_are_in_its_residency_report() {
    let leader = Arc::new(OnceLock::<u32>::new());
    let seen = leader.clone();
    // `nvidia-smi` lists every process of the engine that holds GPU memory:
    // here, every process of the worker's process group (the spawned leader
    // and the child it forks, as vLLM's GPU worker is).
    let gpu: Arc<GpuCollector> = Arc::new(move || {
        let leader = *seen.get()?;
        Some(
            group_pids(leader)
                .into_iter()
                .map(|pid| (pid, 64 << 20))
                .collect(),
        )
    });
    let host = FakeHost::new("host-b")
        .with_groups_policy(WORKER_PEER)
        .with_engine("vllm")
        .with_gpu_processes(gpu);
    let out = host.execute(launch(two_member_plan(&host))).await.unwrap();
    host.record();
    leader.set(out.processes[0].pid).unwrap();
    // ADR 0007: one sample now, which the next availability refresh reports.
    let sampler = host.host().sampler.clone().unwrap();
    assert!(!sampler.sample_fresh().is_empty());
    let inventory = host
        .host()
        .executor
        .inventory()
        .expect("the host reports its availability");
    let residents: Vec<pb::ProcessResidency> = inventory
        .domains
        .iter()
        .flat_map(|domain| domain.residents.clone())
        .collect();
    assert!(!residents.is_empty(), "{inventory:?}");
    for resident in &residents {
        assert!(
            out.processes.iter().any(|p| p.pid == resident.pid
                && p.boot_id == resident.boot_id
                && p.start_ticks == resident.start_ticks),
            "{resident:?} is one of the member's reported processes: {:?}",
            out.processes
        );
    }
    // The forked child, not only the spawned leader, is reported.
    assert!(residents.iter().any(|r| r.pid != out.processes[0].pid));
    let gone = host
        .terminate(&out.owned_handle, &out.processes)
        .await
        .unwrap();
    assert!(gone.all_gone(), "{:?}", gone.result);
}

/// ADR 0028 §12 (R12): the saver-map facts `host` next reports on its session
/// for its member launch `owned_handle`, observed at or after `since`.
async fn reported_saver(host: &FakeHost, owned_handle: &str, since: i64) -> pb::MemberSaver {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let inventory = host.host().executor.inventory().unwrap_or_default();
        if let Some(saver) = inventory
            .member_savers
            .into_iter()
            .find(|s| s.owned_handle == owned_handle && s.observed_at_unix_ms >= since)
        {
            return saver;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{owned_handle} reported its saver map"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

// T20, T22 (ADR 0028 §12, R12): a deep SGLang worker's own host reports its
// member's saver map on its session, read from its own observation directory
// with the member's own credential and recorded processes: every byte mapped
// while resident, none once the head's collective released the rank.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deep_sglang_worker_reports_its_saver_map_on_its_session() {
    let host = FakeHost::new("host-b")
        .with_groups_policy(WORKER_PEER)
        .with_engine("sglang")
        .with_deep_park_and_stub_saver();
    let out = host.execute(launch(two_member_plan(&host))).await.unwrap();
    let handle = out.owned_handle.clone();
    let resident = reported_saver(&host, &handle, now_ms()).await;
    assert_eq!(resident.mapped_bytes, 2 << 20);
    {
        let scopes = host.host().saver.as_ref().unwrap().scopes.lock().unwrap();
        let scope = scopes.last().unwrap();
        let member = host.journal().retained_command(&handle).unwrap();
        let plan = member.action.launch_plan().unwrap();
        assert_eq!(
            (scope.binding_id.as_str(), scope.incarnation.as_str()),
            (plan.binding_id.as_str(), plan.incarnation.as_str())
        );
        assert!(!scope.admin_key.is_empty());
        let members = scope.members.clone().unwrap();
        assert!(out.processes.iter().all(|p| members
            .iter()
            .any(|m| m.pid == p.pid && m.boot_id == p.boot_id && m.start_ticks == p.start_ticks)));
    }
    // The head's collective released every rank, this one included.
    std::fs::write(host.host().root.path().join("runtime/released"), "").unwrap();
    let released = reported_saver(&host, &handle, now_ms()).await;
    assert_eq!(released.mapped_bytes, 0);
    let gone = host.terminate(&handle, &out.processes).await.unwrap();
    assert!(gone.all_gone(), "{:?}", gone.result);
}
