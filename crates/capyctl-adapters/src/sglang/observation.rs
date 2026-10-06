//! SPEC §9.2, §10: host-side helpers for observing an enrolled SGLang launch.
//!
//! - The per-launch observation key (`observation_key`) and each request's
//!   proof (`request_proof`), mirrored byte for byte by
//!   `runtime/sglang_observation_transport.py`. The key is derived from the
//!   launch's admin credential, which only the engine and its host hold, so a
//!   restarted host can still observe the launch it owns (T33).
//! - Engine quiescence from SGLang's own gauges (`engine_idle`): the running and
//!   waiting request counts on the engine's loopback `/metrics`, read with the
//!   launch's inference key. It is one input to quiescence, never the whole of
//!   it: the host's zero in-flight ingress count is the other (SPEC §10 step 4).

use std::time::Duration;

use sha2::{Digest, Sha256};

const KEY_LABEL: &[u8] = b"capyctl-sglang-observation-key-v1\0";
const PROOF_LABEL: &[u8] = b"capyctl-sglang-observation-request-v2\0";
/// One `/metrics` read, connect through body.
pub const METRICS_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_METRICS_BYTES: usize = 4 * 1024 * 1024;
const RUNNING: &str = "sglang:num_running_reqs";
/// The engine's gauges could not be read; unknown work, never quiescence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("engine gauges unavailable")]
pub struct GaugesUnavailable;
const WAITING: &str = "sglang:num_queue_reqs";

