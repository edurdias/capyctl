//! Owner decision 2026-09-25 (ADR 0014 amendment "minimal deployment
//! file"): a deployment document needs only three fields,
//!
//! ```yaml
//! name: my-model
//! engine: vllm                 # a runtime profile name
//! model: ~/models/Qwen3-4B     # or Qwen3-4B, or {hf: Qwen/Qwen3-4B@<commit>}
//! ```
//!
//! and everything else is defaulted when absent. Every advanced field stays
//! optional and, when stated, means what it always meant: a full document
//! resolves exactly as before.
//!
//! The defaults come in two layers, both shared by every role (standalone is a
//! server plus one host):
//!
//! - [`expand`] fills what the document alone decides (`schema_version`,
//!   `kind`, `routes`, `runtime_profile` from `engine`, `recipe`, `recovery`,
//!   the model shorthands, `model.content_fingerprint`, `model.revision`). It
//!   runs inside the strict deployment parse, so the CLI, the management API,
//!   the store and the agent all see the same completed document.
//! - [`for_host`] fills what depends on the host a deployment is resolved
//!   against (the profile an engine family name stands for, the profile's
//!   published revision, the GPU). Residency, the KV cache and the memory
//!   request depend on the checkpoint too, so resolution decides them
//!   ([`default_residency`], [`default_kv_cache`]) and names them in the
//!   engine configuration's provenance, so a revision re-resolved with the
//!   measured weights decides them again (ADR 0014 §7).

use serde_json::{json, Map, Value};

use crate::model_source::{is_commit_sha, ModelSource};
use crate::{ConfigError, ConfigErrorCode};

/// The recipe a deployment runs when it names none.
pub const DEFAULT_RECIPE: &str = "standard";
/// The recovery policy a deployment runs when it names none.
pub const DEFAULT_RECOVERY: &str = "reconcile";
/// `model.revision` when none is stated: the operator's own label for the
/// model's version, which a later revision changes to say "new weights".
pub const DEFAULT_MODEL_REVISION: &str = "1";
/// `model.content_fingerprint` when none is stated. Not in digest form, so it
/// is a label, not an expectation: capyctl measures the checkpoint's digest on
/// the host (ADR 0014 §7) and records it. A stated `sha256:<64 hex>` is an
/// expectation the measurement must match.
pub const MEASURED_FINGERPRINT: &str = "measured";
fn invalid(path: &str, detail: impl Into<String>) -> ConfigError {
    ConfigError::new(ConfigErrorCode::UnsupportedCombination, path, detail)
}

/// A `hf:` shorthand split into the repository and the revision after `@`.
pub fn split_hf(text: &str) -> (&str, Option<&str>) {
    match text.split_once('@') {
        Some((repo, revision)) => (repo, Some(revision)),
        None => (text, None),
    }
}

/// Where a `hf:` shorthand may be written, as JSON pointers: the model, and
/// (ADR 0008 amendment 2026-10-08) its drafter.
pub const HF_SHORTHANDS: [&str; 2] = ["/model", "/model/draft"];

/// The `hf:` shorthand at `at` (one of [`HF_SHORTHANDS`]) of a raw
/// deployment document that is not pinned to a commit yet: `(repository,
/// reference)`, the reference `main` when none is written. `capyctl deploy
/// model --file` pins it before the document is parsed; `capyctl validate
/// config`, which never contacts the network, refuses it.
pub fn unpinned_hf(document: &Value, at: &str) -> Option<(String, String)> {
    let text = document.pointer(at)?.get("hf")?.as_str()?;
    let (repo, revision) = split_hf(text);
    match revision {
        Some(revision) if is_commit_sha(revision) => None,
        Some(revision) => Some((repo.to_owned(), revision.to_owned())),
        None => Some((repo.to_owned(), "main".to_owned())),
    }
}

/// Replace the `hf:` shorthand's reference at `at` with the commit it
/// resolved to.
pub fn pin_hf(document: &mut Value, at: &str, repo: &str, commit: &str) {
    if let Some(block) = document.pointer_mut(at).and_then(Value::as_object_mut) {
        block.insert("hf".into(), Value::String(format!("{repo}@{commit}")));
    }
}

