//! ADR 0028 §12: a deep vLLM group and a deep SGLang group park and wake end to
//! end over the production path: the controller's `park_group` and
//! `wake_group` through `AgentGroupHosts` and `AgentSessions`, over two real,
//! enrolled agent sessions (mutual TLS on loopback), each host a real agent
//! executor (journal, admission, ingress, durable launcher) whose engine is a
//! small Python stand-in.
//!
//! Only the head's agent takes the Park and the Restore, and admits them under
//! its own policy and journal. Every member's evidence is its own host's
//! report on its session: a vLLM member's processes in the host's
//! `process_residency`, a SGLang member's saver map from the host's own
//! observation directory. The stand-ins model the collective reaching every
//! rank: each host's GPU listing and saver map follow the head's engine.
//!
//! CPU and fake-engine tests are not qualification: nothing here shows that a
//! group of any engine parks across two hosts; the live MN rows do.

use capyctl_agent::{
    enrollment::{JoinInvitation, PendingEnrollment},
    host_checks::HostProbes,
    identity::{CertificateAuthority, HostKey},
    identity_storage::IdentityDirectory,
    ingress::Ingress,
    ingress_identity::IngressIdentities,
    journal::HostJournal,
    native_execution::{
        NativeHostExecution, SaverMapped, SaverResidency, SaverScope, SaverUnavailable,
    },
    process_residency::{GpuCollector, ResidencySampler},
};
use capyctl_config::{effective::Residency, remote_roles::HostConfig};
use capyctl_controller::{
    agent_sessions::{AgentSessions, ProvisionOutcome},
    enrollment::EnrollmentAuthority,
    group_activation::{GroupCtx, GroupHosts},
    group_residency::{park_group, wake_group, ResidencyTarget},
    group_settlement::GroupTarget,
    remote_execution::AgentGroupHosts,
    OwnedCoordinatorState,
};
use capyctl_domain::group::{
    member_id, CommandIdentity, GroupEngine, GroupPlan, GroupTopology, MemberKey, MemberPlan,
    MemberRole,
};
use capyctl_protocol::{
    execution::{MemberAction, MemberCommand, SingleLaunchPlan},
    pb::{self, agent_control_server::AgentControlServer, bootstrap_server::Bootstrap},
};
use capyctl_store::{groups::member_owner_id, ordinary_lifecycle::park::ArmedMember};
use serde_json::{json, Value};
use std::{
    net::IpAddr,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};

const HEAD_PEER: &str = "192.0.2.10";
const WORKER_PEER: &str = "192.0.2.11";
const IFNAME: &str = "eth9";
const RENDEZVOUS: u16 = 25000;
const GATE: [u8; 32] = [7; 32];
const DEPLOYMENT: &str = "deployment";
const OPERATION: &str = "operation";
/// The token ids the head's engine generates for any prompt, so the wake's
/// canary repeats the reference exactly.
const COMPLETION: [u32; 8] = [11, 12, 13, 14, 15, 16, 17, 18];
/// What one member's engine holds on its GPU while resident.
const GPU_RESIDENT: i64 = 4 << 30;
/// A member's parked budget in this test: above what a parked stand-in's
/// processes keep (a few Python interpreters' pages), below its resident GPU
/// memory or saver map.
const PARKED_BUDGET: i64 = 1 << 30;
/// The bytes a resident SGLang member's saver maps (each of two tags).
const SAVER_TAG: u64 = 2 << 30;

