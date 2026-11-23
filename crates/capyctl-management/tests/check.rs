//! U7 / R21, R22, AE5: previews use real strict parsing, hub pinning,
//! checkpoint sizing and store placement. Every request proves the entire
//! ledger unchanged, including events, receipts and reservations.
use axum::{
    body::{to_bytes, Body},
    http::Request,
    Extension, Router,
};
use capyctl_agent::host_admin::CachedSourcePinner;
use capyctl_config::{
    deployment_pin::{Hub, CLI_TOKEN_HINT},
    model_source::ModelSourcePolicy,
};
use capyctl_controller::{
    coordinator::{CoordinatorOptions, ObservationFuture, OwnedCoordinator, ServiceObservation},
    OwnedCoordinatorState,
};
use capyctl_management::{
    check::{self, StandaloneSources},
    configuration::SharedConfigurationSource,
    ui_sessions::Principal,
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    os::unix::fs::PermissionsExt,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};
use tower::ServiceExt;

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

fn directory() -> tempfile::TempDir {
    // As in compose.rs: ownership intentionally refuses every directory
    // beneath a world-writable ancestor, including /var/tmp.
    let dir = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

struct Readings(Mutex<Vec<capyctl_domain::resources::MemoryObservation>>);

impl ServiceObservation for Readings {
    fn observe(&self, _: String) -> ObservationFuture {
        let readings = self.0.lock().unwrap().clone();
        Box::pin(async move { Ok(readings) })
    }
}

struct Preview {
    dir: tempfile::TempDir,
    owner: Arc<Mutex<OwnedCoordinatorState>>,
    host: Value,
    app: Router,
    readings: Arc<Readings>,
    worker: OwnedCoordinator,
}

impl Preview {
    fn new(observed: bool, edit: impl FnOnce(&mut Value)) -> Self {
        let dir = directory();
        let owner = Arc::new(Mutex::new(OwnedCoordinatorState::open(dir.path()).unwrap()));
        let golden: Value = serde_json::from_str(include_str!(
            "../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
        ))
        .unwrap();
        let mut host = golden["input"]["host"].clone();
        host["model_store"]["path"] = json!(dir.path());
        edit(&mut host);
        let observations = if observed {
            let effective =
                capyctl_config::effective::resolve_effective(&golden["input"]["deployment"], &host)
                    .unwrap();
            let observations: Vec<_> = effective
                .host
                .domains
                .keys()
                .map(|domain| capyctl_domain::resources::MemoryObservation {
                    domain: domain.clone(),
                    capacity_bytes: 128 << 30,
                    available_bytes: 120 << 30,
                    sampled_at_ms: 1000,
                    outside_processes: None,
                })
                .collect();
            let owned = owner.lock().unwrap();
            owned
                .store()
                .import_resource_policy(owned.session(), &effective.host, &observations, 1000)
                .unwrap();
            observations
        } else {
            Vec::new()
        };
        let readings = Arc::new(Readings(Mutex::new(observations)));
        let worker = capyctl_testkit::spawn_fake_coordinator(
            owner.clone(),
            readings.clone(),
            Arc::new(|| Ok(1000)),
            CoordinatorOptions::default(),
        )
        .unwrap();
        // As with SourceStore's loopback transport in other integration
        // tests, serve the host's HTTPS mirror over loopback HTTP only.
        let policy = ModelSourcePolicy {
            huggingface_endpoint: host["model_sources"]["huggingface_endpoint"]
                .as_str()
                .map(|endpoint| endpoint.replacen("https://127.0.0.1:", "http://127.0.0.1:", 1)),
            huggingface_token_file: host["model_sources"]["huggingface_token_file"]
                .as_str()
                .map(Into::into),
            ..Default::default()
        };
        let pinner = Arc::new(CachedSourcePinner::new(
            policy,
            dir.path().to_path_buf(),
            Arc::new(|_| None),
        ));
        let configuration =
            Arc::new(SharedConfigurationSource::new(owner.clone(), host.clone(), "owner").unwrap());
        let app = check::router(
            configuration,
            Arc::new(StandaloneSources(pinner)),
            Some(worker.commands()),
        )
        .layer(Extension(Principal::Admin));
        Self {
            dir,
            owner,
            host,
            app,
            readings,
            worker,
        }
    }

    fn dump(&self) -> BTreeMap<String, Vec<Vec<String>>> {
        let db = rusqlite::Connection::open(self.dir.path().join("srv.sqlite3")).unwrap();
        let tables: Vec<String> = db
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        tables
            .into_iter()
            .map(|table| {
                let mut statement = db
                    .prepare(&format!("SELECT * FROM \"{}\"", table.replace('"', "\"\"")))
                    .unwrap();
                let count = statement.column_count();
                let rows = statement
                    .query_map([], |row| {
                        (0..count)
                            .map(|i| row.get_ref(i).map(|v| format!("{v:?}")))
                            .collect::<Result<Vec<_>, _>>()
                    })
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap();
                (table, rows)
            })
            .collect()
    }

    async fn request(&self, body: Value) -> (u16, Value) {
        let before = self.dump();
        let response = self
            .app
            .clone()
            .oneshot(
                Request::post("/management/v1/deployments/check")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status().as_u16();
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(self.dump(), before, "U7: the whole store is unchanged");
        (status, value)
    }

    fn measure(&self, source: Value, weights: i64) -> String {
        let config = json!({"name":"measured","engine":"vllm","model":source});
        let owned = self.owner.lock().unwrap();
        let receipt = owned
            .store()
            .create_stopped_managed_configuration(
                owned.session(),
                "owner",
                "measurement",
                &json!({"config":config}).to_string(),
                &self.host,
                1000,
            )
            .unwrap();
        let outcome = owned
            .store()
            .record_checkpoint_digest(
                owned.session(),
                &receipt.deployment_id,
                receipt.revision,
                "lab",
                &format!("sha256:{}", "a".repeat(64)),
                weights,
                1001,
            )
            .unwrap();
        assert!(
            matches!(
                outcome,
                capyctl_store::checkpoint_digests::RecordOutcome::Recorded { .. }
            ),
            "{outcome:?}"
        );
        receipt.deployment_id
    }
}

struct Mirror {
    origin: String,
    queries: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}
impl Mirror {
    fn endpoint(&self) -> String {
        self.origin.replacen("http:", "https:", 1)
    }
}
impl Drop for Mirror {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn mirror(weights: Option<u64>, token: Option<&str>) -> Mirror {
    let queries = Arc::new(AtomicUsize::new(0));
    let count = queries.clone();
    let token = token.map(str::to_owned);
    let app = Router::new().route("/api/models/org/model/revision/{reference}", axum::routing::get(move |request: Request<Body>| {
        let count = count.clone(); let token = token.clone();
        async move {
            count.fetch_add(1, Ordering::SeqCst);
            if let Some(token) = token { assert!(request.headers().get("authorization").is_some_and(|h| h == format!("Bearer {token}").as_str())); }
            let siblings = weights.map(|size| json!([{"rfilename":"model.safetensors","size":size},{"rfilename":"config.json"}])).unwrap_or_else(|| json!([]));
            axum::Json(json!({"sha":SHA,"siblings":siblings}))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Mirror {
        origin,
        queries,
        task,
    }
}
fn hf() -> Value {
    json!({"name":"preview","engine":"vllm","model":{"hf":"org/model"}})
}

// U7 R21 R22: recorded measurement wins over the hub's estimate.
#[tokio::test]
async fn measured_weights_fit_with_a_pinned_config_and_no_writes() {
    let hub = mirror(Some(100 << 30), None).await;
    let p = Preview::new(true, |host| {
        host["model_sources"]["huggingface_endpoint"] = json!(hub.endpoint())
    });
    p.measure(json!({"hf":format!("org/model@{SHA}")}), 1 << 30);
    let (status, value) = p
        .request(json!({"yaml":"name: preview\nengine: vllm\nmodel: {hf: org/model}\n"}))
        .await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["verdict"], "fits", "{value}");
    assert_eq!(value["estimated"], false);
    assert_eq!(value["config"]["model"]["source"]["revision"], SHA);
    assert_eq!(
        capyctl_config::parse_strict(
            capyctl_config::ConfigKind::Deployment,
            value["yaml"].as_str().unwrap()
        )
        .unwrap(),
        value["config"]
    );
}

// U7 AE5: capacity uses the ordinary start's placement refusal and limit.
#[tokio::test]
async fn measured_model_that_cannot_fit_returns_the_start_limit() {
    let p = Preview::new(true, |host| {
        host["resource_policy"]["domains"]["unified"]["memory"] = json!("distinct");
        host["resource_policy"]["domains"]["vram"] = json!({
            "memory":"device","device":"gpu0","managed_limit":"8GiB","free_reserve":"1GiB",
            "parked_limit":"0B"
        });
        host["resource_policy"]["devices"]["gpu0"]["domain"] = json!("vram");
    });
    p.measure(json!("/weights"), 1 << 30);
    let config = json!({"name":"preview","engine":"vllm","model":"/weights",
        "engine_config":{"memory":{"kv_cache":"16GiB"}}});
    let (status, value) = p.request(json!({"config":config})).await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["verdict"], "does_not_fit", "{value}");
    let limit = value["limit"].as_str().unwrap();
    assert!(limit.starts_with("insufficient_device_memory:"), "{limit}");
    assert!(
        limit.contains("8.0 GiB") || limit.contains("8589934592"),
        "{limit}"
    );
    // This is the resolution refusal start records as CheckpointUnusable;
    // actions.rs preserves it verbatim (its device-memory refusal test).
    let refusal = capyctl_config::effective::resolve_effective_with_checkpoint(
        &value["config"],
        &p.host,
        capyctl_config::effective::CheckpointFacts {
            weights_bytes: Some(1 << 30),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert_eq!(limit, refusal.detail);
}

// U7 R21: fits managed memory only after other models make room.
#[tokio::test]
async fn model_that_fits_only_on_an_empty_host_needs_room() {
    let p = Preview::new(true, |_| {});
    let measured = p.measure(json!("/weights"), 1 << 30);
    // Fixture a held owner in the same durable table ordinary starts read.
    let db = rusqlite::Connection::open(p.dir.path().join("srv.sqlite3")).unwrap();
    let golden: Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    let mut config = golden["input"]["deployment"].clone();
    config["name"] = json!("preview");
    config["routes"] = json!(["preview"]);
    config["model"]["path"] = json!("/weights");
    db.execute("INSERT INTO resource_owners(owner_id,footprint_json,deployment_id) VALUES(?1,?2,?1)",
        rusqlite::params![measured,json!({"version":1,"phase":"ready","allocations":[["unified",28_i64 << 30,0]],"devices":[]}).to_string()]).unwrap();
    let (status, value) = p.request(json!({"config":config})).await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["verdict"], "needs_room", "{value}");
    assert!(value["reason"].is_null(), "{value}");
    assert!(value["hosts"][0]["reason"].is_null(), "{value}");
}

// U7 R21: metadata gives an estimate even before any deployment exists.
#[tokio::test]
async fn unstored_hf_weights_are_estimated_and_second_check_does_not_query() {
    let hub = mirror(Some(1 << 30), None).await;
    let p = Preview::new(true, |host| {
        host["model_sources"]["huggingface_endpoint"] = json!(hub.endpoint())
    });
    let body = json!({"yaml":"name: preview\nengine: vllm\nmodel: {hf: org/model}\n"});
    let (status, value) = p.request(body.clone()).await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["verdict"], "fits");
    assert_eq!(value["estimated"], true);
    let queries = hub.queries.load(Ordering::SeqCst);
    assert_eq!(queries, 2);
    assert_eq!(p.request(body).await.1, value);
    assert_eq!(hub.queries.load(Ordering::SeqCst), queries);
}

// U7 R21: the standalone host sizes only weight files, not ancillary files.
#[tokio::test]
async fn local_checkpoint_size_is_estimated() {
    let p = Preview::new(true, |_| {});
    let model = p.dir.path().join("checkpoint");
    std::fs::create_dir(&model).unwrap();
    std::fs::write(model.join("model.safetensors"), vec![0; 4096]).unwrap();
    let (status, value) = p
        .request(json!({"config":{"name":"preview","engine":"vllm","model":model}}))
        .await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["verdict"], "fits");
    assert_eq!(value["estimated"], true);
}

// U7 R21: only the host's mirror and protected token file authorize pinning.
#[tokio::test]
async fn mirror_and_private_repository_token_come_from_the_host_document() {
    let hub = mirror(Some(1 << 30), Some("fixture-private-token")).await;
    let secret_dir = directory();
    let token = secret_dir.path().join("hf-token");
    std::fs::write(&token, "fixture-private-token\n").unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    let p =
        Preview::new(
            true,
            |host| {
                host["model_sources"] =
                    json!({"huggingface_endpoint":hub.endpoint(),"huggingface_token_file":token})
            },
        );
    let (status, value) = p.request(json!({"config":hf()})).await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["config"]["model"]["source"]["revision"], SHA);
    assert_eq!(hub.queries.load(Ordering::SeqCst), 2);
}

// U7 R21: a pin is useful even when no source size is available.
#[tokio::test]
async fn absent_measurement_and_source_sizes_return_weights_unknown() {
    let hub = mirror(None, None).await;
    let p = Preview::new(true, |host| {
        host["model_sources"]["huggingface_endpoint"] = json!(hub.endpoint())
    });
    let (status, value) = p.request(json!({"config":hf()})).await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["verdict"], "unknown");
    assert_eq!(value["reason"], "weights_unknown");
    // Declaring a memory request does not invent a checkpoint measurement.
    let golden: Value = serde_json::from_str(include_str!(
        "../../capyctl-config/tests/fixtures/effective-vllm-golden.json"
    ))
    .unwrap();
    let mut config = golden["input"]["deployment"].clone();
    config["model"] = json!({"hf":"org/model"});
    let (status, value) = p.request(json!({"config":config})).await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["verdict"], "unknown");
    assert_eq!(value["reason"], "weights_unknown");
}

// U7 R21: the host has not published a first memory observation.
#[tokio::test]
async fn host_without_observations_is_unknown() {
    let p = Preview::new(false, |_| {});
    let (status, value) = p
        .request(json!({"config":{"name":"preview","engine":"vllm","model":"/weights"}}))
        .await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["verdict"], "unknown");
    assert_eq!(value["reason"], "no_observations");
}