/// Expand a leading `~/` in a `model:` path shorthand (or `model.path`), and
/// in the drafter's (`model.draft`, or its `path`), against `home`, as a
/// shell would. The capyctl CLI calls it on the file it reads; `home` is its
/// own home directory.
pub fn expand_home(document: &mut Value, home: Option<&std::path::Path>) {
    let Some(home) = home.filter(|home| home.is_absolute()) else {
        return;
    };
    fn slot(block: &mut Value) -> Option<&mut Value> {
        if block.is_string() {
            Some(block)
        } else {
            block.get_mut("path")
        }
    }
    let expand = |slot: Option<&mut Value>| {
        let Some(slot) = slot else {
            return;
        };
        if let Some(rest) = slot.as_str().and_then(|path| path.strip_prefix("~/")) {
            *slot = Value::String(home.join(rest).to_string_lossy().into_owned());
        }
    };
    let Some(model) = document.get_mut("model") else {
        return;
    };
    expand(model.get_mut("draft").and_then(slot));
    expand(slot(model));
}

/// Fill the defaults the document alone decides. Idempotent: a full document
/// is returned unchanged, key for key. A document whose `kind` names another
/// kind is left for the kind check to refuse.
pub fn expand(document: &mut Value) -> Result<(), ConfigError> {
    let Some(object) = document.as_object_mut() else {
        return Ok(());
    };
    if object
        .get("kind")
        .is_some_and(|kind| kind.as_str() != Some("deployment"))
    {
        return Ok(());
    }
    object.entry("schema_version").or_insert(json!(1));
    object.entry("kind").or_insert(json!("deployment"));
    // `engine` is the short name of `runtime_profile` (the name of the engine
    // installation the host publishes, or an engine family, see
    // [`profile_on_host`]).
    if let Some(engine) = object.remove("engine") {
        let name = engine
            .as_str()
            .filter(|name| !name.is_empty())
            .ok_or_else(|| invalid("engine", "must name a runtime profile, e.g. `vllm`"))?;
        match object.get("runtime_profile") {
            Some(profile) if profile.as_str() != Some(name) => {
                return Err(invalid(
                    "engine",
                    "`engine` is the short form of `runtime_profile`; state one of them",
                ))
            }
            _ => {
                object.insert("runtime_profile".into(), json!(name));
            }
        }
    }
    if !object.contains_key("routes") {
        if let Some(name) = object.get("name").and_then(Value::as_str) {
            object.insert("routes".into(), json!([name]));
        }
    }
    object.entry("recipe").or_insert(json!(DEFAULT_RECIPE));
    object.entry("recovery").or_insert(json!(DEFAULT_RECOVERY));
    if let Some(model) = object.get_mut("model") {
        expand_model(model)?;
    }
    Ok(())
}

/// SPEC §7: `model: <path>` is a local path (absolute, or relative to the
/// models directory); `model: {hf: owner/repo@<commit>}` is a pinned Hugging
/// Face source (ADR 0008). The drafter (`model.draft`) takes the same two.
fn expand_model(model: &mut Value) -> Result<(), ConfigError> {
    if let Some(path) = model.as_str() {
        path_shorthand(path, "model")?;
        *model = json!({"path": path});
    }
    let Some(block) = model.as_object_mut() else {
        return Ok(());
    };
    if let Some(hf) = block.remove("hf") {
        if block.contains_key("path") || block.contains_key("source") {
            return Err(invalid("model.hf", "state one of `hf`, `path` or `source`"));
        }
        block.insert("source".into(), hf_source(&hf, "model.hf")?);
    }
    if block
        .get("path")
        .and_then(Value::as_str)
        .is_some_and(|path| path.starts_with('~'))
    {
        return Err(invalid(
            "model.path",
            "a path starting with `~` is expanded by the capyctl CLI; send an absolute path, \
             or one relative to the models directory",
        ));
    }
    if let Some(draft) = block.get_mut("draft") {
        expand_draft(draft)?;
    }
    block
        .entry("content_fingerprint")
        .or_insert(json!(MEASURED_FINGERPRINT));
    block
        .entry("revision")
        .or_insert(json!(DEFAULT_MODEL_REVISION));
    Ok(())
}

