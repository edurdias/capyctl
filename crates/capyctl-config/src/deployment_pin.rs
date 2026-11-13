//! ADR 0008: a deployment document's Hugging Face references pinned to the
//! commit they name now, so the server only ever stores pinned sources.
//!
//! Shared by `capyctl deploy model --file`, which pins on the operator's
//! terms (its flag, environment and home directory), and by a role's
//! deployment check (ADR 0040 §9, the amendment to ADR 0008), which pins on
//! the chosen host's terms: the endpoint and token come from that host's
//! `model_sources`, never from a caller's shell. A role does not expand a
//! `~/` path, since it cannot know whose home it means; the strict parse
//! refuses one.

use std::future::Future;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::deployment_defaults::{expand_home, pin_hf, unpinned_hf, HF_SHORTHANDS};
use crate::model_source::{is_commit_sha, ModelSource};
use crate::{ConfigError, ConfigKind};

/// What a pin refusal tells the operator of `capyctl deploy model --file`
/// to provide for a private repository.
pub const CLI_TOKEN_HINT: &str = "a private repository needs CAPYCTL_HF_TOKEN or HF_TOKEN";
/// ADR 0040 §9: the same, when the role pins on the chosen host's terms.
pub const HOST_TOKEN_HINT: &str = "a private repository needs the host's CAPYCTL_HF_TOKEN or \
                                   model_sources.huggingface_token_file";
/// Bound on a listing the hub answers with.
const MAX_LISTING_BYTES: usize = 16 * 1024 * 1024;

/// Why a document could not be prepared: the strict parse refused it, or a
/// reference could not be pinned (the message names the shorthand).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepareError {
    Config(ConfigError),
    Pin(String),
}

impl std::fmt::Display for PrepareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(error) => error.fmt(f),
            Self::Pin(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for PrepareError {}

/// A file of a repository at a commit, with the size the hub reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubFile {
    pub path: String,
    pub size: u64,
}

/// The Hugging Face API at `endpoint`, asked with `token` when there is one.
#[derive(Clone)]
pub struct Hub {
    endpoint: reqwest::Url,
    token: Option<String>,
    token_hint: &'static str,
    client: Option<reqwest::Client>,
}

impl std::fmt::Debug for Hub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The token is a secret and is never formatted.
        f.debug_struct("Hub")
            .field("endpoint", &self.endpoint.as_str())
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// The Hugging Face endpoint `text` (named `name` in a refusal) as a URL:
/// HTTPS only, except a loopback `http://` origin (a local mirror or a test).
pub fn hub_endpoint(name: &str, text: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(text.trim_end_matches('/'))
        .map_err(|_| format!("{name} is not a URL: {text:?}"))?;
    let loopback =
        url.scheme() == "http" && matches!(url.host_str(), Some("127.0.0.1") | Some("localhost"));
    if url.scheme() != "https" && !loopback {
        return Err(format!("{name} must be an https:// URL: {text:?}"));
    }
    Ok(url)
}

impl Hub {
    /// `token_hint` ([`CLI_TOKEN_HINT`], [`HOST_TOKEN_HINT`]) is what a
    /// refusal for a private repository names.
    pub fn new(endpoint: reqwest::Url, token: Option<String>, token_hint: &'static str) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .ok();
        Self {
            endpoint,
            token: token
                .map(|token| token.trim().to_owned())
                .filter(|token| !token.is_empty()),
            token_hint,
            client,
        }
    }

    fn url(&self, repo: &str, revision: &str) -> String {
        format!(
            "{}/api/models/{repo}/revision/{revision}",
            self.endpoint.as_str().trim_end_matches('/')
        )
    }

    async fn get(&self, url: &str) -> Result<reqwest::Response, String> {
        let client = self.client.as_ref().ok_or("no HTTP client")?;
        let mut request = client.get(url);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .await
            .map_err(|_| "Hugging Face cannot be reached".to_owned())?;
        if !response.status().is_success() {
            return Err(format!(
                "Hugging Face answered {}; {}, or a pinned commit",
                response.status().as_u16(),
                self.token_hint
            ));
        }
        Ok(response)
    }

