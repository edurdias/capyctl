//! SPEC §13.3 / ADR 0019, design §9: exposing the inference endpoint without
//! a key is the operator's explicit choice, and it is said out loud.

use std::net::SocketAddr;

pub use capyctl_config::standalone::InferenceAuth;

use crate::roles::StartError;

/// Design §9, owner rule (flag > environment > document > default): the
/// variable that sets the inference authentication of either role for one
/// run, `api_key` or `none`.
pub const INFERENCE_AUTH_ENV: &str = "CAPYCTL_INFERENCE_AUTH";

/// Design §9, owner rule: the inference authentication of either role for
/// this run. `--no-inference-auth` (`no_auth_flag`) > `CAPYCTL_INFERENCE_AUTH` >
/// the document's `listeners.inference.authentication` (`document`, which is
/// `api_key` when the document states none). A malformed variable refuses the
/// start; it never falls back to either mode.
pub fn effective_inference_auth(
    document: InferenceAuth,
    no_auth_flag: bool,
) -> Result<InferenceAuth, StartError> {
    if no_auth_flag {
        return Ok(InferenceAuth::None);
    }
    match std::env::var_os(INFERENCE_AUTH_ENV) {
        None => Ok(document),
        Some(value) => value
            .to_str()
            .and_then(InferenceAuth::parse)
            .ok_or_else(|| {
                StartError::Setting(format!("{INFERENCE_AUTH_ENV} must be `api_key` or `none`"))
            }),
    }
}

/// Design §9: the start warning for an inference endpoint that serves every
/// client without a key on an address other hosts can reach. `Some` exactly
/// when `bind` is not loopback and `auth` is [`InferenceAuth::None`]; a
/// loopback bind without a key says nothing.
pub fn exposure_warning(bind: SocketAddr, auth: InferenceAuth) -> Option<String> {
    if bind.ip().is_loopback() || auth == InferenceAuth::ApiKey {
        return None;
    }
    Some(format!(
        "WARNING: the inference endpoint on {bind} accepts requests without an API key.\n\
         Anyone who can reach this address can use your models and GPU.\n\
         Set listeners.inference.authentication: api_key, or bind to 127.0.0.1 or a Tailscale address with --listen."
    ))
}

/// Design §9: print the [`exposure_warning`], if any, to stderr (the role's
/// log) before the listener accepts connections.
pub fn warn_if_exposed(bind: SocketAddr, auth: InferenceAuth) {
    if let Some(warning) = exposure_warning(bind, auth) {
        eprintln!("{}", warning_line(capyctl_domain::role_log::mode(), &warning));
    }
}

/// ADR 0019: on a terminal the warning keeps its documented wording; piped,
/// it is a warning notice like every other role line.
fn warning_line(mode: capyctl_domain::role_log::Mode, warning: &str) -> String {
    match mode {
        capyctl_domain::role_log::Mode::Text => warning.to_owned(),
        capyctl_domain::role_log::Mode::Json => {
            let text = warning.strip_prefix("WARNING: ").unwrap_or(warning);
            serde_json::json!({"level": "warning", "message": text}).to_string()
        }
    }
}

/// Design §9: the management view of the inference listener, as status
/// reads it (`inference_listener: {bind, authenticated}`).
pub fn listener_view(bind: SocketAddr, auth: InferenceAuth) -> serde_json::Value {
    serde_json::json!({
        "bind": bind.to_string(),
        "authenticated": auth == InferenceAuth::ApiKey,
    })
}

