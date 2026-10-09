//! The deployment file the CLI reads (`capyctl deploy model --file`,
//! `capyctl validate config`).
//!
//! Owner decision 2026-09-25 (ADR 0014 amendment "minimal deployment file"):
//! `name`, `engine` and `model` are a deployment, and capyctl completes the rest
//! (`capyctl_config::deployment_defaults`, inside the strict parse). Two things
//! only the machine the file is written on can do happen here, before the
//! document leaves it:
//!
//! - a model path starting with `~/` is expanded against this user's home
//!   directory, as a shell would;
//! - `deploy model` pins a `hf: owner/repo[@branch-or-tag]` shorthand to the
//!   commit it names now, asking the Hugging Face API at the endpoint named
//!   by `--hf-endpoint`, else `CAPYCTL_HF_ENDPOINT`, else the Hugging Face
//!   tools' `HF_ENDPOINT`, else the role document's
//!   `model_sources.huggingface_endpoint` (owner rule 2026-09-25: every
//!   setting three ways), else `https://huggingface.co`, so the server only
//!   ever stores a pinned source (ADR 0008). `validate config` never
//!   contacts the network, so it refuses an unpinned reference with the way to
//!   pin it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use capyctl_config::deployment_defaults::{expand_home, pin_hf, unpinned_hf, HF_SHORTHANDS};
use capyctl_config::model_source::{is_commit_sha, ModelSource, DEFAULT_HUGGINGFACE_ENDPOINT};
use capyctl_config::ConfigKind;
use serde_json::Value;

use crate::output::StructuredError;