// U7 R22: the CLI and preview pin the same shorthand with the shared code.
#[tokio::test]
async fn hf_shorthand_matches_the_revision_deploy_stores() {
    let hub = mirror(Some(1 << 30), None).await;
    let p = Preview::new(true, |host| {
        host["model_sources"]["huggingface_endpoint"] = json!(hub.endpoint())
    });
    let text = "name: preview\nengine: vllm\nmodel: {hf: org/model}\n";
    let (_, value) = p.request(json!({"yaml":text})).await;
    let cli = capyctl_config::deployment_pin::prepare(
        text,
        None,
        &Hub::new(hub.origin.parse().unwrap(), None, CLI_TOKEN_HINT),
    )
    .await
    .unwrap();
    assert_eq!(value["config"], cli);
    let owned = p.owner.lock().unwrap();
    let receipt = owned
        .store()
        .create_stopped_managed_configuration(
            owned.session(),
            "owner",
            "cli-deploy",
            &json!({"config":cli}).to_string(),
            &p.host,
            1000,
        )
        .unwrap();
    let stored = owned
        .store()
        .effective_configuration(&receipt.deployment_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        value["config"]["model"]["source"],
        stored.effective["model"]["source"]
    );
}

// U7 R22: a service cannot infer the caller's home directory.
#[tokio::test]
async fn home_relative_path_is_refused_with_the_absolute_path_rule() {
    let p = Preview::new(true, |_| {});
    let (status, value) = p
        .request(json!({"yaml":"name: preview\nengine: vllm\nmodel: ~/weights\n"}))
        .await;
    assert_eq!(status, 400);
    assert!(value["errors"][0]["message"]
        .as_str()
        .unwrap()
        .contains("absolute path"));
}