/// One stand-in for vLLM and SGLang, chosen by the file it runs as (vLLM
/// through capyctl's protected entry, `--headless` on a worker; SGLang as the
/// protected `sglang_entry.py`). A worker forks one child and waits. The head
/// serves the keyed model list, chat, completions and the engine's residency
/// controls (the group's collective), keyed with the admin key, and keeps the
/// `released` marker beside it while its memory is released: the hosts'
/// GPU listings and saver maps read it, as every rank follows the head's
/// collective. Control calls are appended to `residency.log`. It ends itself
/// when the test process or the fixture directory goes away.
const FAKE_ENGINE: &str = r#"
import json, os, signal, sys, threading, time, http.server, urllib.parse
args = sys.argv[1:]
here = os.path.dirname(os.path.abspath(__file__))
if os.path.basename(__file__) == "sglang_entry.py":
    engine = "sglang"
    settings = json.loads(args[args.index("--public-settings-json") + 1])
    worker = (settings.get("settings", {}).get("group") or {}).get("node_rank", 0) > 0
else:
    engine = "vllm"
    worker = "--headless" in args
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
if worker:
    while True:
        time.sleep(0.5)
if engine == "sglang":
    port = int(settings["endpoint"].rsplit(":", 1)[1])
    served = settings["served_name"]
    key = os.pread(int(args[args.index("--inference-credential-fd") + 1]), 4096, 0).decode().strip()
    admin = os.pread(int(args[args.index("--admin-credential-fd") + 1]), 4096, 0).decode().strip()
else:
    port = int(args[args.index("--port") + 1])
    served = args[args.index("--served-model-name") + 1]
    key = os.environ.get("VLLM_API_KEY", "")
    admin = os.environ.get("CAPYCTL_VLLM_ADMIN_KEY", "")
marker = os.path.join(here, "released")
state = {"sleeping": False, "weights": True, "loaded": True, "kv": True}
def logged(call):
    with open(os.path.join(here, "residency.log"), "a") as f:
        f.write(call + "\n")
def usable():
    return state["weights"] and state["loaded"] and state["kv"] and not os.path.exists(marker)
def released(value):
    if value:
        open(marker, "w").close()
    elif os.path.exists(marker):
        os.remove(marker)
FLUSH = "Cache flushed.\nPlease check backend logs for more details. (When there are running or waiting requests, the operation will not be performed.)\n"
with open(os.path.join(here, "completion")) as f:
    COMPLETION = json.load(f)

class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"
    def log_message(self, *a):
        pass
    def refuse(self, code):
        self.send_response(code); self.send_header("Content-Length", "0"); self.end_headers()
    def keyed(self, want):
        if not want or self.headers.get("Authorization") != "Bearer " + want:
            self.refuse(401); return False
        return True
    def send(self, body, kind="application/json"):
        self.send_response(200)
        self.send_header("Content-Type", kind)
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
            elif call == "wake:weights":
                state["weights"] = True
            elif call == "wake:kv_cache":
                state["kv"] = True
            elif call == "collective:reload_weights" and state["weights"]:
                state["loaded"] = True
            elif call == "reset_prefix_cache":
                self.send(json.dumps({"success": True}).encode()); return
            else:
                self.refuse(409); return
            state["sleeping"] = not (state["weights"] and state["loaded"] and state["kv"])
            released(state["sleeping"])
            self.send(b"{}"); return
        logged(path)
        if path == "/release_memory_occupation":
            released(True); self.send(b"null")
        elif path == "/resume_memory_occupation":
            released(False); self.send(b"null")
        elif path == "/update_weights_from_disk":
            self.send(json.dumps({"success": True}).encode())
        else:
            self.send(FLUSH.encode(), "text/plain")
    def do_GET(self):
        if self.path == "/is_sleeping":
            if self.keyed(admin):
                self.send(json.dumps({"is_sleeping": state["sleeping"]}).encode())
            return
        if not self.keyed(key): return
        if self.path == "/v1/models":
            self.send(json.dumps({"object": "list", "data": [{"id": served, "object": "model"}]}).encode())
        elif self.path == "/metrics":
            if engine == "vllm":
                text = "vllm:num_requests_running{engine=\"0\"} 0.0\nvllm:num_requests_waiting{engine=\"0\"} 0.0\n"
            else:
                text = "sglang:num_running_reqs 0.0\nsglang:num_queue_reqs 0.0\n"
            self.send(text.encode(), "text/plain")
        else:
            self.refuse(404)
    def do_POST(self):
        url = urllib.parse.urlparse(self.path)
        if url.path in ("/sleep", "/wake_up", "/collective_rpc", "/reset_prefix_cache",
                        "/release_memory_occupation", "/resume_memory_occupation",
                        "/update_weights_from_disk", "/flush_cache"):
            if self.keyed(admin):
                body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
                self.control(url.path, urllib.parse.parse_qs(url.query), body)
            return
        if not self.keyed(key): return
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
        if not usable() or body.get("model", served) != served:
            self.refuse(503); return
        if self.path == "/v1/completions":
            self.send(json.dumps({"choices": [{"index": 0, "text": "ok", "token_ids": COMPLETION[:body["max_tokens"]]}]}).encode()); return
        if self.path == "/generate":
            self.send(json.dumps({"text": "ok", "output_ids": COMPLETION[:body["sampling_params"]["max_new_tokens"]]}).encode()); return
        if self.path != "/v1/chat/completions":
            self.refuse(404); return
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
        self.send(json.dumps({"id": "c1", "object": "chat.completion", "created": 1, "model": served,
                              "choices": [{"index": 0, "finish_reason": "stop",
                                           "message": {"role": "assistant", "content": "group head"}}]}).encode())

