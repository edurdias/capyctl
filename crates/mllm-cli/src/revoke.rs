//! `mllm revoke host <name|id>`.
//!
//! SPEC §4.1: certificates are revocable, and a revoked host must not continue
//! accepting new commands through an old connection. SPEC §13.3: revocation
//! closes control sessions and prevents new work; terminating existing
//! workloads follows explicit administrative policy. So this command revokes
//! the identity through the server's management API and nothing else: the
//! server closes the host's control session at once, refuses its reconnects
//! and every new command, closes dispatch to its engines and leaves it out of
//! placement. Its engines are not stopped, and their reservations and leases
//! stay accounted until an operator settles them with evidence. ADR 0016: the
//! host returns only through an explicit recovery (`invite host <name|id>
//! --recover`, then `join host --recover`), under its same host id with a new
//! certificate; the revoked certificate stays refused.
//!
//! SPEC §6.4: the command carries a request identity like every other
//! mutation. Revocation is absorbing, so a retry (with the same `--request-id`
//! or not) answers the same host with `newly_revoked: false`; the journal only
//! keeps one request identity from naming two different hosts.
use crate::client_journal::RequestJournal;
use crate::output::StructuredError;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::Duration;

fn error(code: &'static str, message: impl Into<String>) -> StructuredError {
    StructuredError {
        code,
        message: message.into(),
    }
}

fn unavailable() -> StructuredError {
    error(
        "management_unavailable",
        "Management request did not complete; list hosts before retrying with the same --request-id",
    )
}

/// Revoke `host` through the server context's management API.
pub async fn execute(
    host: &str,
    state_dir: &Path,
    config: Option<&Path>,
    request_id: Option<&str>,
) -> Result<Value, StructuredError> {
    let target = crate::local_role::resolve(state_dir, config)?;
    revoke(
        &target.endpoint,
        &target.token,
        &target.journal_root,
        host,
        request_id,
    )
    .await
}

pub(crate) async fn revoke(
    endpoint: &str,
    token: &str,
    journal_root: &Path,
    host: &str,
    request_id: Option<&str>,
) -> Result<Value, StructuredError> {
    // The server validates the same shape; refusing here keeps a malformed
    // name out of the request journal and the URL.
    if !mllm_store::enrollment::valid_name(host) {
        return Err(error(
            "invalid_config",
            "A host is named by its name or id: letters, digits, '-', '_' or '.'",
        ));
    }
    let id = request_id
        .map(str::to_owned)
        .unwrap_or_else(|| ulid::Ulid::new().to_string());
    let authorization = Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let journal = RequestJournal::open(
        journal_root,
        &id,
        endpoint,
        authorization,
        json!({"command": "revoke", "host": host}),
    )?;
    eprintln!("Request identity: {id} (reuse --request-id {id} to recover this command)");
    // SPEC §13: the exact mutation is committed before it is sent.
    let mutation = journal.prepare("revoke", &format!("/hosts/{host}/revoke"), json!({}))?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| error("internal", "Cannot build management client"))?;
    let mut response = client
        .post(format!("{endpoint}{}", mutation.path))
        .bearer_auth(token)
        .header("idempotency-key", mutation.key)
        .send()
        .await
        .map_err(|_| unavailable())?;
    let status = response.status();
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| unavailable())? {
        if bytes.len().saturating_add(chunk.len()) > 64 * 1024 {
            return Err(error("internal", "Management response exceeds its bound"));
        }
        bytes.extend_from_slice(&chunk);
    }
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| error("internal", "Invalid management response"))?;
    if !status.is_success() {
        // SPEC §14: the refusal keeps the server's error class; an unknown
        // host is `not_found`, a malformed one `invalid_config`.
        return Err(crate::client::refusal(status, &value));
    }
    if value["revoked"] != true {
        return Err(error("internal", "Invalid management response"));
    }
    Ok(json!({
        "host_id": value["host_id"],
        "name": value["name"],
        "revoked": true,
        "newly_revoked": value["newly_revoked"],
        "request_id": id,
        // SPEC §13.3: revocation stops new work only; engines keep running
        // and stay accounted until an operator settles them with evidence.
        "engines": "retained",
    }))
}