// U7 R22: parser and schema errors match validate config's strict parser.
#[tokio::test]
async fn duplicate_key_and_cli_rejected_field_have_identical_errors() {
    let p = Preview::new(true, |_| {});
    for text in [
        "name: preview\nengine: vllm\nmodel: /weights\nname: again\n",
        "name: preview\nengine: vllm\nmodel: /weights\nnot_a_field: true\n",
    ] {
        let expected =
            capyctl_config::parse_strict(capyctl_config::ConfigKind::Deployment, text).unwrap_err();
        let (status, value) = p.request(json!({"yaml":text})).await;
        assert_eq!(status, 400);
        assert_eq!(value["errors"][0]["message"], expected.to_string());
        if text.contains("name: again") {
            assert_eq!(value["errors"][0]["position"]["line"], 4);
        }
    }
}

// U7 R22: a request must choose exactly one representation.
#[tokio::test]
async fn neither_yaml_nor_config_is_invalid_request() {
    let p = Preview::new(true, |_| {});
    let (status, value) = p.request(json!({})).await;
    assert_eq!(status, 400);
    assert_eq!(value["error"]["code"], "invalid_request");
}

// U7: the server refuses until U9b supplies host-admin forwarding. It must
// never substitute the server machine's paths, mirror or credentials.
#[tokio::test]
async fn server_provider_refuses_until_host_admin_forwarding_is_installed() {
    let mut p = Preview::new(true, |_| {});
    let configuration =
        Arc::new(SharedConfigurationSource::new(p.owner.clone(), p.host.clone(), "owner").unwrap());
    p.app = check::router(
        configuration,
        Arc::new(check::UnavailableSources),
        Some(p.worker.commands()),
    )
    .layer(Extension(Principal::Admin));
    let (status, value) = p
        .request(json!({"config":{"name":"preview","engine":"vllm","model":"/weights"}}))
        .await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["verdict"], "unknown", "{value}");
    assert_eq!(value["reason"], "host_unavailable", "{value}");
}

