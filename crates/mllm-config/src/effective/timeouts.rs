//! Owner decision 2026-09-22 (1), ADR 0014 amendment A1: a deployment's
//! Initialize and wake timeouts.
//!
//! A deployment may declare `timeouts.initialize` and `timeouts.wake`. When it
//! omits one, mllm derives it from the checkpoint's weights bytes (ADR 0014 §7,
//! WE3). The formula is a placeholder until M16 measures real cold starts and
//! wakes; the constants below are the whole of it.
//!
//! Every lifecycle deadline is bounded by the deployment's request deadline:
//! the store refuses any operation whose deadline lies further out than that
//! (SPEC §6, `accept_start` and the cleanup acceptors). So a declared timeout
//! above the request deadline is refused at resolution, and a derived one is
//! lowered to it. The same bound makes the Stop window below always admissible.
//! A request deadline below the Initialize floor is refused outright: lowering
//! a derived window to it would produce a window no activation can meet.

use super::*;

/// Placeholder formula (recomputed after M16): Initialize is 120 s plus 10 s
/// per GB of weights, capped at 1800 s.
pub const INITIALIZE_BASE_MS: i64 = 120_000;
pub const INITIALIZE_PER_GB_MS: i64 = 10_000;
pub const INITIALIZE_CAP_MS: i64 = 1_800_000;
/// Placeholder formula (recomputed after M16): wake is 60 s plus 5 s per GB of
/// weights, capped at 900 s.
pub const WAKE_BASE_MS: i64 = 60_000;
pub const WAKE_PER_GB_MS: i64 = 5_000;
pub const WAKE_CAP_MS: i64 = 900_000;
/// While the checkpoint digest (and so the weights bytes) is pending, the
/// conservative Initialize timeout. Wake uses its cap: nothing wakes before a
/// launch, and a launch waits for the digest.
pub const PENDING_INITIALIZE_MS: i64 = 900_000;
pub const PENDING_WAKE_MS: i64 = WAKE_CAP_MS;
/// A declared Initialize timeout below this would fail every real cold start
/// (the coordinator's own floor, SPEC §4).
pub const MIN_INITIALIZE_MS: i64 = 30_000;
/// A declared wake timeout below this cannot cover the checkpoint re-check
/// that precedes every wake (ADR 0014 §7).
pub const MIN_WAKE_MS: i64 = 10_000;
/// The Stop window a caller asks for when it names none: the previous fixed CLI
/// window, lowered to the request deadline so the store always admits it.
pub const STOP_WINDOW_MS: i64 = 900_000;
/// The decimal gigabyte the per-GB terms count, rounded up.
const GB: i64 = 1_000_000_000;

/// T14: where a timeout's value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeoutSource {
    /// The deployment declared it under `timeouts`.
    Declared,
    /// mllm derived it from the checkpoint's weights bytes, or used the
    /// pending value while those are unmeasured.
    Derived,
}

/// What a derived timeout was computed from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeoutBasis {
    /// The weights bytes of the recorded checkpoint manifest.
    CheckpointWeights,
    /// The checkpoint digest is pending; the conservative value applies.
    CheckpointDigestPending,
}

/// The resolved `timeouts` of a deployment revision, in milliseconds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeploymentTimeouts {
    pub initialize_ms: i64,
    pub wake_ms: i64,
    /// T14: `declared` or `derived`, per field (`initialize`, `wake`).
    pub provenance: BTreeMap<String, TimeoutSource>,
    /// Present when any value is derived.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub basis: Option<TimeoutBasis>,
}

/// The declared `timeouts` block.
#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawTimeouts {
    #[serde(default)]
    initialize: Option<String>,
    #[serde(default)]
    wake: Option<String>,
}

/// The placeholder Initialize formula for `weights_bytes`, before the request
/// deadline bound.
pub fn derived_initialize_ms(weights_bytes: Option<i64>) -> i64 {
    match weights_bytes {
        None => PENDING_INITIALIZE_MS,
        Some(bytes) => per_gb(
            bytes,
            INITIALIZE_BASE_MS,
            INITIALIZE_PER_GB_MS,
            INITIALIZE_CAP_MS,
        ),
    }
}

/// The placeholder wake formula for `weights_bytes`, before the request
/// deadline bound.
pub fn derived_wake_ms(weights_bytes: Option<i64>) -> i64 {
    match weights_bytes {
        None => PENDING_WAKE_MS,
        Some(bytes) => per_gb(bytes, WAKE_BASE_MS, WAKE_PER_GB_MS, WAKE_CAP_MS),
    }
}

