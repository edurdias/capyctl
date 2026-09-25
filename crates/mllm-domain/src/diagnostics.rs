//! SPEC §6.4: the reason and hint status shows for an operation's error.
//!
//! Status exposes the latest operation and its error. The recorded reason is
//! evidence written by the controller (already redacted at the writer); this
//! module bounds it once more for a status reader: one line, no engine log
//! tail, nothing that may quote a credential, and at most [`MAX_REASON_BYTES`].
//! The hint is fixed text per closed category, never detail from the engine,
//! a path, an option value or a credential.

/// The longest reason status shows.
pub const MAX_REASON_BYTES: usize = 512;

/// Closed categories with an operator hint, most specific first, so a reason
/// that names several is classified by the most specific one.
const HINTS: &[(&str, &str)] = &[
    (
        "capability_missing:deep_park",
        "this engine installation lacks what deep parking needs; declare residency restart_only, or use a build that provides it",
    ),
    (
        "capability_missing:core",
        "this engine installation lacks an interface every launch needs",
    ),
    (
        "installation_drift",
        "the installation changed since the host registered it and its host policy says installation_drift: refuse; restart the host agent to register it again",
    ),
    (
        "checkpoint_mismatch",
        "the checkpoint on the host no longer matches the digest recorded for this revision; restore that checkpoint, or deploy a new revision so it is measured again",
    ),
    (
        "checkpoint_unverified",
        "the host could not measure the checkpoint; check that it exists and the host agent can read it",
    ),
    (
        "startup_requires_empty_host",
        "the model's startup peak is unmeasured and exceeds the host's managed limit, so its first start needs an empty host; start it with --evict to release the other engines there",
    ),
    (
        "residency_tier",
        "the deployment's declared residency does not park; stop it instead, or declare a parking residency",
    ),
    (
        "insufficient_memory",
        "the host cannot hold the launch now; stop or park another deployment there, or lower the deployment's memory request",
    ),
    (
        "insufficient_device_memory",
        "the GPU cannot hold the launch now; stop or park another deployment on that GPU, or lower the deployment's device memory request",
    ),
    (
        "no_host_fits",
        "no allowed host has room without eviction; start it with --evict, or free capacity on an allowed host",
    ),
    (
        "insufficient_capacity",
        "no allowed host has room without eviction; start it with --evict, or free capacity on an allowed host",
    ),
    (
        "engine_argument_rejected",
        "the engine refused an argument the deployment passes; correct engine_config (including extra_args) and deploy a new revision",
    ),
    (
        "engine_exited",
        "the engine exited before it was ready; check the deployment's engine_config and the host's private engine log",
    ),
];

/// Text a status reader must not see quoted (lower-cased comparison).
const SENSITIVE: &[&str] = &[
    "bearer",
    "authorization",
    "api_key",
    "api-key",
    "apikey",
    "password",
    "secret",
    "token=",
    "key=",
    "private key",
];

/// Whether `code` is a closed diagnostic code (`[a-z0-9_:.]`, at most 64
/// bytes), the only shape an operation's error code is shown in.
pub fn is_closed_code(code: &str) -> bool {
    !code.is_empty()
        && code.len() <= 64
        && code
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_:.".contains(&b))
}

/// The fixed operator hint for a closed category, if it has one.
pub fn operator_hint(code: &str) -> Option<&'static str> {
    HINTS
        .iter()
        .find(|(known, _)| *known == code)
        .map(|(_, hint)| *hint)
}

/// The closed category a recorded failure belongs to: the most specific code
/// the reason names, else what the reason says happened to the engine, else
/// the operation's own error code when it has a hint.
pub fn classify(error_code: Option<&str>, reason: &str) -> Option<&'static str> {
    if let Some((code, _)) = HINTS.iter().find(|(code, _)| reason.contains(code)) {
        return Some(code);
    }
    if reason.contains("rejected argument") || reason.contains("rejected its argument") {
        return Some("engine_argument_rejected");
    }
    if reason.contains("exited before readiness") {
        return Some("engine_exited");
    }
    let code = error_code?;
    HINTS
        .iter()
        .find(|(known, _)| *known == code)
        .map(|(known, _)| *known)
}

