//! SPEC §10 (owner decision 2026-10-19): the identifiers of one inference
//! request.
//!
//! - The caller's `X-Request-Id`, optional, in a fixed format: 1 to 128 bytes
//!   of printable ASCII with no space or control character. Anything else is
//!   refused `400 invalid_request` before the body is read. A well-formed one
//!   is echoed on the response and written to CapyCTL's own role logs for the
//!   request; it is never sent to an engine (SPEC §10 / T21: CapyCTL consumes
//!   this header) and never written to the ledger or to an event.
//! - CapyCTL's `X-CapyCTL-Request-Id`, on every response of the inference
//!   endpoints, success or error: the ID of the first request lease granted
//!   for the request, so the response names the lease the ledger and the role
//!   logs know it by. A request refused before any lease was granted gets a
//!   fresh ULID instead, so every response carries one.

use std::sync::OnceLock;

use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use axum::Json;

/// The caller's request ID header.
pub const CALLER_HEADER: &str = "x-request-id";
/// CapyCTL's request ID header.
pub const CAPYCTL_HEADER: &str = "x-capyctl-request-id";
/// The longest caller request ID accepted, in bytes.
pub const MAX_CALLER_ID_BYTES: usize = 128;

/// Whether `value` is a well-formed caller request ID: 1 to
/// [`MAX_CALLER_ID_BYTES`] bytes, each printable ASCII other than space.
pub fn well_formed(value: &[u8]) -> bool {
    (1..=MAX_CALLER_ID_BYTES).contains(&value.len()) && value.iter().all(u8::is_ascii_graphic)
}

/// The identifiers of one request. The caller's is read once from the
/// request's headers; CapyCTL's is set by the first lease granted, or minted
/// the first time it is asked for before any was.
#[derive(Debug)]
pub struct RequestIds {
    /// `Ok(None)` when the caller sent none, `Err` when what it sent is not a
    /// well-formed ID (more than one header included).
    caller: Result<Option<String>, ()>,
    capyctl: OnceLock<String>,
}

impl RequestIds {
    /// The identifiers of a request with these headers.
    pub fn read(headers: &HeaderMap) -> Self {
        let mut values = headers.get_all(CALLER_HEADER).iter();
        let caller = match (values.next(), values.next()) {
            (None, _) => Ok(None),
            (Some(value), None) if well_formed(value.as_bytes()) => {
                // Printable ASCII only, so the conversion cannot fail.
                value.to_str().map(|id| Some(id.to_owned())).map_err(|_| ())
            }
            _ => Err(()),
        };
        Self {
            caller,
            capyctl: OnceLock::new(),
        }
    }

    /// Identifiers for a request with no caller ID (callers that predate them).
    pub fn untracked() -> Self {
        Self {
            caller: Ok(None),
            capyctl: OnceLock::new(),
        }
    }

