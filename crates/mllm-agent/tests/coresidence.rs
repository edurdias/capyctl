//! Per-launch host claims (SPEC §§3.1, 7.3, 13; owner goal D10): two
//! deployments co-resident on one enrolled host, a vLLM stand-in and an SGLang
//! stand-in, each launched through the host agent's own journal, admission,
//! ingress and cleanup.
//!
//! The engines are small Python stand-ins with each engine's observable launch
//! shape: vLLM through mllm's protected entry with the key in `VLLM_API_KEY`,
//! SGLang as the protected `sglang_entry.py` reading its settings from argv and
//! its keys from sealed descriptors. CPU and fake-engine tests are not
//! qualification: nothing here shows either engine runs co-resident on a
//! Spark (SPEC §18).

use mllm_agent::{
    identity_storage::IdentityDirectory,
    ingress::Ingress,
    ingress_identity::IngressIdentities,
    journal::{ClaimPhase, HostJournal},
    load::LoadReporter,
    native_execution::NativeHostExecution,
    session::{Provisioned, SessionExecution},
};
use mllm_config::remote_roles::HostConfig;
use mllm_domain::group::{CommandIdentity, MemberKey};
use mllm_protocol::{
    execution::{MemberAction, MemberCommand, SingleLaunchPlan},
    pb,
};
use serde_json::{json, Value};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
};

/// One stand-in for both engines. As vLLM it is run by mllm's protected entry
/// (`serve <model> --port N --served-model-name R`, key from `VLLM_API_KEY`);
/// as SGLang it *is* the protected `sglang_entry.py`, run with `-IS`, taking the
/// endpoint and served name from `--public-settings-json` and the inference key
/// from the sealed descriptor `--inference-credential-fd` names. Either way it
/// forks one worker child, keys every route, answers chat (streamed or not)
/// naming its engine, and serves the engine's pinned load gauges.
const FAKE_ENGINE: &str = r#"
import json, os, signal, sys, time, http.server
args = sys.argv[1:]
if "--public-settings-json" in args:
    engine = "sglang"
    settings = json.loads(args[args.index("--public-settings-json") + 1])
    port = int(settings["endpoint"].rsplit(":", 1)[1])
    served = settings["served_name"]
    fd = int(args[args.index("--inference-credential-fd") + 1])
    key = os.pread(fd, 4096, 0).decode().strip()
    metrics = "sglang:num_running_reqs 0\nsglang:num_queue_reqs 0\nsglang:token_usage 0.25\n"
    # As loopback_rendezvous.pin: the host-named file rendezvous, 0700.
    rendezvous = os.environ.get("MLLM_RENDEZVOUS_DIR")
    if rendezvous:
        os.mkdir(rendezvous, 0o700)
        with open(os.path.join(rendezvous, "store"), "w") as f:
            f.write("store")
else:
    engine = "vllm"
    key = os.environ.get("VLLM_API_KEY", "")
    port = int(args[args.index("--port") + 1])
    served = args[args.index("--served-model-name") + 1]
    metrics = "vllm:num_requests_running 0\nvllm:num_requests_waiting 0\nvllm:kv_cache_usage_perc 0.5\n"
signal.signal(signal.SIGCHLD, signal.SIG_IGN)
if os.fork() == 0:
    while True:
        time.sleep(60)
answer = "fake-%s-answer for %s" % (engine, served)

class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"
    def log_message(self, *a):
        pass
    def keyed(self):
        if not key or self.headers.get("Authorization") != "Bearer " + key:
            self.send_response(401); self.send_header("Content-Length", "0"); self.end_headers(); return False
        return True
    def send_body(self, body, content_type):
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers(); self.wfile.write(body)
    def do_GET(self):
        if not self.keyed(): return
        if self.path == "/v1/models":
            self.send_body(json.dumps({"object": "list", "data": [{"id": served, "object": "model"}]}).encode(), "application/json")
        elif self.path == "/metrics":
            self.send_body(metrics.encode(), "text/plain")
        elif self.path == "/is_sleeping":
            self.send_body(b'{"is_sleeping": false}', "application/json")
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
            for part in (chunk({"role": "assistant", "content": ""}, None), chunk({"content": answer}, None), chunk({}, "stop")):
                self.wfile.write(("data: " + json.dumps(part) + "\n\n").encode())
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
            return
        self.send_body(json.dumps({"id": "c1", "object": "chat.completion", "created": 1, "model": served,
                                   "choices": [{"index": 0, "finish_reason": "stop",
                                                "message": {"role": "assistant", "content": answer}}]}).encode(),
                       "application/json")

