//! ADR 0040 §9 (the amendment to ADR 0008): a role's deployment check pins
//! model sources and sizes their weights on the chosen host's terms. The
//! Hugging Face endpoint and token are this host's (`model_sources`, and
//! `CAPYCTL_HF_TOKEN`/`HF_TOKEN` in its environment), never a caller's shell.
//! A standalone role calls [`pin_and_size`] in-process; a server asks the
//! host to run it (`HostAdminRequest`'s `pin_and_size`).

use std::path::{Path, PathBuf};

use capyctl_config::deployment_defaults::split_hf;
use capyctl_config::deployment_pin::{hub_endpoint, Hub, HOST_TOKEN_HINT};
use capyctl_config::model_source::{is_commit_sha, ModelSourcePolicy};
use capyctl_protocol::pb;

use crate::checkpoint::{is_weight, CheckpointVerifier};
use crate::sources::{read_secret, Environment, HF_TOKEN_VARIABLES};

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
    let mut hub = None;
    let mut answers = Vec::with_capacity(sources.len());
    for source in sources {
        let answer = match source.strip_prefix(HF_PREFIX) {
            Some(shorthand) => {
                if hub.is_none() {
                    hub = Some(host_hub(policy, environment));
                }
                match hub.as_ref().expect("set above") {
                    Ok(hub) => pin_hf(hub, shorthand).await,
                    Err(why) => Err(why.clone()),
                }
            }
            None => Ok(size_local(model_store, &source).await),
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
async fn pin_hf(hub: &Hub, shorthand: &str) -> Result<(String, Option<u64>), String> {
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
            .try_fold(0u64, |sum, file| sum.checked_add(file.size))?;
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

    // ADR 0040 §9: a local checkpoint is sized from the weight files on
    // disk, and only those, without a pin.
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