/// ADR 0008 amendment 2026-10-08: `model.draft: <path>` is a local drafter
/// and `model.draft: {hf: owner/repo@<commit>}` a pinned Hugging Face one;
/// a written source is kept as written for the strict parse.
fn expand_draft(draft: &mut Value) -> Result<(), ConfigError> {
    if let Some(path) = draft.as_str() {
        path_shorthand(path, "model.draft")?;
        *draft = json!({"type": "local", "path": path});
        return Ok(());
    }
    let Some(block) = draft.as_object() else {
        return Ok(());
    };
    if let Some(hf) = block.get("hf") {
        if block.len() != 1 {
            return Err(invalid(
                "model.draft.hf",
                "state `hf` alone, or the drafter's source written out",
            ));
        }
        *draft = hf_source(hf, "model.draft.hf")?;
    }
    Ok(())
}

/// A path shorthand at `at`: not empty, and not home-relative.
fn path_shorthand(path: &str, at: &str) -> Result<(), ConfigError> {
    if path.is_empty() {
        return Err(invalid(at, "must not be empty"));
    }
    // A home-relative path means the home of whoever wrote it; the capyctl
    // CLI expands it ([`expand_home`]) before the document leaves the
    // machine, so a document that still has one came from elsewhere.
    if path.starts_with('~') {
        return Err(invalid(
            at,
            format!(
                "`{path}`: a path starting with `~` is expanded by the capyctl CLI; \
                 send an absolute path, or one relative to the models directory"
            ),
        ));
    }
    Ok(())
}

/// The pinned Hugging Face source an `hf:` shorthand at `at` names.
fn hf_source(hf: &Value, at: &str) -> Result<Value, ConfigError> {
    let text = hf
        .as_str()
        .ok_or_else(|| invalid(at, "must be `owner/repo` or `owner/repo@<commit>`"))?;
    let (repo, revision) = split_hf(text);
    let revision = revision
        .filter(|revision| is_commit_sha(revision))
        .ok_or_else(|| {
            invalid(
                at,
                format!(
                    "`{text}` is not pinned to a commit: `capyctl deploy model --file` pins it \
                     to the commit it names now, or write `{repo}@<40-character commit>` \
                     (so the same document always means the same bytes)"
                ),
            )
        })?;
    let source = ModelSource::HuggingFace {
        repo: repo.to_owned(),
        revision: revision.to_owned(),
        files: Vec::new(),
        token_ref: None,
    };
    source
        .validate()
        .map_err(|error| ConfigError::new(error.code, at, error.detail))?;
    Ok(serde_json::to_value(&source).expect("a model source serializes"))
}

/// The runtime profile `requested` names on the host document `host`: the
/// profile of that name, else, when `requested` is an engine family (`vllm`,
/// `sglang`, `tensorfold`), the one profile of that family the host publishes. `None` when
/// neither (or several profiles of the family) exist.
pub fn profile_on_host(requested: &str, host: &Value) -> Option<String> {
    let profiles = host.get("runtime_profiles")?.as_object()?;
    if profiles.contains_key(requested) {
        return Some(requested.to_owned());
    }
    // An `engine:` value may name an engine family instead of a profile.
    crate::engine_policy::Engine::from_name(requested)?;
    let mut family = profiles
        .iter()
        .filter(|(_, profile)| profile["engine"].as_str() == Some(requested))
        .map(|(name, _)| name);
    match (family.next(), family.next()) {
        (Some(name), None) => Some(name.clone()),
        _ => None,
    }
}

