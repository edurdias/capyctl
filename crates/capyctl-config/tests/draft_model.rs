//! ADR 0008 amendment 2026-10-08: a deployment may declare its speculative
//! drafter's weights as a separate model source (`model.draft`), which
//! CapyCTL materializes, sizes and hands to the engine itself. CPU-only
//! resolution tests; none of this qualifies a native engine recipe (SPEC §18).

use capyctl_config::effective::{
    checkpoint_location, decode_effective_snapshot, deployment_command_fingerprint,
    resolve_effective, resolve_effective_with_checkpoint, resolve_snapshot_with_checkpoint,
    startup_graph_allowance, CheckpointFacts, DrafterLocation, Engine, ModelSource,
};
use capyctl_config::ConfigErrorCode;
use serde_json::{json, Value};
use std::path::PathBuf;

const GIB: i64 = 1 << 30;
const SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const OTHER_SHA: &str = "89abcdef0123456789abcdef0123456789abcdef";

fn digest(fill: char) -> String {
    fill.to_string().repeat(64)
}

fn http_draft(fill: char) -> Value {
    json!({"http": {"url": "https://drafts.example.test/d.tar", "sha256": digest(fill),
        "archive": "tar"}})
}

/// The fixture deployment on `engine` (no declared phases, a declared memory
/// request), the operator's speculation switch in its extra arguments, and
/// `draft` as its drafter source. Nothing is approved on the host.
fn fixture(engine: &str, draft: Option<Value>) -> (Value, Value) {
    let all: Value =
        serde_json::from_str(include_str!("fixtures/f2-deployment.json")).expect("fixture JSON");
    let (mut deployment, mut host) = (all["deployment"].clone(), all["host"].clone());
    if engine != "tensorfold" {
        // ADR 0023 §4: only TensorFold needs its phases declared.
        let object = deployment.as_object_mut().unwrap();
        object.remove("resources");
        object.remove("residency");
    }
    let profile = &mut host["runtime_profiles"]["local"];
    profile["engine"] = json!(engine);
    profile["args"] = json!([]);
    deployment["engine_config"]["memory"] = json!({"request": "40GiB", "kv_cache": "8GiB"});
    let extra = match engine {
        "sglang" => {
            profile["security"]["admin_credential_ref"] = json!("secret://engine-admin");
            json!(["--speculative-algorithm", "STANDALONE"])
        }
        "vllm" => {
            // ADR 0014 §8: the operator's own `--speculative-config` keeps
            // its named approval; the drafter's path needs none.
            profile["security"]["approved_options"] = json!(["--speculative-config"]);
            json!([
                "--speculative-config",
                json!({"method": "draft_model", "num_speculative_tokens": 4}).to_string()
            ])
        }
        _ => {
            profile["executable"] = json!("/opt/tf/bin/tensorfold");
            profile["build_fingerprint"] = json!("0.6.5");
            profile["security"]["deep_park"] = json!("disabled");
            deployment["residency"] = json!("restart_only");
            deployment["engine_config"] = json!({"context_length": 8192});
            json!([])
        }
    };
    if extra.as_array().is_some_and(|args| !args.is_empty()) {
        deployment["engine_config"]["accept_extra_args"] = json!(true);
        deployment["engine_config"]["extra_args"] = extra;
    }
    if let Some(draft) = draft {
        deployment["model"]["draft"] = draft;
    }
    (deployment, host)
}

fn weights() -> CheckpointFacts {
    CheckpointFacts {
        weights_bytes: Some(GIB),
        ..Default::default()
    }
}

