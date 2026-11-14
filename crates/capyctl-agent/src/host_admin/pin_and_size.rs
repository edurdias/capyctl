//! ADR 0040 §9 (the amendment to ADR 0008): a role's deployment check pins
//! model sources and sizes their weights on the chosen host's terms. The
//! Hugging Face endpoint and token are this host's (`model_sources`, and
//! `CAPYCTL_HF_TOKEN`/`HF_TOKEN` in its environment), never a caller's shell.
//! A standalone role calls [`pin_and_size`] in-process; a server asks the
//! host to run it (`HostAdminRequest`'s `pin_and_size`).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use std::{collections::BTreeMap, future::Future, pin::Pin};

use capyctl_config::deployment_defaults::split_hf;
use capyctl_config::deployment_pin::{hub_endpoint, Hub, HOST_TOKEN_HINT};
use capyctl_config::model_source::{
    is_commit_sha, pattern_matches, secret_name, ModelSource, ModelSourcePolicy,
};
use capyctl_protocol::pb;

use crate::checkpoint::{is_weight, CheckpointVerifier};
use crate::sources::{read_secret, Environment, HF_TOKEN_VARIABLES};

/// ADR 0040 §9: resolved repository/reference pins and their reported sizes
/// survive debounced checks. Local paths are sized afresh; failures are not
/// cached. The mutex also coalesces concurrent checks of the same reference.
pub struct CachedSourcePinner {
    policy: ModelSourcePolicy,
    model_store: PathBuf,
    environment: Environment,
    secrets_dir: Option<PathBuf>,
    pins: tokio::sync::Mutex<BTreeMap<String, (Instant, pb::PinnedSource)>>,
}

impl CachedSourcePinner {
    pub fn new(policy: ModelSourcePolicy, model_store: PathBuf, environment: Environment) -> Self {
        Self {
            policy,
            model_store,
            environment,
            secrets_dir: None,
            pins: Default::default(),
        }
    }

    /// ADR 0008: explicit credentials resolve only in the host secret store.
    pub fn with_secrets_dir(mut self, secrets_dir: Option<PathBuf>) -> Self {
        self.secrets_dir = secrets_dir;
        self
    }
}

// ADR 0040 §9: debounce checks without keeping mutable references forever or
// allowing arbitrary repository names to grow host memory without bound.
const PIN_CACHE_TTL: Duration = Duration::from_secs(60);
const PIN_CACHE_CAPACITY: usize = 256;

fn cache_pin(
    pins: &mut BTreeMap<String, (Instant, pb::PinnedSource)>,
    key: String,
    answer: &pb::PinnedSource,
    now: Instant,
) {
    // A successful pin with a failed listing must be retried on the next check.
    if !answer.error.is_empty() || answer.weight_bytes.is_none() {
        return;
    }
    pins.retain(|_, (created, _)| now.duration_since(*created) < PIN_CACHE_TTL);
    if pins.len() >= PIN_CACHE_CAPACITY && !pins.contains_key(&key) {
        if let Some(oldest) = pins
            .iter()
            .min_by_key(|(_, (created, _))| *created)
            .map(|(key, _)| key.clone())
        {
            pins.remove(&oldest);
        }
    }
    pins.insert(key, (now, answer.clone()));
}

impl crate::host_admin_remote::SourcePinner for CachedSourcePinner {
    fn pin_and_size(
        &self,
        sources: Vec<String>,
    ) -> Pin<Box<dyn Future<Output = Vec<pb::PinnedSource>> + Send + '_>> {
        Box::pin(async move {
            let mut pins = self.pins.lock().await;
            pins.retain(|_, (created, _)| created.elapsed() < PIN_CACHE_TTL);
            let mut answers = Vec::with_capacity(sources.len());
            for source in sources {
                let key = source.strip_prefix(HF_PREFIX).map(|hf| {
                    let (repo, reference) = split_hf(hf);
                    format!("{repo}@{}", reference.unwrap_or("main"))
                });
                let key = key.or_else(|| {
                    matches!(
                        serde_json::from_str::<ModelSource>(&source),
                        Ok(ModelSource::HuggingFace { .. })
                    )
                    .then(|| source.clone())
                });
                if let Some((_, answer)) = key
                    .as_ref()
                    .and_then(|key| pins.get(key))
                    .filter(|(created, _)| created.elapsed() < PIN_CACHE_TTL)
                {
                    let mut answer = answer.clone();
                    answer.source = source;
                    answers.push(answer);
                    continue;
                }
                let mut result = pin_and_size_with_secrets(
                    &self.policy,
                    &self.model_store,
                    &self.environment,
                    self.secrets_dir.as_deref(),
                    vec![source.clone()],
                )
                .await;
                let answer = result.pop().expect("one source has one answer");
                if let Some(key) = key {
                    cache_pin(&mut pins, key, &answer, Instant::now());
                }
                answers.push(answer);
            }
            answers
        })
    }
}