/// Design §9: the status line repeating the warning,
/// `inference: unauthenticated on <addr>`, for a view whose
/// `inference_listener` is not authenticated. Any loopback or keyed listener
/// gives `None`, as does a view without the field.
pub fn status_notice(view: &serde_json::Value) -> Option<String> {
    let listener = &view["inference_listener"];
    if listener["authenticated"].as_bool() != Some(false) {
        return None;
    }
    let bind = listener["bind"].as_str()?;
    let loopback = bind
        .parse::<SocketAddr>()
        .is_ok_and(|address| address.ip().is_loopback());
    (!loopback).then(|| format!("inference: unauthenticated on {bind}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // T37 (ADR 0019): text keeps the documented wording, JSON wraps it.
    #[test]
    fn the_warning_line_follows_the_mode() {
        use capyctl_domain::role_log::Mode;
        let open: SocketAddr = "0.0.0.0:8443".parse().unwrap();
        let warning = exposure_warning(open, InferenceAuth::None).unwrap();
        assert!(warning_line(Mode::Text, &warning)
            .starts_with("WARNING: the inference endpoint on 0.0.0.0:8443 accepts requests"));
        assert!(warning_line(Mode::Json, &warning)
            .starts_with(r#"{"level":"warning","message":"the inference endpoint on"#));
    }

    // T37: the warning fires only for an unauthenticated non-loopback bind.
    #[test]
    fn the_warning_is_loud_and_precise() {
        let open: SocketAddr = "0.0.0.0:8443".parse().unwrap();
        let text = exposure_warning(open, InferenceAuth::None).expect("warns");
        assert!(text.starts_with(
            "WARNING: the inference endpoint on 0.0.0.0:8443 accepts requests without an API key."
        ));
        assert!(text.contains("Anyone who can reach this address can use your models and GPU."));
        assert!(text.contains("--listen"));
        assert!(exposure_warning(open, InferenceAuth::ApiKey).is_none());
        assert!(exposure_warning("127.0.0.1:8443".parse().unwrap(), InferenceAuth::None).is_none());
        assert!(exposure_warning("[::1]:8443".parse().unwrap(), InferenceAuth::None).is_none());
        assert!(exposure_warning("[::]:8443".parse().unwrap(), InferenceAuth::None).is_some());
        assert!(
            exposure_warning("100.64.0.5:8443".parse().unwrap(), InferenceAuth::None).is_some()
        );
    }

    // T37 (design §9): status repeats the warning for an unauthenticated
    // non-loopback listener and says nothing otherwise.
    #[test]
    fn status_repeats_the_warning() {
        let open: SocketAddr = "0.0.0.0:8443".parse().unwrap();
        let view = |bind: SocketAddr, auth| serde_json::json!({ "inference_listener": listener_view(bind, auth) });
        assert_eq!(
            status_notice(&view(open, InferenceAuth::None)).as_deref(),
            Some("inference: unauthenticated on 0.0.0.0:8443")
        );
        assert_eq!(status_notice(&view(open, InferenceAuth::ApiKey)), None);
        assert_eq!(
            status_notice(&view(
                "127.0.0.1:8443".parse().unwrap(),
                InferenceAuth::None
            )),
            None
        );
        assert_eq!(status_notice(&serde_json::json!({"id": "d"})), None);
    }

    // T03 T37 (design §9, owner rule): the flag beats CAPYCTL_INFERENCE_AUTH,
    // which beats the document; a malformed variable refuses.
    #[test]
    fn flag_beats_environment_beats_document() {
        use InferenceAuth::{ApiKey, None as Off};
        std::env::remove_var(INFERENCE_AUTH_ENV);
        assert_eq!(effective_inference_auth(ApiKey, false).unwrap(), ApiKey);
        assert_eq!(effective_inference_auth(Off, false).unwrap(), Off);
        assert_eq!(effective_inference_auth(ApiKey, true).unwrap(), Off);
        std::env::set_var(INFERENCE_AUTH_ENV, "none");
        assert_eq!(effective_inference_auth(ApiKey, false).unwrap(), Off);
        std::env::set_var(INFERENCE_AUTH_ENV, "api_key");
        assert_eq!(effective_inference_auth(Off, false).unwrap(), ApiKey);
        assert_eq!(effective_inference_auth(Off, true).unwrap(), Off);
        for bad in ["", "None", "off", "token"] {
            std::env::set_var(INFERENCE_AUTH_ENV, bad);
            let error = effective_inference_auth(ApiKey, false).unwrap_err();
            assert!(error.to_string().contains(INFERENCE_AUTH_ENV), "{error}");
        }
        std::env::remove_var(INFERENCE_AUTH_ENV);
    }
}
