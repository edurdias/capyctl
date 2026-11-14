//! ADR 0040 §5 / KTD6: issue a browser's one-time sign-in link on the box.
use std::{path::Path, time::Duration};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::{local_role, output::StructuredError};

fn error(code: &'static str, message: impl Into<String>) -> StructuredError {
    StructuredError {
        code,
        message: message.into(),
    }
}

pub async fn execute(
    state_root: &Path,
    config: Option<&Path>,
    address: Option<&str>,
) -> Result<Value, StructuredError> {
    let target = local_role::resolve(state_root, config).map_err(|mut failure| {
        if failure.message.starts_with("This machine is a capyctl host;") {
            failure.message = "This machine is a capyctl host; the UI is served by the server. Run capyctl open ui on the server.".into();
        }
        failure
    })?;
    // ADR 0040 §5: this origin is for the browser only. Even when overridden,
    // the bearer-authenticated request goes to the resolved local role.
    let public = if address.is_none() {
        target.management_public_address(config)?
    } else {
        None
    };
    let origin = address.map(str::to_owned).or(public);
    let loopback_only = origin.is_none() && target.address.ip().is_loopback();
    let origin = origin.unwrap_or_else(|| format!("http://{}", target.address));
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| error("internal", "Cannot build management client"))?;
    let response = client
        .post(format!("{}/ui/sign-in-links", target.endpoint))
        .bearer_auth(&target.token)
        .send()
        .await
        .map_err(|_| {
            error(
                "management_unavailable",
                "Management request did not complete; inspect status before retrying",
            )
        })?;
    let status = response.status();
    let value: Value = response
        .json()
        .await
        .map_err(|_| error("internal", "Invalid management response"))?;
    if !status.is_success() {
        return Err(crate::client::refusal(status, &value));
    }
    #[derive(Deserialize)]
    struct Link {
        token: String,
        expires_at_ms: i64,
    }
    let link: Link = serde_json::from_value(value)
        .map_err(|_| error("internal", "Invalid sign-in link response"))?;
    Ok(json!({
        "url": format!("{origin}/ui/sign-in#t={}", link.token),
        "expires_at_ms": link.expires_at_ms,
        "loopback_only": loopback_only,
    }))
}