    /// ADR 0008: the commit `reference` (a branch or tag) of `repo` names
    /// now. `field` names the shorthand (`model.hf`, `model.draft.hf`) in a
    /// refusal.
    pub async fn pin(&self, field: &str, repo: &str, reference: &str) -> Result<String, String> {
        // The repository shape is the one a pinned source accepts.
        ModelSource::HuggingFace {
            repo: repo.to_owned(),
            revision: "0".repeat(40),
            files: Vec::new(),
            token_ref: None,
        }
        .validate()
        .map_err(|error| format!("{field}: {}", error.detail))?;
        if reference.is_empty()
            || !reference
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        {
            return Err(format!(
                "{field}: `{reference}` is not a branch or tag name; write `{repo}@<40-character commit>`"
            ));
        }
        let unpinnable = |why: &str| {
            format!(
                "{field}: cannot pin `{repo}@{reference}` to a commit ({why}); write \
                 `{repo}@<40-character commit>` in the file"
            )
        };
        let response = self
            .get(&self.url(repo, reference))
            .await
            .map_err(|why| unpinnable(&why))?;
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

    /// ADR 0040 §9: the files of `repo` at `commit` with the sizes the hub
    /// reports (its listing with blob details), for sizing a checkpoint
    /// before it is measured. A listing that answers for another commit is
    /// refused, as the downloader refuses it.
    pub async fn files(&self, repo: &str, commit: &str) -> Result<Vec<HubFile>, String> {
        #[derive(Deserialize)]
        struct Lfs {
            size: u64,
        }
        #[derive(Deserialize)]
        struct Sibling {
            rfilename: String,
            #[serde(default)]
            size: Option<u64>,
            #[serde(default)]
            lfs: Option<Lfs>,
        }
        #[derive(Deserialize)]
        struct Listing {
            sha: String,
            siblings: Vec<Sibling>,
        }
        let mut response = self
            .get(&format!("{}?blobs=true", self.url(repo, commit)))
            .await?;
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| "Hugging Face cannot be reached".to_owned())?
        {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_LISTING_BYTES {
                return Err("the listing is too large".to_owned());
            }
        }
        let listing: Listing =
            serde_json::from_slice(&body).map_err(|_| "the listing is not valid".to_owned())?;
        if listing.sha != commit {
            return Err("the listing names another commit".to_owned());
        }
        listing
            .siblings
            .into_iter()
            .map(|sibling| {
                let size = sibling
                    .lfs
                    .map(|lfs| lfs.size)
                    .or(sibling.size)
                    .ok_or_else(|| format!("the listing gives no size for {}", sibling.rfilename))?;
                Ok(HubFile {
                    path: sibling.rfilename,
                    size,
                })
            })
            .collect()
    }
}

/// Pin each Hugging Face shorthand of a raw deployment document (the model's
/// and, ADR 0008 amendment 2026-10-08, its drafter's) with `pin`, called
/// with the shorthand's field name, the repository and the reference.
pub async fn pin_references<F, Fut>(document: &mut Value, mut pin: F) -> Result<(), String>
where
    F: FnMut(String, String, String) -> Fut,
    Fut: Future<Output = Result<String, String>>,
{
    for at in HF_SHORTHANDS {
        if let Some((repo, reference)) = unpinned_hf(document, at) {
            let field = format!("{}.hf", at.trim_start_matches('/').replace('/', "."));
            let commit = pin(field, repo.clone(), reference).await?;
            pin_hf(document, at, &repo, &commit);
        }
    }
    Ok(())
}

/// The deployment document `capyctl deploy model --file` sends: `text` with
/// a `~/` model path expanded against `home` (the operator's) and each
/// Hugging Face reference pinned through `hub`, strictly parsed and
/// completed with the defaults.
pub async fn prepare(text: &str, home: Option<&Path>, hub: &Hub) -> Result<Value, PrepareError> {
    let mut document = crate::parse_document(text).map_err(PrepareError::Config)?;
    expand_home(&mut document, home);
    pin_references(&mut document, |field, repo, reference| async move {
        hub.pin(&field, &repo, &reference).await
    })
    .await
    .map_err(PrepareError::Pin)?;
    crate::parse_strict_value(ConfigKind::Deployment, document).map_err(PrepareError::Config)
}