// T14: every spelling `model.source` accepts is a drafter source, resolved
// the way the model's own source is: a remote one into its fixed directory
// in the host's sources store, a relative local one against the model
// store. The frozen revision round-trips.
#[test]
fn a_drafter_source_resolves_in_every_spelling_beside_the_weights() {
    for (draft, source, resolved) in [
        (
            json!({"type": "huggingface", "repo": "acme/draft-1b", "revision": SHA}),
            ModelSource::HuggingFace {
                repo: "acme/draft-1b".into(),
                revision: SHA.into(),
                files: Vec::new(),
                token_ref: None,
            },
            format!("/srv/models/sources/huggingface/acme--draft-1b@{SHA}"),
        ),
        (
            json!({"huggingface": {"repo": "acme/draft-1b", "revision": SHA,
                "token_ref": "secret://hf"}}),
            ModelSource::HuggingFace {
                repo: "acme/draft-1b".into(),
                revision: SHA.into(),
                files: Vec::new(),
                token_ref: Some("secret://hf".into()),
            },
            format!("/srv/models/sources/huggingface/acme--draft-1b@{SHA}"),
        ),
        (
            http_draft('a'),
            ModelSource::Http {
                url: "https://drafts.example.test/d.tar".into(),
                sha256: digest('a'),
                archive: capyctl_config::effective::Archive::Tar,
            },
            format!("/srv/models/sources/http/{}-tar", digest('a')),
        ),
        (
            json!({"type": "local", "path": "drafts/d"}),
            ModelSource::Local {
                path: "drafts/d".into(),
            },
            "/srv/models/drafts/d".to_owned(),
        ),
        (
            json!("drafts/d"),
            ModelSource::Local {
                path: "drafts/d".into(),
            },
            "/srv/models/drafts/d".to_owned(),
        ),
        (
            json!({"hf": format!("acme/draft-1b@{SHA}")}),
            ModelSource::HuggingFace {
                repo: "acme/draft-1b".into(),
                revision: SHA.into(),
                files: Vec::new(),
                token_ref: None,
            },
            format!("/srv/models/sources/huggingface/acme--draft-1b@{SHA}"),
        ),
        (
            json!({"local": {"path": "/opt/drafts/d"}}),
            ModelSource::Local {
                path: "/opt/drafts/d".into(),
            },
            "/opt/drafts/d".to_owned(),
        ),
    ] {
        let (deployment, host) = fixture("sglang", Some(draft.clone()));
        let effective = resolve_effective_with_checkpoint(&deployment, &host, weights())
            .unwrap_or_else(|error| panic!("{draft} must resolve: {error}"));
        let declared = effective.model.draft.as_ref().expect("a drafter");
        assert_eq!(declared.source, source, "{draft}");
        assert_eq!(
            declared.resolved_path.as_deref(),
            Some(resolved.as_str()),
            "{draft}"
        );
        // The model's own source is unchanged by its drafter.
        assert_eq!(
            effective.model.resolved_path.as_deref(),
            Some("/srv/models/toy")
        );
        let text = serde_json::to_string(&effective).unwrap();
        assert_eq!(decode_effective_snapshot(&text).unwrap(), effective);
        // ADR 0014 §7: re-resolving the frozen revision with checkpoint facts
        // keeps the drafter.
        let again = resolve_snapshot_with_checkpoint(&text, weights()).unwrap();
        assert_eq!(again.model.draft, effective.model.draft, "{draft}");
    }
    // A stated sources store holds a remote drafter instead of the model store.
    let (deployment, mut host) = fixture("sglang", Some(http_draft('a')));
    host["model_sources"] = json!({"path": "/state/models"});
    let effective = resolve_effective(&deployment, &host).unwrap();
    assert_eq!(
        effective.model.draft.unwrap().resolved_path,
        Some(format!("/state/models/sources/http/{}-tar", digest('a')))
    );
}

// T14 (SPEC §13.3, ADR 0008): a drafter is validated exactly like the
// model's source (pinned commits and digests, HTTPS, secret references),
// and every refusal names `model.draft`.
#[test]
fn a_drafter_source_is_validated_like_the_model_source() {
    for draft in [
        json!({"type": "huggingface", "repo": "acme/draft-1b"}),
        json!({"type": "huggingface", "repo": "acme/draft-1b", "revision": "main"}),
        json!({"type": "huggingface", "repo": "acme/draft-1b", "revision": SHA,
            "token_ref": "hf_plaintext"}),
        json!({"type": "http", "url": "http://drafts.example.test/d", "sha256": digest('a')}),
        json!({"type": "http", "url": "https://drafts.example.test/d", "sha256": "abc"}),
        json!({"type": "local", "path": ""}),
        json!({"type": "s3", "path": "/d"}),
        json!("~/drafts/d"),
        json!({"hf": "acme/draft-1b@main"}),
    ] {
        let (deployment, host) = fixture("sglang", Some(draft.clone()));
        let error =
            resolve_effective(&deployment, &host).expect_err(&format!("{draft} must be refused"));
        // The strict parse names a missing field under `deployment.`.
        assert!(
            error
                .path
                .trim_start_matches("deployment.")
                .starts_with("model.draft"),
            "{draft}: {} at {}",
            error.detail,
            error.path
        );
    }
}