/// HMAC-SHA256 (RFC 2104) over `parts`, concatenated.
fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|b| b ^ 0x36));
    for part in parts {
        inner.update(part);
    }
    let mut outer = Sha256::new();
    outer.update(block.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

/// The per-launch observation key, from the launch's admin credential (hex).
/// ADR 0028 §12 (R12): a group worker holds no admin credential (ADR 0012);
/// its key comes from its own observation credential
/// (`SglangAdapter::with_observation_credential`), passed here in its place.
pub fn observation_key(admin_key: &str, binding_id: &str, incarnation: &str) -> [u8; 32] {
    hmac_sha256(
        admin_key.as_bytes(),
        &[
            KEY_LABEL,
            binding_id.as_bytes(),
            b"\0",
            incarnation.as_bytes(),
        ],
    )
}

/// The hex proof a key-mode (version 2) observation request carries.
pub fn request_proof(
    key: &[u8; 32],
    binding_id: &str,
    incarnation: &str,
    request_id: &str,
) -> String {
    hex::encode(hmac_sha256(
        key,
        &[
            PROOF_LABEL,
            binding_id.as_bytes(),
            b"\0",
            incarnation.as_bytes(),
            b"\0",
            request_id.as_bytes(),
        ],
    ))
}

/// The sum of one exact gauge's samples, or `None` when it is absent or any
/// sample of it is malformed, negative or not finite.
fn gauge(text: &str, name: &str) -> Option<f64> {
    let mut total = None::<f64>;
    for line in text.lines().map(str::trim) {
        if line.starts_with('#') {
            continue;
        }
        let Some(rest) = line.strip_prefix(name) else {
            continue;
        };
        let rest = match rest.chars().next() {
            // Labels never contain `}` unquoted in SGLang's exposition; a
            // quoted one fails the value parse below and the whole read.
            Some('{') => &rest[rest.find('}')? + 1..],
            Some(c) if c.is_whitespace() => rest,
            _ => continue,
        };
        let value: f64 = rest.split_whitespace().next()?.parse().ok()?;
        if !value.is_finite() || value < 0.0 {
            return None;
        }
        total = Some(total.unwrap_or(0.0) + value);
    }
    total
}

/// SPEC §10 step 4: whether the engine's own gauges show no running and no
/// waiting request. `None` when either gauge is missing or malformed: unknown
/// work is never quiescence.
pub fn idle_from_metrics(text: &str) -> Option<bool> {
    let running = gauge(text, RUNNING)?;
    let waiting = gauge(text, WAITING)?;
    Some(running == 0.0 && waiting == 0.0)
}

/// Read the engine's `/metrics` on loopback with the inference key and report
/// whether it is idle. Any failure is `Err`: the caller treats it as unknown.
pub async fn engine_idle(endpoint: &str, inference_key: &str) -> Result<bool, GaugesUnavailable> {
    let base: reqwest::Url = endpoint.parse().map_err(|_| GaugesUnavailable)?;
    // SPEC §13.3: metrics are read on loopback only.
    let loopback = base
        .host_str()
        .map(|host| host.trim_start_matches('[').trim_end_matches(']'))
        .and_then(|host| host.parse::<std::net::IpAddr>().ok())
        .is_some_and(|ip| ip.is_loopback());
    if !loopback {
        return Err(GaugesUnavailable);
    }
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(METRICS_TIMEOUT)
        .build()
        .map_err(|_| GaugesUnavailable)?;
    let read = async {
        let mut response = client
            .get(base.join("/metrics").map_err(|_| GaugesUnavailable)?)
            .bearer_auth(inference_key)
            .send()
            .await
            .map_err(|_| GaugesUnavailable)?;
        if response.status() != reqwest::StatusCode::OK {
            return Err(GaugesUnavailable);
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| GaugesUnavailable)? {
            if body.len() + chunk.len() > MAX_METRICS_BYTES {
                return Err(GaugesUnavailable);
            }
            body.extend_from_slice(&chunk);
        }
        idle_from_metrics(std::str::from_utf8(&body).map_err(|_| GaugesUnavailable)?)
            .ok_or(GaugesUnavailable)
    };
    tokio::time::timeout(METRICS_TIMEOUT, read)
        .await
        .map_err(|_| GaugesUnavailable)?
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 4231 test case 2: HMAC-SHA256 with a short key.
    #[test]
    fn hmac_matches_rfc_4231() {
        assert_eq!(
            hex::encode(hmac_sha256(
                b"Jefe",
                &[b"what do ya want ", b"for nothing?"]
            )),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // Case 6: a key longer than the block is hashed first.
        assert_eq!(
            hex::encode(hmac_sha256(
                &[0xaa; 131],
                &[b"Test Using Larger Than Block-Size Key - Hash Key First"]
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn keys_are_per_launch_and_proofs_per_request() {
        let key = observation_key("admin", "binding", "incarnation");
        assert_ne!(key, observation_key("admin", "binding", "other"));
        assert_ne!(key, observation_key("other", "binding", "incarnation"));
        let proof = request_proof(&key, "binding", "incarnation", "r-1");
        // The same vectors runtime/sglang_observation_transport.py computes.
        assert_eq!(
            hex::encode(key),
            "b8554e55164b83355eeb71802ce34e3bb258d3ce4d65435d2fb9277913ae85e0"
        );
        assert_eq!(
            proof,
            "1ee4a2715d328a008eb2388c4af021426bac79190b9c6c4977dde11ce9655904"
        );
        assert_ne!(proof, request_proof(&key, "binding", "incarnation", "r-2"));
    }

    // SPEC §10 step 4: only both gauges present and zero are idle; a missing
    // or malformed gauge is unknown, never idle.
    #[test]
    fn idle_needs_both_gauges_at_zero() {
        let idle = "# HELP x\nsglang:num_running_reqs{model_name=\"m\"} 0.0\nsglang:num_queue_reqs{model_name=\"m\"} 0\n";
        assert_eq!(idle_from_metrics(idle), Some(true));
        let busy = idle.replace("} 0.0", "} 1.0");
        assert_eq!(idle_from_metrics(&busy), Some(false));
        assert_eq!(
            idle_from_metrics("sglang:num_running_reqs 0\n"),
            None,
            "a missing waiting gauge is unknown"
        );
        assert_eq!(
            idle_from_metrics("sglang:num_running_reqs 0\nsglang:num_queue_reqs NaN\n"),
            None
        );
        assert_eq!(
            idle_from_metrics(
                "sglang:num_running_reqs_total 5\nsglang:num_running_reqs 0\nsglang:num_queue_reqs 0\n"
            ),
            Some(true),
            "only the exact gauge name counts"
        );
    }
}