/// The recorded reason as status shows it, or `None` when nothing may be shown.
///
/// Only the first line is kept (an engine log tail never follows it), the
/// journal's `deployment <id>: ` prefix and the coordinator's service wrapper
/// are removed, and a reason that may quote a credential is withheld.
pub fn public_reason(evidence: &str) -> Option<String> {
    let mut text = evidence.lines().next()?.trim();
    // A structured journal record is evidence for tools, not a reason.
    if text.starts_with('{') || text.starts_with('[') {
        return None;
    }
    if let Some(rest) = text.strip_prefix("deployment ") {
        if let Some((id, tail)) = rest.split_once(": ") {
            if !id.is_empty()
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            {
                text = tail;
            }
        }
    }
    let owned = text.replace("coordinator service failed: ", "");
    let mut text = owned.as_str();
    for marker in ["; log tail:", " log tail:", "log tail:"] {
        if let Some((head, _)) = text.split_once(marker) {
            text = head;
        }
    }
    let text = text.trim().trim_end_matches([';', ':', ',']).trim();
    if text.is_empty() {
        return None;
    }
    let lower = text.to_ascii_lowercase();
    if SENSITIVE.iter().any(|marker| lower.contains(marker)) {
        return Some(
            "the recorded reason is withheld from status because it may quote a credential".into(),
        );
    }
    let mut end = text.len().min(MAX_REASON_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Some(text[..end].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    // T08 T29 (SPEC §6.4): a status reader sees the reason, never a log tail,
    // the journal's own prefix, or the coordinator's wrapper.
    #[test]
    fn a_reason_keeps_one_line_without_the_log_tail() {
        let evidence = "deployment 01ABC: launch failed: coordinator service failed: engine exited before readiness; log tail:\nValueError: --moe-backend bogus\nsecret";
        assert_eq!(
            public_reason(evidence).as_deref(),
            Some("launch failed: engine exited before readiness")
        );
    }

    // T08: a structured journal record is not a reason.
    #[test]
    fn a_structured_record_is_not_a_reason() {
        assert_eq!(public_reason(r#"{"event":"quiesce_unknown"}"#), None);
        assert_eq!(public_reason(""), None);
        assert_eq!(public_reason("\nsecond line"), None);
    }

    // T29: anything that may quote a credential is withheld, and the reason
    // is bounded.
    #[test]
    fn a_reason_that_may_quote_a_credential_is_withheld_and_long_ones_are_bounded() {
        let withheld = public_reason("launch failed: header Authorization: Bearer abc").unwrap();
        assert!(!withheld.contains("abc"));
        assert!(withheld.contains("withheld"));
        let long = public_reason(&"é".repeat(600)).unwrap();
        assert!(long.len() <= MAX_REASON_BYTES);
    }

    // T08 T29: the most specific closed category the reason names picks the
    // hint; the engine's own exit is classified from what the reason says.
    #[test]
    fn classification_prefers_the_named_category() {
        let refused =
            "host policy refused the launch before any effect: capability_missing:deep_park";
        assert_eq!(
            classify(Some("launch_failed"), refused),
            Some("capability_missing:deep_park")
        );
        assert!(operator_hint("capability_missing:deep_park")
            .unwrap()
            .contains("restart_only"));
        assert_eq!(
            classify(Some("launch_failed"), "engine launch failed: the engine exited before readiness with exit code 2; it rejected argument --moe-backend"),
            Some("engine_argument_rejected")
        );
        assert_eq!(
            classify(Some("launch_failed"), "engine exited before readiness"),
            Some("engine_exited")
        );
        assert_eq!(
            classify(Some("startup_requires_empty_host"), ""),
            Some("startup_requires_empty_host")
        );
        assert_eq!(classify(Some("launch_failed"), "something else"), None);
        assert!(operator_hint("startup_requires_empty_host")
            .unwrap()
            .contains("--evict"));
    }

    // T29: only a closed code is shown as an operation's error code.
    #[test]
    fn only_closed_codes_are_shown() {
        assert!(is_closed_code("launch_failed"));
        assert!(is_closed_code("capability_missing:deep_park"));
        assert!(!is_closed_code("SECRET-ERROR"));
        assert!(!is_closed_code(""));
        assert!(!is_closed_code(&"a".repeat(65)));
    }
}