fn per_gb(bytes: i64, base: i64, per: i64, cap: i64) -> i64 {
    let gigabytes = bytes.max(0).saturating_add(GB - 1) / GB;
    base.saturating_add(gigabytes.saturating_mul(per)).min(cap)
}

fn declared(
    value: Option<&str>,
    path: &str,
    floor: i64,
    request_deadline_ms: i64,
) -> Result<Option<i64>, ConfigError> {
    let Some(text) = value else {
        return Ok(None);
    };
    let ms = parse_duration_ms(text).map_err(|e| ConfigError::new(e.code, path, e.detail))?;
    if ms < floor {
        return Err(invalid(path, format!("must be at least {}s", floor / 1000)));
    }
    // SPEC §6: an operation's deadline may not lie beyond the request deadline,
    // so a longer timeout could never be used.
    if ms > request_deadline_ms {
        return Err(invalid(
            path,
            "must not exceed the deployment's request deadline",
        ));
    }
    Ok(Some(ms))
}

/// SPEC §15.3, T03 T20: every Initialize and wake window is lowered to the
/// request deadline, so a deadline below the larger of their floors would
/// resolve windows no cold start or wake can meet. Refuse it instead of
/// silently accepting activations that are bound to time out.
fn refuse_short_request_deadline(request_deadline_ms: i64) -> Result<(), ConfigError> {
    let floor = MIN_INITIALIZE_MS.max(MIN_WAKE_MS);
    if request_deadline_ms < floor {
        return Err(invalid(
            "request_deadline",
            format!(
                "must be at least {}s: the Initialize and wake windows are bounded by it",
                floor / 1000
            ),
        ));
    }
    Ok(())
}

/// Resolve the `timeouts` block against the deployment's request deadline and
/// the checkpoint facts the rest of the revision is resolved with.
pub(super) fn resolve_timeouts(
    raw: Option<&RawTimeouts>,
    request_deadline_ms: i64,
    facts: CheckpointFacts,
) -> Result<DeploymentTimeouts, ConfigError> {
    let raw = raw.cloned().unwrap_or_default();
    refuse_short_request_deadline(request_deadline_ms)?;
    // Zero weights is the placeholder a provisional revision is frozen with
    // until its digest is measured (WE3); it counts as unmeasured here, so the
    // conservative value is shown until the real weights re-resolve it.
    let weights = facts.weights_bytes.filter(|bytes| *bytes > 0);
    let initialize = declared(
        raw.initialize.as_deref(),
        "timeouts.initialize",
        MIN_INITIALIZE_MS,
        request_deadline_ms,
    )?;
    let wake = declared(
        raw.wake.as_deref(),
        "timeouts.wake",
        MIN_WAKE_MS,
        request_deadline_ms,
    )?;
    let mut provenance = BTreeMap::new();
    let mut pick = |field: &str, declared: Option<i64>, derived: i64| match declared {
        Some(ms) => {
            provenance.insert(field.to_owned(), TimeoutSource::Declared);
            ms
        }
        None => {
            provenance.insert(field.to_owned(), TimeoutSource::Derived);
            derived.min(request_deadline_ms)
        }
    };
    let initialize_ms = pick("initialize", initialize, derived_initialize_ms(weights));
    let wake_ms = pick("wake", wake, derived_wake_ms(weights));
    let basis = (initialize.is_none() || wake.is_none()).then_some(match weights {
        Some(_) => TimeoutBasis::CheckpointWeights,
        None => TimeoutBasis::CheckpointDigestPending,
    });
    Ok(DeploymentTimeouts {
        initialize_ms,
        wake_ms,
        provenance,
        basis,
    })
}

/// SPEC §15.3 / `validate config` without a host: the declared block's own
/// shape and floors, and the request deadline bound when the deployment states
/// one. The host's default request deadline is checked at resolution.
pub fn validate_declared_timeouts(deployment: &serde_json::Value) -> Result<(), ConfigError> {
    let ceiling = match deployment.get("request_deadline").and_then(|v| v.as_str()) {
        Some(text) => parse_duration_ms(text)?,
        None => i64::MAX,
    };
    let Some(block) = deployment.get("timeouts") else {
        return refuse_short_request_deadline(ceiling);
    };
    let raw: RawTimeouts = decode(block, "timeouts")?;
    resolve_timeouts(Some(&raw), ceiling, CheckpointFacts::default()).map(|_| ())
}