http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
"#;

fn now() -> i64 {
    capyctl_protocol::now_unix_ms() / 1000
}

fn directory() -> tempfile::TempDir {
    let d = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
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

async fn eventually(what: &str, mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(20), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}"));
}

/// SPEC §9.2, ADR 0028 §12 (R12): a host's saver observation of the stand-in
/// SGLang engine: every allocation mapped until the head's engine released
/// its memory occupation (its `released` marker), none after.
struct StubSaver {
    dir: PathBuf,
    head_marker: PathBuf,
}
impl SaverResidency for StubSaver {
    fn mapped(&self, scope: &SaverScope) -> Result<SaverMapped, SaverUnavailable> {
        if scope.admin_key.is_empty() || scope.members.as_ref().is_none_or(Vec::is_empty) {
            return Err(SaverUnavailable);
        }
        let mapped = if self.head_marker.exists() {
            0
        } else {
            SAVER_TAG
        };
        Ok(SaverMapped {
            real_saver: true,
            weight_bytes: mapped,
            kv_bytes: mapped,
            weight_virtual_bytes: SAVER_TAG,
            kv_virtual_bytes: SAVER_TAG,
        })
    }
    fn observation_dir(&self) -> Option<&Path> {
        Some(&self.dir)
    }
}

/// Every process of process group `group` now, from `/proc`.
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

/// One enrolled member host: its fixture, its executor and its session.
struct MemberHost {
    host_id: String,
    root: tempfile::TempDir,
    config: HostConfig,
    deployment: Value,
    service_port: u16,
    worker_port: u16,
    journal: Arc<HostJournal>,
    /// The engine leader whose process group this host's GPU listing shows.
    leader: Arc<OnceLock<u32>>,
    stop: tokio::sync::watch::Sender<bool>,
    _keys: tempfile::TempDir,
}

impl MemberHost {
    fn effective(&self) -> capyctl_config::effective::EffectiveDeployment {
        let local =
            capyctl_config::remote_resources::local_host_document(&self.config.document).unwrap();
        capyctl_config::effective::resolve_effective(&self.deployment, &local).unwrap()
    }

    fn digest(&self) -> String {
        let local =
            capyctl_config::remote_resources::local_host_document(&self.config.document).unwrap();
        let location =
            capyctl_config::effective::checkpoint_location(&self.deployment, &local).unwrap();
        capyctl_agent::checkpoint::CheckpointVerifier::in_memory()
            .measure(&location.model_store, &location.checkpoint)
            .unwrap()
            .manifest
            .digest
    }

    fn model_path(&self) -> String {
        self.root
            .path()
            .join("models/toy")
            .to_string_lossy()
            .into_owned()
    }

