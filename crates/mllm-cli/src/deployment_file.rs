//! The deployment file the CLI reads (`mllm deploy model --file`,
//! `mllm validate config`).
//!
//! Owner decision 2026-09-25 (ADR 0014 amendment "minimal deployment file"):
//! `name`, `engine` and `model` are a deployment, and mllm completes the rest
//! (`mllm_config::deployment_defaults`, inside the strict parse). Two things
//! only the machine the file is written on can do happen here, before the
//! document leaves it:
//!
//! - a model path starting with `~/` is expanded against this user's home
//!   directory, as a shell would;
//! - `deploy model` pins a `hf: owner/repo[@branch-or-tag]` shorthand to the
//!   commit it names now, asking the Hugging Face API at `HF_ENDPOINT` (the
//!   Hugging Face variable, default `https://huggingface.co`), so the server
//!   only ever stores a pinned source (ADR 0008). `validate config` never
//!   contacts the network, so it refuses an unpinned reference with the way to
//!   pin it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use mllm_config::deployment_defaults::{expand_home, pin_hf, unpinned_hf};
use mllm_config::model_source::{is_commit_sha, ModelSource, DEFAULT_HUGGINGFACE_ENDPOINT};
use mllm_config::ConfigKind;
use serde_json::Value;

use crate::output::StructuredError;

/// The Hugging Face endpoint variable (the one the Hugging Face tools read).
pub const HF_ENDPOINT_ENV: &str = "HF_ENDPOINT";
/// Bound on a deployment file read.
const MAX_BYTES: usize = 1024 * 1024;

fn invalid(message: impl Into<String>) -> StructuredError {
    StructuredError {
        code: "invalid_config",
        message: message.into(),
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Read a deployment file, bounded.
pub fn read_text(file: &Path) -> Result<String, StructuredError> {
    use std::io::Read;
    let source = std::fs::File::open(file).map_err(|_| invalid("Cannot read deployment file"))?;
    let mut contents = String::new();
    source
        .take(MAX_BYTES as u64 + 1)
        .read_to_string(&mut contents)
        .map_err(|_| invalid("Cannot read deployment file"))?;
    if contents.len() > MAX_BYTES {
        return Err(invalid("Deployment file is too large"));
    }
    Ok(contents)
}

/// `text` with a `~/` model path expanded against this user's home, as JSON
/// (which is YAML); `text` itself when there is nothing to expand or it does
/// not parse (the strict parse then reports why).
pub fn with_home_expanded(text: &str) -> String {
    let Ok(mut document) = mllm_config::parse_document(text) else {
        return text.to_owned();
    };
    let before = document.clone();
    expand_home(&mut document, home().as_deref());
    if document == before {
        text.to_owned()
    } else {
        serde_json::to_string(&document).expect("a parsed document serializes")
    }
}

/// The deployment document `deploy model --file` sends: the file with a `~/`
/// model path expanded and a Hugging Face reference pinned, strictly parsed
/// and completed with the defaults.
pub async fn prepare(text: &str) -> Result<Value, StructuredError> {
    let mut document =
        mllm_config::parse_document(text).map_err(|error| invalid(error.to_string()))?;
    expand_home(&mut document, home().as_deref());
    if let Some((repo, reference)) = unpinned_hf(&document) {
        let commit = pin(&repo, &reference).await?;
        pin_hf(&mut document, &repo, &commit);
    }
    mllm_config::parse_strict_value(ConfigKind::Deployment, document)
        .map_err(|error| invalid(error.to_string()))
}

/// The endpoint the pin is asked of: `HF_ENDPOINT`, else Hugging Face. HTTPS
/// only, except a loopback `http://` origin (a local mirror or a test).
fn endpoint() -> Result<reqwest::Url, StructuredError> {
    let text = std::env::var(HF_ENDPOINT_ENV)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_HUGGINGFACE_ENDPOINT.to_owned());
    let url = reqwest::Url::parse(text.trim_end_matches('/'))
        .map_err(|_| invalid(format!("{HF_ENDPOINT_ENV} is not a URL: {text:?}")))?;
    let loopback =
        url.scheme() == "http" && matches!(url.host_str(), Some("127.0.0.1") | Some("localhost"));
    if url.scheme() != "https" && !loopback {
        return Err(invalid(format!(
            "{HF_ENDPOINT_ENV} must be an https:// URL: {text:?}"
        )));
    }
    Ok(url)
}

/// ADR 0008: the commit `reference` (a branch or tag) of `repo` names now.
async fn pin(repo: &str, reference: &str) -> Result<String, StructuredError> {
    // The repository shape is the one a pinned source accepts.
    ModelSource::HuggingFace {
        repo: repo.to_owned(),
        revision: "0".repeat(40),
        files: Vec::new(),
        token_ref: None,
    }
    .validate()
    .map_err(|error| invalid(format!("model.hf: {}", error.detail)))?;
    if reference.is_empty()
        || !reference
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(invalid(format!(
            "model.hf: `{reference}` is not a branch or tag name; write `{repo}@<40-character commit>`"
        )));
    }
    let base = endpoint()?;
    let url = format!(
        "{}/api/models/{repo}/revision/{reference}",
        base.as_str().trim_end_matches('/')
    );
    let unpinnable = |why: &str| {
        invalid(format!(
            "model.hf: cannot pin `{repo}@{reference}` to a commit ({why}); write \
             `{repo}@<40-character commit>` in the file"
        ))
    };
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| unpinnable("no HTTP client"))?;
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|_| unpinnable("Hugging Face cannot be reached"))?;
    if !response.status().is_success() {
        return Err(unpinnable(&format!(
            "Hugging Face answered {}; a private repository must be pinned by hand",
            response.status().as_u16()
        )));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|_| unpinnable("the answer is not JSON"))?;
    body["sha"]
        .as_str()
        .filter(|sha| is_commit_sha(sha))
        .map(str::to_owned)
        .ok_or_else(|| unpinnable("the answer names no commit"))
}