/// The prefix of a Hugging Face source: `hf:<owner/repo>[@<branch, tag or
/// commit>]`, the reference `main` when none is written. Any other source is
/// a local checkpoint path, absolute or relative to the model store.
pub const HF_PREFIX: &str = "hf:";
/// The closed code a source that could not be pinned answers with, before
/// the reason (`pin_failed: <reason>`).
pub const PIN_FAILED: &str = "pin_failed";

/// ADR 0040 §9: each of `sources` pinned and sized on this host's terms, in
/// order. A Hugging Face source answers `<repo>@<commit>` and the sum of the
/// weight files' sizes the hub reports for that commit (the `is_weight`
/// rule); a local path answers itself and its weights' on-disk size. Every
/// size is `estimated` (sizes, not a measurement); `weight_bytes` is absent
/// when no size is known, and `error` (`pin_failed: …`) when the source could
/// not be pinned.
pub async fn pin_and_size(
    policy: &ModelSourcePolicy,
    model_store: &Path,
    environment: &Environment,
    sources: Vec<String>,
) -> Vec<pb::PinnedSource> {
    pin_and_size_with_secrets(policy, model_store, environment, None, sources).await
}

async fn pin_and_size_with_secrets(
    policy: &ModelSourcePolicy,
    model_store: &Path,
    environment: &Environment,
    secrets_dir: Option<&Path>,
    sources: Vec<String>,
) -> Vec<pb::PinnedSource> {
    let mut hub = None;
    let mut answers = Vec::with_capacity(sources.len());
    for source in sources {
        let structured = serde_json::from_str::<ModelSource>(&source).ok();
        let answer = if let Some(ModelSource::HuggingFace {
            repo,
            revision,
            files,
            token_ref,
        }) = structured
        {
            let selected_hub = match token_ref {
                Some(reference) => {
                    let path = secret_name(&reference)
                        .and_then(|name| secrets_dir.map(|dir| dir.join(name)));
                    match path.and_then(|path| read_secret(&path).ok()) {
                        Some(token) => hub_endpoint(
                            "model_sources.huggingface_endpoint",
                            policy.huggingface_endpoint(),
                        )
                        .map(|endpoint| Hub::new(endpoint, Some(token.0), HOST_TOKEN_HINT)),
                        None => Err("the host source token_ref cannot be read".to_owned()),
                    }
                }
                None => host_hub(policy, environment),
            };
            match selected_hub {
                Ok(hub) => pin_hf(&hub, &format!("{repo}@{revision}"), &files).await,
                Err(why) => Err(why),
            }
        } else {
            match source.strip_prefix(HF_PREFIX) {
                Some(shorthand) => {
                    if hub.is_none() {
                        hub = Some(host_hub(policy, environment));
                    }
                    match hub.as_ref().expect("set above") {
                        Ok(hub) => pin_hf(hub, shorthand, &[]).await,
                        Err(why) => Err(why.clone()),
                    }
                }
                None => Ok(size_local(model_store, &source).await),
            }
        };
        answers.push(match answer {
            Ok((pinned, weight_bytes)) => pb::PinnedSource {
                source,
                pinned,
                weight_bytes,
                estimated: weight_bytes.is_some(),
                error: String::new(),
            },
            Err(why) => pb::PinnedSource {
                source,
                pinned: String::new(),
                weight_bytes: None,
                estimated: false,
                error: format!("{PIN_FAILED}: {why}"),
            },
        });
    }
    answers
}

/// The hub this host pins against: its `model_sources.huggingface_endpoint`
/// (else Hugging Face), asked with its default token (owner rule
/// 2026-09-25: `CAPYCTL_HF_TOKEN`, else `HF_TOKEN`, else the protected
/// `model_sources.huggingface_token_file`).
fn host_hub(policy: &ModelSourcePolicy, environment: &Environment) -> Result<Hub, String> {
    let endpoint = hub_endpoint(
        "model_sources.huggingface_endpoint",
        policy.huggingface_endpoint(),
    )?;
    let token = match HF_TOKEN_VARIABLES
        .iter()
        .find_map(|name| environment(name).filter(|value| !value.is_empty()))
    {
        Some(token) => Some(token),
        None => match &policy.huggingface_token_file {
            Some(file) => Some(
                read_secret(file)
                    .map_err(|_| {
                        "the host's model_sources.huggingface_token_file cannot be read".to_owned()
                    })?
                    .0,
            ),
            None => None,
        },
    };
    Ok(Hub::new(endpoint, token, HOST_TOKEN_HINT))
}