/// Fill the defaults the host decides, on a document [`expand`] completed:
/// the profile an engine family name stands for, the profile's published
/// revision (`runtime_profile_revision`: the latest the host publishes), and
/// the GPU when no `devices` are stated (the lowest-index device, with the
/// sharing the host allows it). Stated values are kept.
///
/// Discrete GPU design §7: on a host whose GPUs are device domains the store
/// resolves an undeclared deployment once per GPU first
/// ([`crate::instances::device_choices`]), so this default is the host's own
/// (first) resolution.
pub fn for_host(deployment: &Value, host: &Value) -> Result<Value, ConfigError> {
    let mut result = deployment.clone();
    let Some(object) = result.as_object_mut() else {
        return Ok(result);
    };
    if let Some(requested) = object.get("runtime_profile").and_then(Value::as_str) {
        if let Some(name) = profile_on_host(requested, host) {
            if name != requested {
                object.insert("runtime_profile".into(), json!(name));
            }
            if !object.contains_key("runtime_profile_revision") {
                if let Some(revision) = host["runtime_profiles"][&name]["revision"].as_u64() {
                    object.insert("runtime_profile_revision".into(), json!(revision));
                }
            }
        }
    }
    // The short form names no domain, so it takes the default device as a
    // document without resources does.
    let names_domains = object
        .get("resources")
        .is_some_and(|r| !crate::short_resources::is_short(r));
    if !object.contains_key("devices") && !names_domains {
        if let Some(claim) = default_device(host) {
            object.insert("devices".into(), json!([claim]));
        }
    }
    fill_device_sharing(&mut result, host);
    crate::short_resources::expand_for_host(&mut result, host)?;
    Ok(result)
}

/// ADR 0019 (final review I9): the short pin form `devices: [{id: gpu1}]`
/// takes the sharing `host` states for that device, in the deployment's
/// claims and in any `resources` phase that names it. A stated sharing is
/// kept. Applied wherever a document meets its host, before anything reads
/// its claims.
pub fn fill_device_sharing(deployment: &mut Value, host: &Value) {
    let Some(object) = deployment.as_object_mut() else {
        return;
    };
    let sharing_of = |id: &str| device_sharing(host, id);
    if let Some(claims) = object.get_mut("devices").and_then(Value::as_array_mut) {
        fill_sharing(claims, &sharing_of);
    }
    if let Some(phases) = object.get_mut("resources").and_then(Value::as_object_mut) {
        for phase in phases.values_mut() {
            if let Some(claims) = phase.get_mut("devices").and_then(Value::as_array_mut) {
                fill_sharing(claims, &sharing_of);
            }
        }
    }
}

/// The sharing the host states for device `id` (its own, else the policy's
/// `device_sharing`, else exclusive), or `None` for a device it does not have.
fn device_sharing(host: &Value, id: &str) -> Option<String> {
    let policy = &host["resource_policy"];
    let device = policy["devices"].get(id)?;
    Some(
        device["sharing"]
            .as_str()
            .or_else(|| policy["device_sharing"].as_str())
            .unwrap_or("exclusive")
            .to_owned(),
    )
}

/// Give every named claim without `sharing` the host's sharing for it. A
/// claim naming a device the host does not have is left for resolution to
/// refuse by name.
fn fill_sharing(claims: &mut [Value], sharing_of: &dyn Fn(&str) -> Option<String>) {
    for claim in claims {
        let Some(object) = claim.as_object_mut() else {
            continue;
        };
        if object.contains_key("sharing") {
            continue;
        }
        let Some(sharing) = object
            .get("id")
            .and_then(Value::as_str)
            .and_then(sharing_of)
        else {
            continue;
        };
        object.insert("sharing".into(), json!(sharing));
    }
}

/// The device an undeclared deployment takes: the lowest driver index
/// (`gpuN`), then the lowest id, with the sharing the host states for it.
fn default_device(host: &Value) -> Option<Value> {
    let policy = &host["resource_policy"];
    let default_sharing = policy["device_sharing"].as_str().unwrap_or("exclusive");
    let devices: &Map<String, Value> = policy["devices"].as_object()?;
    let index = |id: &str| -> u32 {
        id.strip_prefix("gpu")
            .and_then(|n| n.parse().ok())
            .unwrap_or(u32::MAX)
    };
    let (id, device) = devices
        .iter()
        .min_by(|(a, _), (b, _)| index(a).cmp(&index(b)).then_with(|| a.cmp(b)))?;
    let sharing = device["sharing"].as_str().unwrap_or(default_sharing);
    Some(json!({"id": id, "sharing": sharing}))
}