// T14 T37 (ADR 0008): a host's `model_sources` policy decides a drafter's
// source exactly as the model's: a kind the host turned off, or an origin
// outside its allowed hosts, is refused `model_source_denied` at `model.draft`.
#[test]
fn the_host_policy_refuses_a_denied_drafter_source() {
    for policy in [
        json!({"http": "denied"}),
        json!({"http": "disabled"}),
        json!({"allowed_hosts": ["huggingface.co"]}),
    ] {
        let (deployment, mut host) = fixture("sglang", Some(http_draft('a')));
        host["model_sources"] = policy.clone();
        let error = resolve_effective(&deployment, &host).expect_err(&policy.to_string());
        assert_eq!(error.code, ConfigErrorCode::ModelSourceDenied, "{policy}");
        assert_eq!(error.path, "model.draft", "{policy}");
    }
    // The same host serves the deployment without the drafter, and a local
    // drafter is never a policy question.
    let (deployment, mut host) = fixture("sglang", None);
    host["model_sources"] = json!({"http": "denied", "huggingface": "denied"});
    assert!(resolve_effective(&deployment, &host).is_ok());
    let (deployment, mut host) = deployment_local_draft();
    host["model_sources"] = json!({"http": "denied", "huggingface": "denied"});
    assert!(resolve_effective(&deployment, &host).is_ok());
}

fn deployment_local_draft() -> (Value, Value) {
    fixture("sglang", Some(json!({"type": "local", "path": "drafts/d"})))
}

// T14: a deployment without a drafter encodes, and fingerprints, exactly as
// before the field existed (goldens computed on the parent commit).
#[test]
fn an_absent_drafter_keeps_every_encoding_and_fingerprint() {
    let all: Value =
        serde_json::from_str(include_str!("fixtures/f2-deployment.json")).expect("fixture JSON");
    let (deployment, host) = (all["deployment"].clone(), all["host"].clone());
    assert_eq!(
        deployment_command_fingerprint(&deployment, 300_000).unwrap(),
        "29c1337aca5fdb3a56e9e70e023f57af5906c2e9567c38165dde76e7e77431e2"
    );
    let effective = resolve_effective(&deployment, &host).unwrap();
    assert_eq!(
        effective.recipe_fingerprint,
        "e011aa21a8961a1cc894d17e5ea69f147ffc93f17b20b3ee5675643174fdba75"
    );
    let model = serde_json::to_value(&effective.model).unwrap();
    assert_eq!(
        model.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["content_fingerprint", "resolved_path", "revision", "source"]
    );
}

// T14: the drafter's pinned declaration is part of what a deployment asks
// for, so two deployments differing only in its digest or commit are
// different revisions, and each differs from one without a drafter.
#[test]
fn the_drafters_pin_is_part_of_the_revision_identity() {
    let command = |draft: Option<Value>| {
        let (deployment, _) = fixture("sglang", draft);
        deployment_command_fingerprint(&deployment, 300_000).unwrap()
    };
    let recipe = |draft: Option<Value>| {
        let (deployment, host) = fixture("sglang", draft);
        resolve_effective(&deployment, &host)
            .unwrap()
            .recipe_fingerprint
    };
    let hf =
        |revision: &str| json!({"huggingface": {"repo": "acme/draft-1b", "revision": revision}});
    for identity in [&command as &dyn Fn(Option<Value>) -> String, &recipe] {
        let none = identity(None);
        let a = identity(Some(http_draft('a')));
        assert_eq!(a, identity(Some(http_draft('a'))), "deterministic");
        assert_ne!(a, none);
        assert_ne!(a, identity(Some(http_draft('b'))), "the digest is identity");
        let commit = identity(Some(hf(SHA)));
        assert_ne!(
            commit,
            identity(Some(hf(OTHER_SHA))),
            "the commit is identity"
        );
        assert_ne!(commit, none);
    }
}