/// `repo@reference` pinned to its commit (a commit is kept as written), and
/// the weights the hub lists for that commit. A listing the hub refuses
/// leaves the size unknown, never the pin.
async fn pin_hf(
    hub: &Hub,
    shorthand: &str,
    patterns: &[String],
) -> Result<(String, Option<u64>), String> {
    let (repo, reference) = split_hf(shorthand);
    let reference = reference.unwrap_or("main");
    let commit = if is_commit_sha(reference) {
        reference.to_owned()
    } else {
        hub.pin("model.hf", repo, reference).await?
    };
    let weights = hub.files(repo, &commit).await.ok().and_then(|files| {
        let bytes = files
            .iter()
            .filter(|file| is_weight(&file.path))
            .filter(|file| {
                patterns.is_empty()
                    || patterns
                        .iter()
                        .any(|pattern| pattern_matches(pattern, &file.path))
            })
            .try_fold(0u64, |sum, file| sum.checked_add(file.size?))?;
        (bytes > 0).then_some(bytes)
    });
    Ok((format!("{repo}@{commit}"), weights))
}

/// `path` as written, and the on-disk size of its weight files, sized with
/// the confined walk a measurement uses (hashing nothing). A relative path
/// is under the model store.
async fn size_local(model_store: &Path, path: &str) -> (String, Option<u64>) {
    let store = model_store.to_path_buf();
    let checkpoint = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        store.join(path)
    };
    let size = tokio::task::spawn_blocking(move || {
        CheckpointVerifier::in_memory().size(&store, &checkpoint)
    })
    .await
    .ok()
    .and_then(Result::ok)
    .and_then(|size| u64::try_from(size.weights_bytes).ok())
    .filter(|bytes| *bytes > 0);
    (path.to_owned(), size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn no_environment() -> Environment {
        Arc::new(|_: &str| None)
    }

    // T16 / ADR 0008, ADR 0040 §9: structured sources retain host credential
    // selection; an unavailable explicit token never falls back to defaults.
    #[tokio::test]
    async fn structured_source_requires_its_explicit_host_secret() {
        let store = tempfile::tempdir().unwrap();
        let source = serde_json::to_string(&ModelSource::HuggingFace {
            repo: "owner/model".into(),
            revision: "a".repeat(40),
            files: vec!["selected/*.safetensors".into()],
            token_ref: Some("secret://missing".into()),
        })
        .unwrap();
        let answers = pin_and_size(
            &ModelSourcePolicy::default(),
            store.path(),
            &no_environment(),
            vec![source.clone()],
        )
        .await;
        assert_eq!(answers[0].source, source);
        assert!(answers[0].error.starts_with(PIN_FAILED));
        assert!(answers[0].weight_bytes.is_none());
    }

    // T16 / ADR 0040 §9: cache memory and mutable-reference freshness are bounded.
    #[test]
    fn pin_cache_expires_and_evicts_at_capacity() {
        let mut pins = BTreeMap::new();
        let now = Instant::now();
        let answer = pb::PinnedSource {
            weight_bytes: Some(1),
            ..Default::default()
        };
        cache_pin(&mut pins, "expired".into(), &answer, now - PIN_CACHE_TTL);
        for index in 0..PIN_CACHE_CAPACITY {
            cache_pin(&mut pins, index.to_string(), &answer, now);
        }
        assert!(!pins.contains_key("expired"));
        assert_eq!(pins.len(), PIN_CACHE_CAPACITY);
        cache_pin(&mut pins, "new".into(), &answer, now);
        assert_eq!(pins.len(), PIN_CACHE_CAPACITY);
        assert!(pins.contains_key("new"));
    }

    // T16 / ADR 0040 §9: transient listing failures never cache an unknown size.
    #[test]
    fn unknown_size_is_retried_instead_of_cached() {
        let mut pins = BTreeMap::new();
        let mut answer = pb::PinnedSource::default();
        cache_pin(&mut pins, "repo@main".into(), &answer, Instant::now());
        assert!(pins.is_empty());
        answer.weight_bytes = Some(42);
        cache_pin(&mut pins, "repo@main".into(), &answer, Instant::now());
        assert_eq!(pins["repo@main"].1.weight_bytes, Some(42));
    }

    // T16 / ADR 0040 §9: only local checkpoint weight files contribute size.
    #[tokio::test]
    async fn a_local_path_is_sized_from_its_weight_files() {
        let store = tempfile::tempdir().unwrap();
        let model = store.path().join("toy");
        std::fs::create_dir(&model).unwrap();
        std::fs::write(model.join("model.safetensors"), vec![0u8; 3000]).unwrap();
        std::fs::write(model.join("config.json"), b"{}").unwrap();
        let path = model.to_string_lossy().into_owned();
        let answers = pin_and_size(
            &ModelSourcePolicy::default(),
            store.path(),
            &no_environment(),
            vec![path.clone()],
        )
        .await;
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].pinned, path);
        assert_eq!(answers[0].weight_bytes, Some(3000));
        assert!(answers[0].estimated);
        assert!(answers[0].error.is_empty());
    }
}