    /// The residency controls this host's engine received, in order.
    fn controls(&self) -> Vec<String> {
        let path = self.root.path();
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

/// Two hosts enrolled with one controller, each with its live session.
struct World {
    sessions: Arc<AgentSessions>,
    state: Arc<Mutex<OwnedCoordinatorState>>,
    controller_id: String,
    hosts: Vec<MemberHost>,
    engine: GroupEngine,
    server: tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
    _dirs: Vec<tempfile::TempDir>,
}

impl Drop for World {
    fn drop(&mut self) {
        for host in &self.hosts {
            let _ = host.stop.send(true);
        }
        self.server.abort();
    }
}

impl World {
    async fn new(engine: &str) -> Self {
        let engine = match engine {
            "vllm" => GroupEngine::Vllm,
            _ => GroupEngine::Sglang,
        };
        let state_dir = directory();
        let state = Arc::new(Mutex::new(
            OwnedCoordinatorState::open(state_dir.path()).unwrap(),
        ));
        let ca = CertificateAuthority::generate(now()).unwrap();
        let ca_pem = ca.certificate_pem().to_owned();
        let key = HostKey::generate().unwrap();
        let cert = ca.issue_server("localhost", &key, now()).unwrap();
        let authority = Arc::new(EnrollmentAuthority::new(state.clone(), ca));
        let sessions = AgentSessions::new(authority.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!(
            "https://localhost:{}",
            listener.local_addr().unwrap().port()
        );
        let server = tokio::spawn(
            Server::builder()
                .tls_config(
                    ServerTlsConfig::new()
                        .identity(Identity::from_pem(cert.pem, key.private_key_pem()))
                        .client_ca_root(Certificate::from_pem(&ca_pem)),
                )
                .unwrap()
                .add_service(
                    AgentControlServer::from_arc(sessions.clone()).max_decoding_message_size(65536),
                )
                .serve_with_incoming(TcpListenerStream::new(listener)),
        );
        let head_root = directory();
        let head_marker = head_root.path().join(match engine {
            GroupEngine::Vllm => "venv/bin/released",
            _ => "runtime/released",
        });
        let mut dirs = vec![state_dir];
        let mut roots = [head_root, directory()].into_iter();
        let mut hosts = Vec::new();
        let mut controller_id = String::new();
        for (name, peer) in [("host-a", HEAD_PEER), ("host-b", WORKER_PEER)] {
            let storage_dir = directory();
            let storage = IdentityDirectory::open(storage_dir.path()).unwrap();
            let invite = authority.invite(name, 300, now()).unwrap();
            let invitation = JoinInvitation {
                version: 1,
                server_address: address.clone(),
                control_address: address.clone(),
                server_ca: ca_pem.clone(),
                invitation_id: invite.id,
                invitation_secret: invite.secret,
                host_name: invite.host_name,
                expires_unix: invite.expires_unix,
                recover_host_id: None,
            };
            let mut identity = PendingEnrollment::prepare(&storage, &invitation).unwrap();
            let request = identity.request(&invitation).unwrap();
            let certificate = Bootstrap::enroll(authority.as_ref(), tonic::Request::new(request))
                .await
                .unwrap()
                .into_inner();
            let host_id = identity
                .accept_certificate(&storage, certificate, now())
                .unwrap();
            controller_id = identity.controller_id();
            let root = roots.next().unwrap();
            hosts.push(member_host(
                engine,
                root,
                host_id,
                peer,
                identity,
                &head_marker,
            ));
            dirs.push(storage_dir);
        }
        let world = Self {
            sessions,
            state,
            controller_id,
            hosts,
            engine,
            server,
            _dirs: dirs,
        };
        for host in &world.hosts {
            eventually(&format!("{} reconciled its session", host.host_id), || {
                world
                    .sessions
                    .inspect(&host.host_id)
                    .is_some_and(|s| s.online && s.reconciled)
            })
            .await;
        }
        world
    }

    /// ADR 0028 §4: host A heads at 192.0.2.10 and host B works at
    /// 192.0.2.11, TP 2 with one rank per host; each member's local facts are
    /// its own host's.
    fn plan(&self) -> GroupPlan {
        let member = |rank: u32, host: &MemberHost, peer: &str| {
            let effective = host.effective();
            MemberPlan {
                member: MemberKey {
                    host_id: host.host_id.clone(),
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
                checkpoint_fingerprint: host.digest(),
                model_path: host.model_path(),
                devices: effective
                    .selected_devices
                    .iter()
                    .map(|device| device.id.clone())
                    .collect(),
                peer_address: peer.parse().unwrap(),
                service_port: (rank == 0).then_some(host.service_port),
                worker_port: (rank > 0 && self.engine == GroupEngine::Sglang)
                    .then_some(host.worker_port),
            }
        };
        GroupPlan::new(
            self.engine,
            vec![
                member(0, &self.hosts[0], HEAD_PEER),
                member(1, &self.hosts[1], WORKER_PEER),
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

    fn identity(&self, host: &MemberHost, rank: u32, id: &str, state: &str) -> CommandIdentity {
        CommandIdentity {
            controller_id: self.controller_id.clone(),
            member: MemberKey {
                host_id: host.host_id.clone(),
                member_id: member_id(rank),
            },
            deployment_id: DEPLOYMENT.into(),
            operation_id: OPERATION.into(),
            command_id: id.into(),
            step_id: id.into(),
            generation: 1,
            revision: 1,
            deadline_ms: capyctl_protocol::now_unix_ms() + 60_000,
            payload_digest: [0; 32],
            expected_state: state.into(),
            profile_fingerprint: host.effective().profile.build_fingerprint,
            instance_index: 0,
        }
    }

    async fn send(&self, mut command: MemberCommand) -> pb::MemberExecutionResult {
        command.identity.payload_digest = command.canonical_digest();
        tokio::time::timeout(Duration::from_secs(60), self.sessions.execute(command))
            .await
            .expect("answered in time")
            .expect("answered")
    }

    /// ADR 0028 §6, §8 (R29): each host measures its checkpoint, the head is
    /// provisioned, and each host launches its own member of `plan`, as the
    /// activation sends them. The replies, in rank order.
    async fn launch(&self, plan: &GroupPlan) -> Vec<pb::MemberExecutionResult> {
        let mut launches = Vec::new();
        for (rank, host) in (0..).zip(&self.hosts) {
            let effective = host.effective();
            let policy =
                capyctl_config::remote_resources::policy_fingerprint(&host.config.document);
            let measured = self
                .send(MemberCommand {
                    identity: self.identity(host, rank, &format!("digest-{rank}"), "checkpoint"),
                    action: MemberAction::DigestCheckpoint(
                        capyctl_protocol::execution::DigestCheckpointPlan {
                            deployment_config: host.deployment.to_string(),
                            host_policy_fingerprint: policy.clone(),
                            expected_digest: None,
                            size_only: false,
                        },
                    ),
                })
                .await;
            assert_eq!(measured.checkpoint.unwrap().state, "computed");
            let mut launch = MemberCommand {
                identity: self.identity(host, rank, &format!("launch-{rank}"), "reserved"),
                action: MemberAction::Launch {
                    plan: plan.clone(),
                    member: SingleLaunchPlan {
                        deployment_config: host.deployment.to_string(),
                        profile_name: "local".into(),
                        checkpoint_fingerprint: effective.model.content_fingerprint.clone(),
                        host_policy_fingerprint: policy,
                        binding_id: ulid::Ulid::new().to_string(),
                        incarnation: ulid::Ulid::new().to_string(),
                        grant_id: ulid::Ulid::new().to_string(),
                        service_port: if rank == 0 { host.service_port } else { 0 },
                        issued_at_ms: capyctl_protocol::now_unix_ms(),
                        coordinator_session_id: ulid::Ulid::new().to_string(),
                        checkpoint_digest: host.digest(),
                        checkpoint_weights_bytes: None,
                        checkpoint_state_slot_bytes: None,
                        checkpoint_layout: None,
                        checkpoint_tables: None,
                        startup_bytes: None,
                    },
                },
            };
            launch.identity.payload_digest = launch.canonical_digest();
            if rank == 0 {
                // ADR 0012: the head's private ingress, before its Launch.
                assert_eq!(
                    self.sessions
                        .provision_ingress(&launch, GATE)
                        .await
                        .unwrap(),
                    ProvisionOutcome::Provisioned
                );
            }
            launches.push(launch);
        }
        let mut replies = Vec::new();
        for launch in launches {
            replies.push(self.send(launch).await);
        }
        for (host, reply) in self.hosts.iter().zip(&replies) {
            assert!(reply.claim_retained, "{reply:?}");
            assert!(!reply.processes.is_empty(), "{reply:?}");
            let _ = host.leader.set(reply.processes[0].pid);
        }
        assert!(replies[0].model_usable, "{:?}", replies[0]);
        replies
    }

    /// The group's armed park or wake as the store hands it to the
    /// coordinator: each member on its own host, with its own launch, its
    /// recorded processes and its parked budget.
    fn target(&self, plan: &GroupPlan, replies: &[pb::MemberExecutionResult]) -> ResidencyTarget {
        let members = (0..)
            .zip(self.hosts.iter().zip(replies))
            .map(|(rank, (host, reply))| ArmedMember {
                rank,
                host_id: host.host_id.clone(),
                owner_id: member_owner_id(DEPLOYMENT, 0, rank),
                parked_bytes: PARKED_BUDGET,
                launch_handle: Some(reply.owned_handle.clone()),
                identities: reply
                    .processes
                    .iter()
                    .map(|p| capyctl_domain::completion::ProcessIdentity {
                        role: p.role.clone(),
                        pid: p.pid,
                        boot_id: p.boot_id.clone(),
                        start_ticks: p.start_ticks,
                    })
                    .collect(),
                residency: Residency::Deep,
            })
            .collect();
        ResidencyTarget {
            group: GroupTarget {
                deployment_id: DEPLOYMENT.into(),
                instance_index: 0,
                revision: 1,
                operation_id: OPERATION.into(),
                plan: plan.clone(),
            },
            members,
            step_id: ulid::Ulid::new().to_string(),
            deadline_ms: capyctl_protocol::now_unix_ms() + 90_000,
        }
    }

    /// The production group transport over these sessions.
    fn ctx(&self) -> GroupCtx {
        let hosts: Arc<dyn GroupHosts> = Arc::new(AgentGroupHosts::new(
            self.state.clone(),
            self.sessions.clone(),
            self.controller_id.clone(),
            Arc::default(),
        ));
        GroupCtx {
            owner: self.state.clone(),
            hosts,
            observations: self.sessions.clone(),
            clock: Arc::new(|| Ok(capyctl_protocol::now_unix_ms())),
        }
    }

    /// ADR 0028 §11: stop each member on its own host, proven gone.
    async fn stop(&self, replies: &[pb::MemberExecutionResult]) {
        for (rank, (host, reply)) in (0..).zip(self.hosts.iter().zip(replies)) {
            let gone = self
                .send(MemberCommand {
                    identity: self.identity(host, rank, &format!("stop-{rank}"), "retained"),
                    action: MemberAction::Terminate {
                        owned_handle: reply.owned_handle.clone(),
                        recorded: Vec::new(),
                    },
                })
                .await;
            assert!(
                !gone.claim_retained && gone.processes.iter().all(|p| p.presence == "gone"),
                "{gone:?}"
            );
        }
    }
}

fn golden(engine: GroupEngine) -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(format!(
            "{}/../capyctl-config/tests/fixtures/effective-{}-golden.json",
            env!("CARGO_MANIFEST_DIR"),
            if engine == GroupEngine::Sglang {
                "sglang"
            } else {
                "vllm"
            }
        ))
        .unwrap(),
    )
    .unwrap()
}

/// One member host of `engine`: a deep-park host document holding `peer`,
/// its stand-in engine, its executor (a SGLang host reads its saver map, a
/// vLLM host lists its GPU processes; both follow the head's `released`
/// marker at `head_marker`) and its live session.
fn member_host(
    engine: GroupEngine,
    root: tempfile::TempDir,
    host_id: String,
    peer: &str,
    identity: PendingEnrollment,
    head_marker: &Path,
) -> MemberHost {
    use capyctl_agent::session::SessionExecution;
    let path = root.path();
    let base_port = capyctl_testkit::ports::free_ports(3, true)[0];
    let bin = path.join("venv/bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(python3(), bin.join("python3")).unwrap();
    let runtime = private(&path.join("runtime"));
    let vllm = bin.join("vllm");
    capyctl_config::test_support::write_executable(
        &vllm,
        format!("#!{}\n{FAKE_ENGINE}", python3().display()),
        0o755,
    )
    .unwrap();
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
    for dir in [&bin, &runtime] {
        std::fs::write(dir.join("completion"), json!(COMPLETION).to_string()).unwrap();
    }
    let models = private(&path.join("models"));
    std::fs::create_dir_all(models.join("toy")).unwrap();
    std::fs::write(models.join("toy/config.json"), "{}").unwrap();

    let source = golden(engine);
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
    host["resource_policy"]["groups"] = json!({ "peer_address": peer });
    let profile = &mut host["runtime_profiles"]["local"];
    profile["security"]["deep_park"] = json!("enabled");
    profile["executable"] = match engine {
        GroupEngine::Sglang => json!(bin.join("python3")),
        _ => json!(vllm),
    };
    let mut deployment = source["input"]["deployment"].clone();
    deployment["model"]["path"] = json!(models.join("toy"));
    deployment["engine_config"] = json!({"memory": {"kv_cache": "64MiB"}});
    deployment["residency"] = json!("deep");
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

    let keys = directory();
    let journal = HostJournal::open(
        &private(&path.join("journal")),
        &identity.controller_id(),
        &host_id,
    )
    .unwrap();
    // ADR 0028 §7, §10: the substituted host holds its peer address on one
    // interface; its loopback ports are probed for real.
    let interfaces = vec![(IFNAME.to_owned(), peer.parse::<IpAddr>().unwrap())];
    let probes = HostProbes::with(
        move || interfaces.clone(),
        |address, port| {
            !address.is_loopback() || capyctl_agent::host_checks::port_free(address, port)
        },
    );
    let mut executor = NativeHostExecution::new(
        journal.clone(),
        Ingress::new().unwrap(),
        IngressIdentities::new(IdentityDirectory::open(keys.path()).unwrap()),
        config.clone(),
        host_id.clone(),
        identity.controller_id(),
        config.runtime_dir.clone(),
        private(&path.join("logs")),
        // ADR 0007: the role's startup inventory names the host's one memory
        // domain, which every availability refresh reports.
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
    .with_engine_cache_root(private(&path.join("engines")));
    let leader = Arc::new(OnceLock::<u32>::new());
    match engine {
        // ADR 0028 §12 (R12): the saver map, from this host's own
        // observation directory.
        GroupEngine::Sglang => {
            executor = executor.with_saver_residency(Arc::new(StubSaver {
                dir: private(&state.join("observation")),
                head_marker: head_marker.to_owned(),
            }));
        }
        // ADR 0007: `nvidia-smi` lists every process of the member's engine
        // holding GPU memory, which it gives back while the group sleeps.
        _ => {
            let (seen, marker) = (leader.clone(), head_marker.to_owned());
            let gpu: Arc<GpuCollector> = Arc::new(move || {
                let leader = *seen.get()?;
                let bytes = if marker.exists() { 0 } else { GPU_RESIDENT };
                Some(
                    group_pids(leader)
                        .into_iter()
                        .map(|pid| (pid, bytes))
                        .collect(),
                )
            });
            executor = executor.with_process_residency(ResidencySampler::with_collector(gpu));
        }
    }
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let startup = executor.inventory().expect("the host measures its memory");
    let (session_journal, session_executor) = (journal.clone(), executor.clone());
    tokio::spawn(async move {
        let _ = capyctl_agent::session::run_session_with_execution(
            &identity,
            session_journal,
            startup,
            shutdown,
            Some(session_executor),
        )
        .await;
    });
    MemberHost {
        host_id,
        root,
        config,
        deployment,
        service_port: base_port,
        worker_port: base_port + 1,
        journal,
        leader,
        stop,
        _keys: keys,
    }
}

// T20, T22 (ADR 0028 §12, R12, R41): a deep vLLM group and a deep SGLang group
// park and wake end to end over the production path. The controller sends the
// head's agent alone one Park and one Restore, which that agent admits under
// its own policy and journal and carries out as one collective; each member
// settles only on its own host's report on its own session (vLLM: its
// processes in the host's `process_residency`; SGLang: its saver map from the
// host's observation directory); the wake passes the head's readiness probe and
// its canary. The worker's agent never receives a residency change.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deep_groups_park_and_wake_through_the_head_on_per_member_evidence() {
    for engine in ["vllm", "sglang"] {
        let world = World::new(engine).await;
        let plan = world.plan();
        let replies = world.launch(&plan).await;
        let ctx = world.ctx();
        let (head, worker) = (&world.hosts[0], &world.hosts[1]);
        let handles: Vec<&str> = replies.iter().map(|r| r.owned_handle.as_str()).collect();

        let park = world.target(&plan, &replies);
        let parked = park_group(&ctx, &park)
            .await
            .unwrap_or_else(|e| panic!("{engine}: the park failed: {e}"));
        assert_eq!(parked.len(), 2, "{engine}");
        for (report, member) in parked.iter().zip(&park.members) {
            assert!(report.released(member), "{engine}: {report:?} {member:?}");
        }
        let collective = match engine {
            "vllm" => vec!["sleep:2"],
            _ => vec!["/release_memory_occupation"],
        };
        assert_eq!(head.controls(), collective, "{engine}");
        assert!(worker.controls().is_empty(), "{engine}");
        assert_eq!(
            head.journal.residency_of(handles[0]).unwrap().as_deref(),
            Some("parked"),
            "{engine}"
        );
        assert_eq!(worker.journal.residency_of(handles[1]).unwrap(), None);
        if engine == "sglang" {
            // R12: each member's saver map, from its own host's report.
            assert!(parked.iter().all(|r| r.resident_bytes == 0), "{parked:?}");
        }

        let wake = ResidencyTarget {
            step_id: ulid::Ulid::new().to_string(),
            deadline_ms: capyctl_protocol::now_unix_ms() + 90_000,
            ..park.clone()
        };
        let woken = wake_group(&ctx, &wake)
            .await
            .unwrap_or_else(|e| panic!("{engine}: the wake failed: {e}"));
        for (report, member) in woken.iter().zip(&wake.members) {
            assert!(report.resident(member), "{engine}: {report:?} {member:?}");
        }
        match engine {
            "vllm" => assert!(woken.iter().all(|r| r.resident_bytes >= GPU_RESIDENT)),
            _ => assert!(woken
                .iter()
                .all(|r| r.resident_bytes == 2 * SAVER_TAG as i64)),
        }
        // One collective each way, on the head alone.
        assert_eq!(
            head.controls(),
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
        assert!(worker.controls().is_empty(), "{engine}");
        assert_eq!(head.journal.residency_of(handles[0]).unwrap(), None);
        world.stop(&replies).await;
    }
}