    /// Refuse a malformed caller ID, naming the header.
    pub fn check(&self) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
        match self.caller {
            Ok(_) => Ok(()),
            Err(()) => Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "code": "invalid_request",
                    "message": format!(
                        "X-Request-Id must be one value of 1 to {MAX_CALLER_ID_BYTES} printable \
                         ASCII characters with no spaces"
                    ),
                })),
            )),
        }
    }

    /// The caller's well-formed request ID, if it sent one.
    pub fn caller(&self) -> Option<&str> {
        self.caller.as_ref().ok().and_then(Option::as_deref)
    }

    /// A lease was granted for the request: the first one names it.
    pub fn granted(&self, lease: &str) {
        let _ = self.capyctl.set(lease.to_owned());
    }

    /// Whether CapyCTL's ID is decided.
    pub fn settled(&self) -> bool {
        self.capyctl.get().is_some()
    }

    /// CapyCTL's request ID: the first granted lease's, else a ULID minted
    /// now and kept.
    pub fn capyctl(&self) -> &str {
        self.capyctl.get_or_init(|| ulid::Ulid::new().to_string())
    }

    /// Both IDs as role-log fields: `capyctl_request_id` always,
    /// `caller_request_id` when the caller sent one.
    pub fn log_fields(&self, event: &mut serde_json::Value) {
        if let Some(object) = event.as_object_mut() {
            object.insert("capyctl_request_id".into(), self.capyctl().into());
            if let Some(caller) = self.caller() {
                object.insert("caller_request_id".into(), caller.into());
            }
        }
    }

    /// Both IDs as a suffix for a role-log notice.
    pub fn log_suffix(&self) -> String {
        match self.caller() {
            Some(caller) => format!(
                " (CapyCTL request {}, caller request {caller})",
                self.capyctl()
            ),
            None => format!(" (CapyCTL request {})", self.capyctl()),
        }
    }

    /// Put the IDs on a response: CapyCTL's always, the caller's when it sent
    /// a well-formed one.
    pub fn stamp(
        &self,
        outcome: Result<axum::response::Response, (StatusCode, Json<serde_json::Value>)>,
    ) -> axum::response::Response {
        let mut response = outcome.unwrap_or_else(IntoResponse::into_response);
        let headers = response.headers_mut();
        if let Ok(value) = HeaderValue::from_str(self.capyctl()) {
            headers.insert(CAPYCTL_HEADER, value);
        }
        if let Some(Ok(value)) = self.caller().map(HeaderValue::from_str) {
            headers.insert(CALLER_HEADER, value);
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(values: &[&[u8]]) -> RequestIds {
        let mut headers = HeaderMap::new();
        for value in values {
            headers.append(CALLER_HEADER, HeaderValue::from_bytes(value).unwrap());
        }
        RequestIds::read(&headers)
    }

    // T19: the caller's ID has one fixed format; anything else is refused.
    #[test]
    fn the_caller_id_format_is_fixed() {
        assert_eq!(read(&[]).caller(), None);
        assert!(read(&[]).check().is_ok());
        for good in [&b"a"[..], b"req-01J9:x/y?z=1&{}", &[b'~'; 128]] {
            let ids = read(&[good]);
            assert!(ids.check().is_ok(), "{good:?}");
            assert_eq!(ids.caller().unwrap().as_bytes(), good);
        }
        for bad in [
            &b""[..],
            b"has space",
            b"tab\there",
            &[b'a'; 129],
            "caf\u{e9}".as_bytes(),
        ] {
            let ids = read(&[bad]);
            let (status, Json(body)) = ids.check().unwrap_err();
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(body["code"], "invalid_request");
            assert!(body["message"].as_str().unwrap().contains("X-Request-Id"));
            assert_eq!(ids.caller(), None);
        }
        assert!(read(&[b"one", b"two"]).check().is_err());
    }

    // T17: the first granted lease names the request; without one a ULID does.
    #[test]
    fn the_first_lease_names_the_request() {
        let ids = RequestIds::untracked();
        assert!(!ids.settled());
        ids.granted("lease-1");
        ids.granted("lease-2");
        assert_eq!(ids.capyctl(), "lease-1");
        let minted = RequestIds::untracked();
        let id = minted.capyctl().to_owned();
        assert!(ulid::Ulid::from_string(&id).is_ok(), "{id}");
        minted.granted("too-late");
        assert_eq!(minted.capyctl(), id);
    }

    // T17: role-log lines name both IDs; the caller's only when it sent one.
    #[test]
    fn log_lines_carry_both_ids() {
        let ids = read(&[b"caller-7"]);
        ids.granted("lease-9");
        let mut event = serde_json::json!({"event": "router_failover"});
        ids.log_fields(&mut event);
        assert_eq!(
            event,
            serde_json::json!({"event": "router_failover",
                "capyctl_request_id": "lease-9", "caller_request_id": "caller-7"})
        );
        assert_eq!(
            ids.log_suffix(),
            " (CapyCTL request lease-9, caller request caller-7)"
        );
        let anonymous = RequestIds::untracked();
        anonymous.granted("lease-3");
        let mut event = serde_json::json!({});
        anonymous.log_fields(&mut event);
        assert_eq!(event, serde_json::json!({"capyctl_request_id": "lease-3"}));
        assert_eq!(anonymous.log_suffix(), " (CapyCTL request lease-3)");
    }
}