/// The residency a deployment gets when it states none (owner decision
/// 2026-09-25, discrete GPU design §5, ADR 0010, ADR 0012):
///
/// - `restart_only` when the profile opted out of deep parking;
/// - on a discrete host (`discrete` is `(weights, system parked limit)`),
///   `host_backed` when the weights' host-RAM copy plus the engine's host
///   overhead fits what the system domain holds parked, else `deep`. Unknown
///   weights park `deep`: no copy can be sized;
/// - `deep` on a unified host, where a copy in host RAM frees nothing.
pub fn default_residency(
    deep_park: bool,
    discrete: Option<(Option<i64>, i64)>,
) -> crate::effective::Residency {
    use crate::effective::{Residency, ENGINE_HOST_OVERHEAD_PLACEHOLDER_BYTES as OVERHEAD};
    match (deep_park, discrete) {
        (false, _) => Residency::RestartOnly,
        (true, Some((Some(weights), parked_limit)))
            if crate::effective::host_backed_copy_bytes(weights).saturating_add(OVERHEAD)
                <= parked_limit =>
        {
            Residency::HostBacked
        }
        (true, _) => Residency::Deep,
    }
}

/// The KV cache a deployment gets when it states no memory at all (discrete
/// GPU design §3, the standalone template's rule): `min(4 GiB, managed / 4)`
/// of the memory domain it runs in. The memory request is then derived from
/// the checkpoint's weights (ADR 0014 §5), sized for the card on a discrete
/// host.
pub fn default_kv_cache(managed_limit: i64) -> i64 {
    const GIB: i64 = 1 << 30;
    (4 * GIB).min(managed_limit / 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn expanded(document: Value) -> Result<Value, ConfigError> {
        let mut document = document;
        expand(&mut document)?;
        Ok(document)
    }

    // T14 (owner decision 2026-09-25): three fields are a deployment.
    #[test]
    fn a_minimal_document_is_completed() {
        let document =
            expanded(json!({"name": "m", "engine": "vllm", "model": "Qwen3-4B"})).unwrap();
        assert_eq!(
            document,
            json!({
                "schema_version": 1, "kind": "deployment", "name": "m",
                "runtime_profile": "vllm", "routes": ["m"],
                "recipe": "standard", "recovery": "reconcile",
                "model": {"path": "Qwen3-4B", "content_fingerprint": "measured", "revision": "1"},
            })
        );
    }

    // T14 T03: stated fields are kept exactly, and expansion is idempotent.
    #[test]
    fn stated_fields_are_kept() {
        let full = json!({
            "schema_version": 1, "kind": "deployment", "name": "m",
            "runtime_profile": "p", "runtime_profile_revision": 3, "routes": ["r"],
            "recipe": "standalone", "recovery": "reconcile", "residency": "deep",
            "model": {"path": "/x", "content_fingerprint": "sha256:x", "revision": "r1"},
            "devices": [{"id": "gpu0", "sharing": "shared"}],
        });
        assert_eq!(expanded(full.clone()).unwrap(), full);
        let once = expanded(json!({"name": "m", "engine": "vllm", "model": "a"})).unwrap();
        assert_eq!(expanded(once.clone()).unwrap(), once);
        // `engine` beside the long form must agree with it.
        let both = json!({"name": "m", "engine": "vllm", "runtime_profile": "vllm", "model": "a"});
        assert_eq!(expanded(both).unwrap()["runtime_profile"], "vllm");
        let error =
            expanded(json!({"name": "m", "engine": "vllm", "runtime_profile": "x", "model": "a"}))
                .unwrap_err();
        assert_eq!(error.path, "engine");
        // Another kind is left for the kind check.
        let host = json!({"kind": "host", "name": "h"});
        assert_eq!(expanded(host.clone()).unwrap(), host);
    }

    // T14 (ADR 0008): the Hugging Face shorthand is a pinned source; an
    // unpinned one is refused with the way to pin it.
    #[test]
    fn the_hugging_face_shorthand_is_a_pinned_source() {
        let document = expanded(json!({
            "name": "m", "engine": "vllm", "model": {"hf": format!("Qwen/Qwen3-4B@{SHA}")}
        }))
        .unwrap();
        assert_eq!(
            document["model"],
            json!({
                "source": {"type": "huggingface", "repo": "Qwen/Qwen3-4B", "revision": SHA},
                "content_fingerprint": "measured", "revision": "1",
            })
        );
        for unpinned in ["Qwen/Qwen3-4B", "Qwen/Qwen3-4B@main"] {
            let error = expanded(json!({"name": "m", "model": {"hf": unpinned}})).unwrap_err();
            assert_eq!(error.path, "model.hf");
            assert!(
                error.detail.contains("capyctl deploy model --file"),
                "{error}"
            );
        }
        assert_eq!(
            unpinned_hf(&json!({"model": {"hf": "Qwen/Qwen3-4B"}}), "/model"),
            Some(("Qwen/Qwen3-4B".into(), "main".into()))
        );
        assert_eq!(
            unpinned_hf(&json!({"model": {"hf": "Qwen/Qwen3-4B@v1"}}), "/model"),
            Some(("Qwen/Qwen3-4B".into(), "v1".into()))
        );
        assert_eq!(
            unpinned_hf(
                &json!({"model": {"hf": format!("Qwen/Qwen3-4B@{SHA}")}}),
                "/model"
            ),
            None
        );
        let mut pinned = json!({"model": {"hf": "Qwen/Qwen3-4B"}});
        pin_hf(&mut pinned, "/model", "Qwen/Qwen3-4B", SHA);
        assert_eq!(unpinned_hf(&pinned, "/model"), None);
        let error =
            expanded(json!({"name": "m", "model": {"hf": format!("a/b@{SHA}"), "path": "x"}}))
                .unwrap_err();
        assert_eq!(error.path, "model.hf");
        let error =
            expanded(json!({"name": "m", "model": {"hf": format!("../b@{SHA}")}})).unwrap_err();
        assert_eq!(error.path, "model.hf");
    }

    // T14 (ADR 0008 amendment 2026-10-08): a drafter takes the model's
    // shorthands, a path or a pinned `hf:` reference, which the CLI pins
    // where it pins the model's own; `model.draft` is refused under its name.
    #[test]
    fn a_drafter_takes_the_models_shorthands() {
        let document = expanded(json!({
            "name": "m", "engine": "sglang", "model": {"path": "m", "draft": "drafts/d"}
        }))
        .unwrap();
        assert_eq!(
            document["model"]["draft"],
            json!({"type": "local", "path": "drafts/d"})
        );
        let document = expanded(json!({
            "name": "m", "model": {"path": "m", "draft": {"hf": format!("acme/draft-1b@{SHA}")}}
        }))
        .unwrap();
        assert_eq!(
            document["model"]["draft"],
            json!({"type": "huggingface", "repo": "acme/draft-1b", "revision": SHA})
        );
        // A written source is kept as written.
        let written = json!({"http": {"url": "https://d.example/d", "sha256": "a".repeat(64)}});
        let document =
            expanded(json!({"name": "m", "model": {"path": "m", "draft": written.clone()}}))
                .unwrap();
        assert_eq!(document["model"]["draft"], written);
        for (draft, path) in [
            (json!({"hf": "acme/draft-1b@main"}), "model.draft.hf"),
            (
                json!({"hf": "acme/draft-1b", "repo": "x"}),
                "model.draft.hf",
            ),
            (json!("~/drafts/d"), "model.draft"),
            (json!(""), "model.draft"),
        ] {
            let error =
                expanded(json!({"name": "m", "model": {"path": "m", "draft": draft}})).unwrap_err();
            assert_eq!(error.path, path, "{error}");
        }
        let unpinned = json!({"model": {"hf": format!("a/b@{SHA}"), "draft": {"hf": "acme/d"}}});
        assert_eq!(unpinned_hf(&unpinned, "/model"), None);
        assert_eq!(
            unpinned_hf(&unpinned, "/model/draft"),
            Some(("acme/d".into(), "main".into()))
        );
        let mut pinned = unpinned.clone();
        pin_hf(&mut pinned, "/model/draft", "acme/d", SHA);
        assert_eq!(
            pinned["model"]["draft"]["hf"],
            json!(format!("acme/d@{SHA}"))
        );
        assert_eq!(HF_SHORTHANDS, ["/model", "/model/draft"]);
        // The CLI expands a home-relative drafter path as it does the model's.
        let mut document = json!({"model": {"path": "~/m", "draft": "~/d"}});
        expand_home(&mut document, Some(std::path::Path::new("/home/u")));
        assert_eq!(
            document["model"],
            json!({"path": "/home/u/m", "draft": "/home/u/d"})
        );
    }

    // T14 (ADR 0018 §5): an engine family names the host's one profile of
    // that family; a profile name wins; several of the family is no match.
    #[test]
    fn an_engine_family_names_the_hosts_profile() {
        let host = json!({"runtime_profiles": {
            "local": {"engine": "vllm", "revision": 2},
            "sgl-a": {"engine": "sglang", "revision": 1},
            "sgl-b": {"engine": "sglang", "revision": 1},
        }});
        assert_eq!(profile_on_host("vllm", &host).as_deref(), Some("local"));
        assert_eq!(profile_on_host("local", &host).as_deref(), Some("local"));
        assert_eq!(profile_on_host("sglang", &host), None);
        assert_eq!(profile_on_host("other", &host), None);
        let completed = for_host(&json!({"runtime_profile": "vllm"}), &host).unwrap();
        assert_eq!(completed["runtime_profile"], "local");
        assert_eq!(completed["runtime_profile_revision"], 2);
        // A stated revision is kept (a mismatch is refused by resolution).
        let stated = for_host(
            &json!({"runtime_profile": "vllm", "runtime_profile_revision": 1}),
            &host,
        )
        .unwrap();
        assert_eq!(stated["runtime_profile_revision"], 1);
    }

    // T14 T27: an undeclared deployment takes the lowest-index GPU with the
    // sharing the host states; stated devices and explicit resources are kept.
    #[test]
    fn an_undeclared_deployment_takes_the_first_gpu() {
        let host = json!({"resource_policy": {
            "device_sharing": "exclusive",
            "devices": {"gpu10": {"domain": "d"}, "gpu2": {"domain": "d", "sharing": "shared"}},
        }});
        assert_eq!(
            for_host(&json!({}), &host).unwrap()["devices"],
            json!([{"id": "gpu2", "sharing": "shared"}])
        );
        let stated = json!({"devices": []});
        assert_eq!(for_host(&stated, &host).unwrap(), stated);
        let resources = json!({"resources": {}});
        assert_eq!(for_host(&resources, &host).unwrap(), resources);
    }

    // T14 T21 (ADR 0012, discrete GPU design §5): residency by host type.
    #[test]
    fn residency_follows_the_host_type() {
        use crate::effective::{Residency, ENGINE_HOST_OVERHEAD_PLACEHOLDER_BYTES as OVERHEAD};
        assert_eq!(default_residency(true, None), Residency::Deep);
        assert_eq!(default_residency(false, None), Residency::RestartOnly);
        assert_eq!(
            default_residency(false, Some((Some(1), 1 << 40))),
            Residency::RestartOnly
        );
        assert_eq!(
            default_residency(true, Some((Some(8 << 30), (12 << 30) + OVERHEAD))),
            Residency::HostBacked
        );
        assert_eq!(
            default_residency(true, Some((Some(8 << 30), (12 << 30) + OVERHEAD - 1))),
            Residency::Deep
        );
        assert_eq!(
            default_residency(true, Some((None, 1 << 40))),
            Residency::Deep
        );
        assert_eq!(default_kv_cache(64 << 30), 4 << 30);
        assert_eq!(default_kv_cache(8 << 30), 2 << 30);
    }
}