// T26 T29 / ADR 0035 §8 / SPEC §7.2: admission's holder explanation is shared
// with Start even when the model fits the configured managed limit.
#[tokio::test]
async fn low_observed_free_memory_returns_the_actual_start_refusal() {
    let p = Preview::new(true, |host| {
        host["resource_policy"]["domains"]["unified"]["memory"] = json!("distinct");
        host["resource_policy"]["domains"]["vram"] = json!({
            "memory":"device","device":"gpu0","managed_limit":"64GiB","free_reserve":"1GiB",
            "parked_limit":"0B"
        });
        host["resource_policy"]["devices"]["gpu0"]["domain"] = json!("vram");
    });
    p.measure(json!("/weights"), 1 << 30);
    for observation in p.readings.0.lock().unwrap().iter_mut() {
        if observation.domain == "vram" {
            observation.available_bytes = 1 << 30;
        }
    }
    let config = json!({"name":"preview","engine":"vllm","model":"/weights",
        "engine_config":{"memory":{"kv_cache":"16GiB"}}});
    let (status, absent) = p.request(json!({"config": config.clone()})).await;
    assert_eq!(status, 200, "{absent}");
    assert_eq!(absent["verdict"], "does_not_fit", "{absent}");
    for observation in p.readings.0.lock().unwrap().iter_mut() {
        if observation.domain == "vram" {
            observation.outside_processes = Some(capyctl_domain::resources::OutsideProcesses {
                processes: vec![capyctl_domain::resources::OutsideProcess {
                    pid: 4242,
                    name: Some("trainer".into()),
                    bytes: 24 << 30,
                    capyctl_state_dir: None,
                    other_capyctl: false,
                }],
                remainder: None,
            });
        }
    }
    let (status, value) = p.request(json!({"config": config})).await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["verdict"], "does_not_fit", "{value}");
    let limit = value["limit"].as_str().unwrap();
    assert!(
        limit.starts_with(absent["limit"].as_str().unwrap()),
        "{limit}"
    );
    assert!(limit.contains("trainer"), "{limit}");
    assert!(limit.contains("4242"), "{limit}");
    let observations = p.readings.0.lock().unwrap().clone();
    let owned = p.owner.lock().unwrap();
    let store = owned.store();
    let receipt = store
        .create_stopped_managed_configuration(
            owned.session(),
            "owner",
            "low-free-start",
            &json!({"config": value["config"]}).to_string(),
            &p.host,
            1000,
        )
        .unwrap();
    // First start still needs digest evidence for this newly stored revision;
    // preview's size-only evidence belongs to the earlier measurement.
    let recorded = store
        .record_checkpoint_digest(
            owned.session(),
            &receipt.deployment_id,
            receipt.revision,
            "lab",
            &format!("sha256:{}", "a".repeat(64)),
            1 << 30,
            1000,
        )
        .unwrap();
    assert!(matches!(
        recorded,
        capyctl_store::checkpoint_digests::RecordOutcome::Recorded { .. }
    ));
    let start = store
        .accept_start(
            owned.session(),
            &capyctl_store::lifecycle::DeploymentFence {
                deployment_id: receipt.deployment_id,
                revision: receipt.revision,
                generation: receipt.generation,
            },
            1000,
            100_000,
        )
        .unwrap();
    let policy = store.resource_policy("lab").unwrap().unwrap();
    let limits: Vec<_> = policy
        .controls
        .domains
        .iter()
        .map(|(domain, control)| capyctl_domain::resources::MemoryLimit {
            domain: domain.clone(),
            managed_bytes: control.managed_limit,
            free_reserve_bytes: control.free_reserve,
            reserve_absorbs_unmanaged: control.memory
                == capyctl_config::effective::DomainMemory::Device,
            host_kv_bytes: control.host_kv_limit,
            parked_bytes: control.parked_limit,
        })
        .collect();
    let error = store
        .arm_step(
            owned.session(),
            &start.step_id,
            capyctl_scheduler::residency::AdmissionContext::new(
                &observations,
                &limits,
                1000,
                policy.controls.observation_ttl_ms,
                policy.controls.max_parked as usize,
            ),
        )
        .unwrap_err();
    let capyctl_store::lifecycle::LifecycleError::Rejected(reason) = error else {
        panic!("expected physical capacity refusal, got {error:?}");
    };
    assert!(reason.contains("insufficient_device_memory"), "{reason}");
    assert_eq!(value["limit"], reason);
}