/// The Hugging Face endpoint variable (the one the Hugging Face tools read).
pub const HF_ENDPOINT_ENV: &str = capyctl_config::model_settings::HF_TOOLS_ENDPOINT_ENV;
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
    let Ok(mut document) = capyctl_config::parse_document(text) else {
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
/// model path expanded and each Hugging Face reference (the model's and,
/// ADR 0008 amendment 2026-10-08, its drafter's) pinned, strictly parsed and
/// completed with the defaults.
pub async fn prepare(text: &str, endpoint: &reqwest::Url) -> Result<Value, StructuredError> {
    let mut document =
        capyctl_config::parse_document(text).map_err(|error| invalid(error.to_string()))?;
    expand_home(&mut document, home().as_deref());
    for at in HF_SHORTHANDS {
        if let Some((repo, reference)) = unpinned_hf(&document, at) {
            let field = format!("{}.hf", at.trim_start_matches('/').replace('/', "."));
            let commit = pin(endpoint, &field, &repo, &reference).await?;
            pin_hf(&mut document, at, &repo, &commit);
        }
    }
    capyctl_config::parse_strict_value(ConfigKind::Deployment, document)
        .map_err(|error| invalid(error.to_string()))
}

/// The endpoint the pin is asked of (owner rule 2026-09-25): `flag`
/// (`--hf-endpoint`), else `CAPYCTL_HF_ENDPOINT`, else `HF_ENDPOINT`, else the
/// role document's `model_sources.huggingface_endpoint`, else Hugging Face.
/// The role document is the host or standalone document `config` names
/// (`--config`, `CAPYCTL_CONFIG`), else the implicit standalone document under
/// the state root, when there is one. HTTPS only, except a loopback `http://`
/// origin (a local mirror or a test).
pub fn pin_endpoint(
    flag: Option<&str>,
    state_dir: &Path,
    config: Option<&Path>,
) -> Result<reqwest::Url, StructuredError> {
    pin_endpoint_with(flag, &|key| std::env::var(key).ok(), state_dir, config)
}

/// [`pin_endpoint`], reading the environment through `get`.
fn pin_endpoint_with(
    flag: Option<&str>,
    get: &dyn Fn(&str) -> Option<String>,
    state_dir: &Path,
    config: Option<&Path>,
) -> Result<reqwest::Url, StructuredError> {
    let env = |key: &str| get(key).filter(|value| !value.is_empty());
    let config = config
        .map(Path::to_path_buf)
        .or_else(|| env("CAPYCTL_CONFIG").map(PathBuf::from));
    let (name, text) = match (
        flag,
        env(capyctl_config::model_settings::HF_ENDPOINT_ENV),
        env(HF_ENDPOINT_ENV),
    ) {
        (Some(flag), _, _) => ("--hf-endpoint", flag.to_owned()),
        (None, Some(value), _) => (capyctl_config::model_settings::HF_ENDPOINT_ENV, value),
        (None, None, Some(value)) => (HF_ENDPOINT_ENV, value),
        (None, None, None) => match document_endpoint(state_dir, config.as_deref()) {
            Some(value) => ("model_sources.huggingface_endpoint", value),
            None => ("endpoint", DEFAULT_HUGGINGFACE_ENDPOINT.to_owned()),
        },
    };
    let url = reqwest::Url::parse(text.trim_end_matches('/'))
        .map_err(|_| invalid(format!("{name} is not a URL: {text:?}")))?;
    let loopback =
        url.scheme() == "http" && matches!(url.host_str(), Some("127.0.0.1") | Some("localhost"));
    if url.scheme() != "https" && !loopback {
        return Err(invalid(format!("{name} must be an https:// URL: {text:?}")));
    }
    Ok(url)
}

/// `model_sources.huggingface_endpoint` of the host document `config` names
/// (or its standalone `host:` block), else of the implicit standalone
/// document; `None` when there is no such document or it names none. Best
/// effort: an unreadable document states nothing here (the role refuses it).
fn document_endpoint(state_dir: &Path, config: Option<&Path>) -> Option<String> {
    let path = config
        .map(Path::to_path_buf)
        .unwrap_or_else(|| state_dir.join("config").join("standalone.yaml"));
    let text = std::fs::read_to_string(path).ok()?;
    let document = capyctl_config::parse_document(&text).ok()?;
    let sources = match document["kind"].as_str() {
        Some("standalone") => &document["host"]["model_sources"],
        _ => &document["model_sources"],
    };
    sources["huggingface_endpoint"].as_str().map(str::to_owned)
}

/// ADR 0008: the commit `reference` (a branch or tag) of `repo` names now.
/// `field` names the shorthand (`model.hf`, `model.draft.hf`) in a refusal.
async fn pin(
    base: &reqwest::Url,
    field: &str,
    repo: &str,
    reference: &str,
) -> Result<String, StructuredError> {
    // The repository shape is the one a pinned source accepts.
    ModelSource::HuggingFace {
        repo: repo.to_owned(),
        revision: "0".repeat(40),
        files: Vec::new(),
        token_ref: None,
    }
    .validate()
    .map_err(|error| invalid(format!("{field}: {}", error.detail)))?;
    if reference.is_empty()
        || !reference
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err(invalid(format!(
            "{field}: `{reference}` is not a branch or tag name; write `{repo}@<40-character commit>`"
        )));
    }
    let url = format!(
        "{}/api/models/{repo}/revision/{reference}",
        base.as_str().trim_end_matches('/')
    );
    let unpinnable = |why: &str| {
        invalid(format!(
            "{field}: cannot pin `{repo}@{reference}` to a commit ({why}); write \
             `{repo}@<40-character commit>` in the file"
        ))
    };
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| unpinnable("no HTTP client"))?;
    // Owner rule 2026-09-25: a private repository is pinned with the token
    // in `CAPYCTL_HF_TOKEN` (else `HF_TOKEN`); a secret is never a flag.
    let mut request = client.get(&url);
    if let Some(token) = capyctl_agent::sources::HF_TOKEN_VARIABLES
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
    {
        request = request.bearer_auth(token.trim());
    }
    let response = request
        .send()
        .await
        .map_err(|_| unpinnable("Hugging Face cannot be reached"))?;
    if !response.status().is_success() {
        return Err(unpinnable(&format!(
            "Hugging Face answered {}; a private repository needs CAPYCTL_HF_TOKEN or HF_TOKEN, or a pinned commit",
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

#[cfg(test)]
mod tests {
    use super::*;

    // T14 (ADR 0008 amendment 2026-10-08): a drafter's `hf:` reference is
    // pinned to the commit it names now, beside the model's own, so the
    // server only ever stores pinned sources.
    #[tokio::test]
    async fn a_drafters_hugging_face_reference_is_pinned_with_the_models() {
        const MODEL: &str = "0123456789abcdef0123456789abcdef01234567";
        const DRAFT: &str = "89abcdef0123456789abcdef0123456789abcdef";
        let app = axum::Router::new()
            .route(
                "/api/models/org/model/revision/main",
                axum::routing::get(|| async { axum::Json(serde_json::json!({"sha": MODEL})) }),
            )
            .route(
                "/api/models/acme/draft/revision/v2",
                axum::routing::get(|| async { axum::Json(serde_json::json!({"sha": DRAFT})) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let endpoint = reqwest::Url::parse(&origin).unwrap();
        let document = prepare(
            "name: m\nengine: sglang\nmodel: {hf: org/model, draft: {hf: acme/draft@v2}}\n",
            &endpoint,
        )
        .await
        .unwrap();
        assert_eq!(
            document["model"]["source"],
            serde_json::json!({"type": "huggingface", "repo": "org/model", "revision": MODEL})
        );
        assert_eq!(
            document["model"]["draft"],
            serde_json::json!({"type": "huggingface", "repo": "acme/draft", "revision": DRAFT})
        );
        // A drafter reference the hub does not know is refused under its name.
        let error = prepare(
            &format!("name: m\nengine: sglang\nmodel: {{hf: org/model@{MODEL}, draft: {{hf: acme/x}}}}\n"),
            &endpoint,
        )
        .await
        .unwrap_err();
        assert!(
            error.message.starts_with("model.draft.hf: "),
            "{}",
            error.message
        );
    }

    // T03 (owner rule 2026-09-25: every setting three ways): the endpoint a
    // `hf:` reference is pinned against follows `--hf-endpoint` >
    // `CAPYCTL_HF_ENDPOINT` > `HF_ENDPOINT` > the role document's
    // `model_sources.huggingface_endpoint` (a named host document, or the
    // standalone document's `host:` block) > Hugging Face.
    #[test]
    fn the_pin_endpoint_follows_flag_env_document_default() {
        let dir = tempfile::TempDir::new().unwrap();
        let host = dir.path().join("host.yaml");
        std::fs::write(
            &host,
            "kind: host\nmodel_sources:\n  huggingface_endpoint: https://host-doc.example\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::write(
            dir.path().join("config/standalone.yaml"),
            "kind: standalone\nhost:\n  model_sources:\n    huggingface_endpoint: https://implicit.example\n",
        )
        .unwrap();
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |key: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| v.to_string())
            }
        };
        let all: &[(&str, &str)] = &[
            ("CAPYCTL_HF_ENDPOINT", "https://capyctl-env.example"),
            ("HF_ENDPOINT", "https://tools-env.example"),
        ];
        let tools: &[(&str, &str)] = &[("HF_ENDPOINT", "https://tools-env.example")];
        let named: &[(&str, &str)] = &[("CAPYCTL_CONFIG", "/nowhere/host.yaml")];
        let seen = |flag: Option<&str>, get: &dyn Fn(&str) -> Option<String>, config| {
            pin_endpoint_with(flag, get, dir.path(), config)
                .unwrap()
                .as_str()
                .trim_end_matches('/')
                .to_owned()
        };
        assert_eq!(
            seen(Some("https://flag.example"), &env(all), Some(&host)),
            "https://flag.example"
        );
        assert_eq!(
            seen(None, &env(all), Some(&host)),
            "https://capyctl-env.example"
        );
        assert_eq!(
            seen(None, &env(tools), Some(&host)),
            "https://tools-env.example"
        );
        assert_eq!(
            seen(None, &env(&[]), Some(&host)),
            "https://host-doc.example"
        );
        assert_eq!(seen(None, &env(&[]), None), "https://implicit.example");
        // A named document that cannot be read states nothing.
        assert_eq!(seen(None, &env(named), None), "https://huggingface.co");
        // Not https and not loopback: refused, naming the source.
        let error = pin_endpoint_with(Some("http://mirror.example"), &env(&[]), dir.path(), None)
            .unwrap_err();
        assert!(error.message.contains("--hf-endpoint"), "{}", error.message);
    }
}