// T14 T41 (ADR 0014 §8, ADR 0023 §5): CapyCTL renders the drafter's path
// itself, so engine arguments that already name a draft model, or that do
// not turn speculation on where the engine needs it, are refused at
// `model.draft` with the reason named.
#[test]
fn engine_arguments_leave_the_drafter_path_to_capyctl() {
    let with = |engine: &str, extra: Value, approved: Value| {
        let (mut deployment, mut host) = fixture(engine, Some(http_draft('a')));
        let profile = &mut host["runtime_profiles"]["local"];
        profile["security"]["approved_options"] = approved;
        profile["security"]["approved_paths"] = json!(["/srv/drafters"]);
        deployment["engine_config"]["accept_extra_args"] = json!(true);
        deployment["engine_config"]["extra_args"] = extra;
        resolve_effective(&deployment, &host)
    };
    let refused = |result: Result<_, capyctl_config::ConfigError>, needle: &str| {
        let error = result.expect_err(needle);
        assert_eq!(error.path, "model.draft", "{error}");
        assert!(error.detail.contains(needle), "{needle}: {error}");
    };
    // SGLang: speculation is the operator's `--speculative-algorithm`; the
    // draft path is CapyCTL's.
    refused(
        with("sglang", json!([]), json!([])),
        "--speculative-algorithm",
    );
    refused(
        with(
            "sglang",
            json!([
                "--speculative-algorithm",
                "STANDALONE",
                "--speculative-draft-model-path",
                "/srv/drafters/d"
            ]),
            json!(["--speculative-draft-model-path"]),
        ),
        "--speculative-draft-model-path",
    );
    // vLLM: the operator's `--speculative-config` states the method and the
    // token count; its `model` is CapyCTL's.
    refused(with("vllm", json!([]), json!([])), "--speculative-config");
    refused(
        with(
            "vllm",
            json!([
                "--speculative-config",
                json!({"method": "draft_model", "model": "/srv/drafters/d",
                    "num_speculative_tokens": 4})
                .to_string()
            ]),
            json!(["--speculative-config"]),
        ),
        "`model`",
    );
    // TensorFold: `--drafter` is CapyCTL's; drafts off contradicts a drafter.
    for extra in [
        json!(["--drafter", "/srv/drafters/d"]),
        json!(["--drafter=/srv/drafters/d"]),
        json!(["--no-drafts"]),
    ] {
        refused(
            with("tensorfold", extra.clone(), json!(["--drafter"])),
            "--drafter",
        );
    }
    // The host-fixed arguments are held to the same rule.
    let (deployment, mut host) = fixture("tensorfold", Some(http_draft('a')));
    host["runtime_profiles"]["local"]["args"] = json!(["--no-drafts"]);
    refused(resolve_effective(&deployment, &host), "--no-drafts");
    // Each engine's operator switch alone resolves.
    for engine in ["sglang", "vllm", "tensorfold"] {
        let (deployment, host) = fixture(engine, Some(http_draft('a')));
        resolve_effective(&deployment, &host).unwrap_or_else(|error| panic!("{engine}: {error}"));
    }
}

// T14 (ADR 0014 §5 amendments A6, A8): a declared drafter's weights are
// counted with the checkpoint's, located in the store CapyCTL materialized
// it into, with no `approved_paths` or `approved_options` on the host; and
// the derived startup peak carries its CUDA graphs too.
#[test]
fn a_declared_drafter_is_sized_with_the_checkpoint_without_approvals() {
    for (draft, root, path, outside) in [
        (
            http_draft('a'),
            "/srv/models",
            format!("/srv/models/sources/http/{}-tar", digest('a')),
            false,
        ),
        (
            json!({"type": "local", "path": "drafts/d"}),
            "/srv/models",
            "/srv/models/drafts/d".to_owned(),
            true,
        ),
    ] {
        let (deployment, host) = fixture("sglang", Some(draft.clone()));
        assert!(host["runtime_profiles"]["local"]["security"]
            .get("approved_paths")
            .is_none());
        let expected = DrafterLocation {
            root: PathBuf::from(root),
            path: PathBuf::from(&path),
            outside_root_allowed: outside,
        };
        let effective = resolve_effective(&deployment, &host).unwrap();
        assert_eq!(
            effective.drafter_location(),
            Some(expected.clone()),
            "{draft}"
        );
        assert_eq!(
            checkpoint_location(&deployment, &host).unwrap().drafter,
            Some(expected),
            "{draft}"
        );
        assert_eq!(
            effective.engine_config.memory().startup_graphs_bytes,
            Some(startup_graph_allowance(Engine::Sglang, true)),
            "{draft}"
        );
    }
}