// T14 / ADR 0040 §9: a drafter's source is sized along with the main
// checkpoint. Sparse files keep this a metadata-only preview regression.
#[tokio::test]
async fn drafter_weights_are_included_in_the_fit_estimate() {
    let p = Preview::new(true, |host| {
        host["runtime_profiles"]["local"]["security"]["approved_options"] =
            json!(["--speculative-config"]);
    });
    let model = p.dir.path().join("main-model");
    let draft = p.dir.path().join("draft-model");
    for (path, size) in [(&model, 1_u64 << 30), (&draft, 64_u64 << 30)] {
        std::fs::create_dir(path).unwrap();
        std::fs::File::create(path.join("model.safetensors"))
            .unwrap()
            .set_len(size)
            .unwrap();
    }
    let (status, value) = p
        .request(json!({"config":{
            "name":"preview", "engine":"vllm", "residency":"restart_only",
            "model":{"path":model,"draft":draft},
            "engine_config":{
                "accept_extra_args":true,
                "extra_args":["--speculative-config",
                    json!({"method":"draft_model","num_speculative_tokens":4}).to_string()]
            }
        }}))
        .await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["estimated"], true, "{value}");
    assert_eq!(value["verdict"], "does_not_fit", "{value}");
}

// T14 / ADR 0040 §9: structured source selectors survive the host pin/size
// round trip instead of silently widening a selected-file deployment.
#[tokio::test]
async fn structured_huggingface_files_survive_the_check() {
    let hub = mirror(Some(1 << 30), None).await;
    let p = Preview::new(true, |host| {
        host["model_sources"] = json!({"huggingface_endpoint":hub.endpoint()});
    });
    let source = json!({
        "type":"huggingface", "repo":"org/model", "revision":SHA,
        "files":["model.safetensors"]
    });
    let (status, value) = p
        .request(json!({"config":{
            "name":"preview", "engine":"vllm", "model":{"source":source}
        }}))
        .await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["verdict"], "fits", "{value}");
    assert_eq!(value["estimated"], true, "{value}");
    assert_eq!(value["config"]["model"]["source"]["files"], source["files"]);
    assert_eq!(value["config"]["model"]["source"]["revision"], SHA);
}

// T14 / U7 / ADR 0040 §9: sources not sized by the preview are unknown,
// never a zero-byte estimate from an empty source traversal.
#[tokio::test]
async fn an_unsized_http_checkpoint_is_unknown_not_zero_bytes() {
    let p = Preview::new(true, |_| {});
    let (status, value) = p.request(json!({"config":{
        "name":"preview","engine":"vllm","model":{"source":{
            "type":"http","url":"https://example.invalid/model.safetensors","sha256":"a".repeat(64)
        }}
    }})).await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(value["verdict"], "unknown", "{value}");
    assert_eq!(value["estimated"], false, "{value}");
}