http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
"#;

fn directory() -> tempfile::TempDir {
    let d = tempfile::Builder::new()
        .prefix("mllm-coresidence-")
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
    mllm_protocol::now_unix_ms()
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

/// One enrolled host with a vLLM and an SGLang profile on one shared device of
/// one unified memory domain, and the deployments that run there.
struct Fixture {
    root: tempfile::TempDir,
    base_port: u16,
    config: HostConfig,
    vllm: Value,
    sglang: Value,
}

impl Fixture {
    /// `managed` is the unified domain's managed budget.
    fn new(managed: &str) -> Self {
        let root = directory();
        let path = root.path();
        let base_port = free_ports(3);
        let bin = path.join("venv/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let engine = bin.join("vllm");
        std::fs::write(&engine, format!("#!{}\n{FAKE_ENGINE}", python3().display())).unwrap();
        std::fs::set_permissions(&engine, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(python3(), bin.join("python3")).unwrap();
        // SPEC §9.1, §13.3: the runtime directory holds mllm's protected
        // entries, owned by the agent user and writable by it alone.
        let runtime = private(&path.join("runtime"));
        module(
            &runtime.join("vllm_entry.py"),
            &format!(
                "import runpy\nrunpy.run_path({:?}, run_name=\"__main__\")\n",
                engine.display().to_string()
            ),
        );
        module(&runtime.join("mllm_vllm_guard.py"), "# stand-in guard\n");
        module(&runtime.join("sglang_entry.py"), FAKE_ENGINE);
        module(&runtime.join("pinned_file_observation.py"), "# stand-in\n");
        // ADR 0008: every SGLang launch imports the capability probes.
        module(&runtime.join("engine_capabilities.py"), "# stand-in probes\n");
        let models = private(&path.join("models"));
        for name in ["tv", "ts", "big"] {
            std::fs::create_dir_all(models.join(name)).unwrap();
        }

        let golden = |engine: &str| -> Value {
            serde_json::from_str(
                &std::fs::read_to_string(format!(
                    "{}/../mllm-config/tests/fixtures/effective-{engine}-golden.json",
                    env!("CARGO_MANIFEST_DIR")
                ))
                .unwrap(),
            )
            .unwrap()
        };
        let (vllm_golden, sglang_golden) = (golden("vllm"), golden("sglang"));
        let mut host = vllm_golden["input"]["host"].clone();
        let state = path.join("state");
        host["state_dir"] = json!(state);
        host["identity_dir"] = json!(state.join("identity"));
        host["runtime_dir"] = json!(runtime);
        host["model_store"]["path"] = json!(models);
        host["resource_policy"]["endpoint_port_range"] =
            json!({"start": base_port, "end": base_port + 2});
        // D10: one GB10-shaped pool, shared device, a budget any CI host holds.
        host["resource_policy"]["domains"]["unified"] = json!({
            "free_reserve": "16MiB", "host_kv_limit": "64MiB", "managed_limit": managed,
            "memory": "unified", "parked_limit": "256MiB"
        });
        let vllm_profile = &mut host["runtime_profiles"]["local"];
        vllm_profile["executable"] = json!(engine);
        vllm_profile["security"]["deep_park"] = json!("disabled");
        let mut sglang_profile =
            sglang_golden["input"]["host"]["runtime_profiles"]["local"].clone();
        sglang_profile["executable"] = json!(bin.join("python3"));
        sglang_profile["build_fingerprint"] = json!("sglang-build-1");
        sglang_profile["security"]["deep_park"] = json!("disabled");
        host["runtime_profiles"]["sglang"] = sglang_profile;

        let deployment = |source: &Value, name: &str, profile: &str| {
            let mut d = source["input"]["deployment"].clone();
            d["name"] = json!(name);
            d["routes"] = json!([name]);
            d["runtime_profile"] = json!(profile);
            d["model"]["path"] = json!(models.join(name));
            d["engine_config"]["memory"]["kv_cache"] = json!("64MiB");
            d["residency"] = json!("restart_only");
            for (phase, bytes) in [
                ("cold", "256MiB"),
                ("ready", "128MiB"),
                ("parking", "128MiB"),
                ("parked", "32MiB"),
                ("wake", "256MiB"),
            ] {
                d["resources"][phase]["allocations"][0]["bytes"] = json!(bytes);
                d["resources"][phase]["allocations"][0]["host_kv_bytes"] = json!("0B");
            }
            d
        };
        let vllm = deployment(&vllm_golden, "tv", "local");
        let sglang = deployment(&sglang_golden, "ts", "sglang");
        let config = HostConfig::parse(&host.to_string()).unwrap();
        Self {
            root,
            base_port,
            config,
            vllm,
            sglang,
        }
    }

    /// The reserved launch of `deployment` as the controller would send it:
    /// its own deployment id, generation, binding, incarnation and leased port.
    fn launch(
        &self,
        id: &str,
        deployment_id: &str,
        deployment: &Value,
        slot: u16,
    ) -> MemberCommand {
        let local =
            mllm_config::remote_resources::local_host_document(&self.config.document).unwrap();
        let effective = mllm_config::effective::resolve_effective(deployment, &local).unwrap();
        let location = mllm_config::effective::checkpoint_location(deployment, &local).unwrap();
        let checkpoint_digest = mllm_agent::checkpoint::CheckpointVerifier::in_memory()
            .measure(&location.model_store, &location.checkpoint)
            .unwrap()
            .manifest
            .digest;
        sign(MemberCommand {
            identity: identity(
                id,
                deployment_id,
                "reserved",
                &effective.profile.build_fingerprint,
            ),
            action: MemberAction::LaunchSingle(SingleLaunchPlan {
                deployment_config: deployment.to_string(),
                profile_name: deployment["runtime_profile"].as_str().unwrap().into(),
                checkpoint_fingerprint: effective.model.content_fingerprint.clone(),
                host_policy_fingerprint: mllm_config::remote_resources::policy_fingerprint(
                    &self.config.document,
                ),
                binding_id: format!("01K0000000000000000000000{slot}"),
                incarnation: format!("01K0000000000000000000001{slot}"),
                grant_id: format!("01K0000000000000000000002{slot}"),
                issued_at_ms: now_ms(),
                coordinator_session_id: "01K00000000000000000000030".into(),
                service_port: self.base_port + slot,
                checkpoint_digest,
                checkpoint_weights_bytes: None,
                startup_bytes: None,
            }),
        })
    }
}

fn identity(
    id: &str,
    deployment: &str,
    expected_state: &str,
    fingerprint: &str,
) -> CommandIdentity {
    CommandIdentity {
        controller_id: "controller".into(),
        member: MemberKey {
            host_id: "host".into(),
            member_id: "head".into(),
        },
        deployment_id: deployment.into(),
        operation_id: format!("operation-{id}"),
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

/// A command naming the launch `launch` owns (Terminate or Probe).
fn about(
    launch: &MemberCommand,
    id: &str,
    expected_state: &str,
    action: MemberAction,
) -> MemberCommand {
    sign(MemberCommand {
        identity: identity(
            id,
            &launch.identity.deployment_id,
            expected_state,
            &launch.identity.profile_fingerprint,
        ),
        action,
    })
}

fn stop(launch: &MemberCommand, id: &str) -> MemberCommand {
    about(
        launch,
        id,
        "retained",
        MemberAction::Terminate {
            owned_handle: launch.identity.command_id.clone(),
            recorded: Vec::new(),
        },
    )
}

fn probe(launch: &MemberCommand, id: &str) -> MemberCommand {
    about(
        launch,
        id,
        "ready",
        MemberAction::Probe {
            owned_handle: launch.identity.command_id.clone(),
        },
    )
}

struct Host {
    journal: Arc<HostJournal>,
    identities: Arc<IngressIdentities>,
    ingress: Arc<Ingress>,
    executor: Arc<NativeHostExecution>,
    session: u64,
}

fn executor(
    fixture: &Fixture,
    journal: Arc<HostJournal>,
    identities: Arc<IngressIdentities>,
    ingress: Arc<Ingress>,
) -> Arc<NativeHostExecution> {
    let path = fixture.root.path();
    NativeHostExecution::new(
        journal,
        ingress,
        identities,
        fixture.config.clone(),
        "host".into(),
        "controller".into(),
        fixture.config.runtime_dir.clone(),
        private(&path.join("logs")),
        Default::default(),
    )
    .with_rendezvous_root(private(&path.join("state/rendezvous")))
}

fn host(fixture: &Fixture) -> Host {
    let journal = HostJournal::open(
        &private(&fixture.root.path().join("journal")),
        "controller",
        "host",
    )
    .unwrap();
    let ingress = Ingress::new().unwrap();
    let identities = IngressIdentities::new(
        IdentityDirectory::open(&private(&fixture.root.path().join("ingress-identity"))).unwrap(),
    );
    let executor = executor(
        fixture,
        journal.clone(),
        identities.clone(),
        ingress.clone(),
    );
    let session = journal.connect().unwrap();
    executor.connected(session).unwrap();
    Host {
        journal,
        identities,
        ingress,
        executor,
        session,
    }
}

/// A host agent restart: the journal survives; ingress gates, readiness
/// authority and the executor do not.
fn restarted(fixture: &Fixture, before: Host) -> Host {
    before.executor.disconnected(before.session);
    let _ = before.journal.disconnect(before.session);
    let ingress = Ingress::new().unwrap();
    let executor = executor(
        fixture,
        before.journal.clone(),
        before.identities.clone(),
        ingress.clone(),
    );
    let session = before.journal.connect().unwrap();
    executor.connected(session).unwrap();
    Host {
        journal: before.journal,
        identities: before.identities,
        ingress,
        executor,
        session,
    }
}

/// Kills every process the journal recorded, so a failed assertion cannot
/// leave a stand-in engine behind.
struct Reap(Arc<HostJournal>);
impl Drop for Reap {
    fn drop(&mut self) {
        use mllm_adapters::traits::OwnedProcessLaunch;
        struct NoSpawn;
        impl mllm_launchers::LaunchAssociation for NoSpawn {
            fn persist_api_identity(
                &self,
                _: &mllm_domain::completion::ProcessIdentity,
            ) -> Result<(), mllm_launchers::AssociationError> {
                panic!("cleanup cannot spawn")
            }
        }
        for handle in ["vllm", "sglang"] {
            if let Ok(owned) = self.0.inspect_owned(handle) {
                let identities: Vec<_> = owned.into_iter().map(|(p, _)| p).collect();
                let tools = mllm_launchers::DurableProcessLaunch::new(Arc::new(NoSpawn));
                let _ = tools.terminate_owned(&identities, std::time::Duration::from_millis(200));
            }
        }
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

async fn chat(address: std::net::SocketAddr, gate: [u8; 32], model: &str) -> (u16, String) {
    let response = reqwest::Client::new()
        .post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(hex::encode(gate))
        .json(&json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.text().await.unwrap_or_default())
}

const GATE_V: [u8; 32] = [7; 32];
const GATE_S: [u8; 32] = [8; 32];
const GATE_BIG: [u8; 32] = [9; 32];

async fn ready(host: &Host, launch: &MemberCommand, gate: [u8; 32]) -> pb::MemberExecutionResult {
    assert_eq!(
        host.executor.provision(launch.clone(), gate).await.unwrap(),
        Provisioned::Stored
    );
    let result = host
        .executor
        .execute(host.session, launch.clone())
        .await
        .unwrap();
    assert!(result.model_usable && result.claim_retained, "{result:?}");
    assert!(result.processes.len() >= 2 && result.processes.iter().all(|p| p.presence == "alive"));
    result
}

/// D10, tier-2 mixed-engine co-residency: a vLLM and an SGLang deployment are
/// ready at once on one enrolled host, each with its own journal claim, gate
/// and load sample; a launch that would exceed the host's managed budget is
/// refused by the host itself with `insufficient_memory` before anything is
/// journaled; a host restart adopts both retained launches and serves them
/// again only after each one's fresh probe; and each stops on its own, with
/// cleanup proving only its own processes gone.
// T24 T26 T27 T33 T34 T23 T10 T37
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_vllm_and_an_sglang_deployment_co_reside_on_one_host() {
    // Two ready launches hold 2 x 128 MiB; a third's 900 MiB cold footprint
    // fits the 1 GiB budget alone but not beside them.
    let fixture = Fixture::new("1GiB");
    let host = host(&fixture);
    let _reap = Reap(host.journal.clone());
    let vllm = fixture.launch("vllm", "deployment-v", &fixture.vllm, 0);
    let sglang = fixture.launch("sglang", "deployment-s", &fixture.sglang, 1);

    // SPEC §§3.1, 7.3: the second launch is admitted beside the first, which
    // a single-claim host refused (its journal allowed one claim).
    ready(&host, &vllm, GATE_V).await;
    let result = ready(&host, &sglang, GATE_S).await;
    assert_eq!(result.owned_handle, "sglang");
    let claimed = host.journal.claimed_launches("").unwrap();
    assert_eq!(claimed.len(), 2);
    assert!(
        claimed.iter().all(|c| c.phase == ClaimPhase::Ready),
        "{claimed:?}"
    );

    // SPEC §10: both gates forward at once, each to its own engine.
    let (address, server) = serve(host.ingress.clone()).await;
    let (status, body) = chat(address, GATE_V, "tv").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("fake-vllm-answer for tv"), "{body}");
    let (status, body) = chat(address, GATE_S, "ts").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("fake-sglang-answer for ts"), "{body}");
    // A gate key opens only its own entry.
    assert_ne!(chat(address, GATE_V, "ts").await.0, 200);

    // W8 / D9: one load sample per co-resident launch, each scraped from its
    // own engine with its own key.
    let reporter = LoadReporter::new(host.ingress.clone(), "host".into()).unwrap();
    let mut samples: Vec<pb::LoadSample> = reporter
        .reports()
        .await
        .into_iter()
        .flat_map(|r| r.samples)
        .collect();
    samples.sort_by(|a, b| a.deployment_id.cmp(&b.deployment_id));
    let seen: Vec<(&str, &str, bool, u32)> = samples
        .iter()
        .map(|s| {
            (
                s.deployment_id.as_str(),
                s.owned_handle.as_str(),
                s.scrape_ok,
                s.kv_usage_ppm,
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            ("deployment-s", "sglang", true, 250_000),
            ("deployment-v", "vllm", true, 500_000)
        ]
    );

    // D10: over budget, the host refuses with a typed reason before any
    // effect: no key stored, nothing journaled, no process.
    let mut big = fixture.vllm.clone();
    big["name"] = json!("big");
    big["routes"] = json!(["big"]);
    big["model"]["path"] = json!(fixture.root.path().join("models/big"));
    big["resources"]["cold"]["allocations"][0]["bytes"] = json!("900MiB");
    big["resources"]["wake"]["allocations"][0]["bytes"] = json!("900MiB");
    let big = fixture.launch("big", "deployment-big", &big, 2);
    assert_eq!(
        host.executor
            .provision(big.clone(), GATE_BIG)
            .await
            .unwrap(),
        Provisioned::Refused("insufficient_memory")
    );
    let refused = host
        .executor
        .execute(host.session, big.clone())
        .await
        .unwrap();
    mllm_protocol::execution::validate_result(&big, &refused).unwrap();
    assert_eq!(refused.refused, "insufficient_memory");
    assert!(!refused.claim_retained && refused.processes.is_empty() && !refused.model_usable);
    assert!(host
        .journal
        .history(0, 100)
        .unwrap()
        .iter()
        .all(|r| r.command_id != "big"));

    // SPEC §13.2, T33: a host restart adopts both retained launches. Neither
    // serves until its own fresh probe passes, and one probe opens only its
    // own gate.
    server.abort();
    let host = restarted(&fixture, host);
    let (address, server) = serve(host.ingress.clone()).await;
    assert_eq!(host.journal.claimed_launches("").unwrap().len(), 2);
    assert_ne!(chat(address, GATE_V, "tv").await.0, 200);
    let probe_vllm = probe(&vllm, "probe-vllm");
    let proved = host
        .executor
        .execute(host.session, probe_vllm.clone())
        .await
        .unwrap();
    assert!(proved.model_usable, "{proved:?}");
    assert_eq!(chat(address, GATE_V, "tv").await.0, 200);
    assert_ne!(chat(address, GATE_S, "ts").await.0, 200, "not probed yet");
    let proved = host
        .executor
        .execute(host.session, probe(&sglang, "probe-sglang"))
        .await
        .unwrap();
    assert!(proved.model_usable, "{proved:?}");
    assert_eq!(chat(address, GATE_S, "ts").await.0, 200);
    // Each launch's readiness stays its own: a replay of the first probe is
    // still usable after the second one published.
    let replay = host
        .executor
        .execute(host.session, probe_vllm)
        .await
        .unwrap();
    assert!(replay.model_usable, "{replay:?}");

    // SPEC §13, T10: stopping the vLLM deployment proves only its processes
    // gone and releases only its claim; SGLang keeps serving.
    let sglang_before = host.journal.inspect_owned("sglang").unwrap();
    let stopped = host
        .executor
        .execute(host.session, stop(&vllm, "stop-vllm"))
        .await
        .unwrap();
    assert!(
        stopped.state == "completed" && !stopped.claim_retained,
        "{stopped:?}"
    );
    assert!(stopped.processes.iter().all(|p| p.presence == "gone"));
    assert_ne!(chat(address, GATE_V, "tv").await.0, 200);
    let (status, body) = chat(address, GATE_S, "ts").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(host.journal.inspect_owned("sglang").unwrap(), sglang_before);
    let claimed = host.journal.claimed_launches("").unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].command.identity.command_id, "sglang");

    // Room frees only as claims are released: beside SGLang the large launch
    // still does not fit; with nothing claimed it is admitted.
    assert_eq!(
        host.executor
            .provision(big.clone(), GATE_BIG)
            .await
            .unwrap(),
        Provisioned::Refused("insufficient_memory")
    );
    let stopped = host
        .executor
        .execute(host.session, stop(&sglang, "stop-sglang"))
        .await
        .unwrap();
    assert!(!stopped.claim_retained && stopped.processes.iter().all(|p| p.presence == "gone"));
    assert!(host.journal.claimed_launches("").unwrap().is_empty());
    assert_eq!(
        host.executor.provision(big, GATE_BIG).await.unwrap(),
        Provisioned::Stored
    );
    server.abort();
}

/// SPEC §§3.1, 7.3: this executor advertises per-launch claims in the
/// inventory it publishes, so the controller places co-resident launches on
/// it; the capability is a fact about the agent build, not a controller claim.
/// ADR 0013 §5: the journal (v5) also fences per instance, so it advertises
/// `per_instance`, which implies per-launch claims.
// T24 T33
#[test]
fn the_executor_advertises_per_launch_claims() {
    let fixture = Fixture::new("1GiB");
    let journal = HostJournal::open(
        &private(&fixture.root.path().join("journal")),
        "controller",
        "host",
    )
    .unwrap();
    let path = fixture.root.path();
    let execution = NativeHostExecution::new(
        journal,
        Ingress::new().unwrap(),
        IngressIdentities::new(
            IdentityDirectory::open(&private(&path.join("ingress-identity"))).unwrap(),
        ),
        fixture.config.clone(),
        "host".into(),
        "controller".into(),
        fixture.config.runtime_dir.clone(),
        private(&path.join("logs")),
        pb::ReportInventory {
            domains: vec![pb::DomainObservation {
                domain_id: "unified".into(),
                ..Default::default()
            }],
            ..Default::default()
        },
    );
    let inventory = execution.inventory().expect("one measured domain");
    assert_eq!(
        inventory.launch_claims,
        mllm_agent::journal::PER_INSTANCE_CLAIMS
    );
}

/// SPEC §8.2 / T21 (found live 2026-09-23): a signalled SGLang stop never runs
/// the entry's exit handlers, so its file rendezvous outlived the launch in
/// `/tmp`. The host names the directory inside its private root and removes it
/// once Terminate proves the group gone.
// T21 T10
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stopped_sglang_launch_leaves_no_rendezvous_directory() {
    let fixture = Fixture::new("1GiB");
    let host = host(&fixture);
    let _reap = Reap(host.journal.clone());
    let sglang = fixture.launch("sglang", "deployment-s", &fixture.sglang, 1);
    let MemberAction::LaunchSingle(plan) = &sglang.action else {
        unreachable!()
    };
    let dir = fixture
        .root
        .path()
        .join("state/rendezvous")
        .join(&plan.incarnation);

    ready(&host, &sglang, GATE_S).await;
    assert!(dir.join("store").is_file(), "the entry was handed its directory");

    let stopped = host
        .executor
        .execute(host.session, stop(&sglang, "stop-sglang"))
        .await
        .unwrap();
    assert!(!stopped.claim_retained && stopped.processes.iter().all(|p| p.presence == "gone"));
    assert!(!dir.exists(), "the gone launch's rendezvous directory remains");
    assert!(fixture.root.path().join("state/rendezvous").is_dir());
}