/// The lifecycle windows a caller that names none asks for, in milliseconds:
/// Initialize is the deployment's resolved timeout, Stop is
/// [`STOP_WINDOW_MS`]; both lowered to the request deadline, which the store
/// enforces. A revision resolved before `timeouts` existed has none; it gets
/// the pending Initialize value, lowered the same way.
pub fn lifecycle_windows(request_deadline_ms: i64, initialize_ms: Option<i64>) -> (i64, i64) {
    let initialize = initialize_ms
        .unwrap_or(PENDING_INITIALIZE_MS)
        .min(request_deadline_ms);
    (initialize, STOP_WINDOW_MS.min(request_deadline_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: i64 = 1 << 30;

    // T14
    #[test]
    fn the_placeholder_formula_rounds_gigabytes_up_and_caps() {
        assert_eq!(derived_initialize_ms(Some(0)), 120_000);
        assert_eq!(derived_initialize_ms(Some(1)), 130_000);
        assert_eq!(derived_initialize_ms(Some(8 * GB)), 200_000);
        assert_eq!(derived_initialize_ms(Some(200 * GB)), INITIALIZE_CAP_MS);
        assert_eq!(derived_initialize_ms(None), PENDING_INITIALIZE_MS);
        assert_eq!(derived_wake_ms(Some(8 * GB)), 100_000);
        assert_eq!(derived_wake_ms(Some(500 * GIB)), WAKE_CAP_MS);
        assert_eq!(derived_wake_ms(None), PENDING_WAKE_MS);
    }

    // T14
    #[test]
    fn derived_values_are_lowered_to_the_request_deadline() {
        let t = resolve_timeouts(
            None,
            300_000,
            CheckpointFacts {
                weights_bytes: Some(60 * GB),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!((t.initialize_ms, t.wake_ms), (300_000, 300_000));
        assert_eq!(t.provenance["initialize"], TimeoutSource::Derived);
        assert_eq!(t.basis, Some(TimeoutBasis::CheckpointWeights));
    }

    // T03
    #[test]
    fn declared_values_outside_their_bounds_are_refused() {
        for (initialize, wake, path) in [
            ("29s", "60s", "timeouts.initialize"),
            ("601s", "60s", "timeouts.initialize"),
            ("60s", "9s", "timeouts.wake"),
            ("60s", "601s", "timeouts.wake"),
            ("10GiB", "60s", "timeouts.initialize"),
        ] {
            let raw = RawTimeouts {
                initialize: Some(initialize.into()),
                wake: Some(wake.into()),
            };
            let error =
                resolve_timeouts(Some(&raw), 600_000, CheckpointFacts::default()).unwrap_err();
            assert_eq!(error.path, path, "{initialize} {wake}");
        }
    }

    // T14
    #[test]
    fn the_provisional_zero_weight_placeholder_counts_as_unmeasured() {
        let t = resolve_timeouts(
            None,
            3_600_000,
            CheckpointFacts {
                weights_bytes: Some(0),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            (t.initialize_ms, t.wake_ms),
            (PENDING_INITIALIZE_MS, PENDING_WAKE_MS)
        );
        assert_eq!(t.basis, Some(TimeoutBasis::CheckpointDigestPending));
    }

    // T03 T20 (SPEC §15.3): a request deadline shorter than the Initialize or
    // wake floor would lower the derived windows below what any cold start or
    // wake can meet, so the configuration is refused rather than accepted with
    // windows that fail every activation.
    #[test]
    fn a_request_deadline_below_the_activation_floors_is_refused() {
        for deadline in [5_000, MIN_WAKE_MS, MIN_INITIALIZE_MS - 1] {
            let error = resolve_timeouts(None, deadline, CheckpointFacts::default()).unwrap_err();
            assert_eq!(error.path, "request_deadline", "{deadline}");
            assert!(error.detail.contains("30s"), "{}", error.detail);
        }
        let t = resolve_timeouts(None, MIN_INITIALIZE_MS, CheckpointFacts::default()).unwrap();
        assert_eq!((t.initialize_ms, t.wake_ms), (MIN_INITIALIZE_MS, MIN_INITIALIZE_MS));
        let stated = serde_json::json!({"request_deadline": "5s"});
        assert_eq!(
            validate_declared_timeouts(&stated).unwrap_err().path,
            "request_deadline"
        );
    }

    // T20
    #[test]
    fn windows_never_exceed_the_request_deadline() {
        assert_eq!(
            lifecycle_windows(600_000, Some(1_200_000)),
            (600_000, 600_000)
        );
        assert_eq!(
            lifecycle_windows(3_600_000, None),
            (PENDING_INITIALIZE_MS, STOP_WINDOW_MS)
        );
        assert_eq!(lifecycle_windows(20_000, Some(20_000)), (20_000, 20_000));
    }
}
