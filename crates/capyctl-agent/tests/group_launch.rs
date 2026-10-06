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
    native_execution::{EnrolledSaver, NativeHostExecution},
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
/// start, never a key. A worker forks one child and waits; the SGLang head
/// serves the keyed model list and chat. With `ignore-sigterm` beside it the
/// whole tree ignores SIGTERM, as SGLang's rank > 0 does. It ends itself when
/// the test process or the fixture directory goes away.
const FAKE_ENGINE: &str = r#"
import json, os, signal, sys, threading, time, http.server
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
if worker or engine != "sglang":
    while True:
        time.sleep(0.5)
port = int(settings["endpoint"].rsplit(":", 1)[1])
served = settings["served_name"]
fd = int(args[args.index("--inference-credential-fd") + 1])
key = os.pread(fd, 4096, 0).decode().strip()

class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"
    def log_message(self, *a):
        pass
    def keyed(self):
        if not key or self.headers.get("Authorization") != "Bearer " + key:
            self.send_response(401); self.send_header("Content-Length", "0"); self.end_headers(); return False
        return True
    def send_body(self, body):
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers(); self.wfile.write(body)
    def do_GET(self):
        if not self.keyed(): return
        if self.path == "/v1/models":
            self.send_body(json.dumps({"object": "list", "data": [{"id": served, "object": "model"}]}).encode())
        else:
            self.send_response(404); self.send_header("Content-Length", "0"); self.end_headers()
    def do_POST(self):
        if not self.keyed(): return
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
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

/// `count` consecutive loopback ports that are free now.
fn free_ports(count: u16) -> u16 {
    for _ in 0..200 {
        let base = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        if base.checked_add(count).is_none() {
            continue;
        }
        let held: Vec<_> = (base..base + count)
            .map(|port| std::net::TcpListener::bind(("127.0.0.1", port)))
            .collect();
        if held.iter().all(Result::is_ok) {
            return base;
        }
    }
    panic!("no run of free loopback ports");
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
    deep: bool,
    /// Whether the substituted host holds its own peer address.
    holds_peer: bool,
    /// Peer ports held outside CapyCTL.
    held: Vec<u16>,
    built: OnceLock<Built>,
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
            deep: false,
            holds_peer: true,
            held: Vec::new(),
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

    fn host(&self) -> &Built {
        self.built.get_or_init(|| self.build())
    }

    fn build(&self) -> Built {
        let root = directory();
        let path = root.path();
        let base_port = free_ports(3);
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
        if self.ignore_sigterm {
            for dir in [&bin, &runtime] {
                std::fs::write(dir.join("ignore-sigterm"), "").unwrap();
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
            Default::default(),
        )
        .with_host_probes(probes)
        // SPEC §8.2: a single SGLang launch would keep its rendezvous here;
        // a group member never does.
        .with_rendezvous_root(private(&state.join("rendezvous")))
        .with_engine_cache_root(private(&path.join("engines")));
        if self.deep {
            executor = executor.with_saver_residency(Arc::new(EnrolledSaver::new(private(
                &state.join("observation"),
            ))));
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
        let MemberAction::Launch { plan, .. } = action else {
            panic!("only a group Launch is sent here")
        };
        self.send(&self.launch_command("launch", &plan)).await
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
