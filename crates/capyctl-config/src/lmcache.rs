//! ADR 0033: LMCache, a private KV cache server per deployment instance.
//!
//! The vocabulary every role shares: the deployment's `lmcache` and
//! `retention` settings, the host's `lmcache` and `retention` settings, the
//! closed set of reasons LMCache is off, the supported engine versions, the
//! qualified parking combinations and the figures LC1 measures.

use serde::{Deserialize, Serialize};

/// ADR 0033 §9: a deployment's `lmcache`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LmcacheMode {
    /// Every deployment that can use LMCache gets it; one that cannot runs
    /// without it, shown off with the reason.
    #[default]
    Auto,
    /// No server, no connector.
    Off,
    /// Refuse to start instead of running without it.
    Required,
}

impl LmcacheMode {
    pub fn is_auto(&self) -> bool {
        *self == Self::Auto
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Off => "off",
            Self::Required => "required",
        }
    }
}

/// ADR 0033 §10: a host's or a deployment's `retention`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Retention {
    #[default]
    Standard,
    /// Engine logs in memory, no core dumps, LMCache never started.
    Zero,
}

impl Retention {
    pub fn is_standard(&self) -> bool {
        *self == Self::Standard
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Zero => "zero",
        }
    }
}

/// ADR 0033 §9: a host's `lmcache`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HostLmcache {
    #[default]
    Auto,
    Off,
}

impl HostLmcache {
    pub fn is_auto(&self) -> bool {
        *self == Self::Auto
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Off => "off",
        }
    }
}

/// ADR 0033 §14: why LMCache is off for a deployment or a launch. The code
/// is what status, `engine list`, the `lmcache_off` event and the
/// `lmcache_unavailable:<reason>` refusal carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LmcacheOffReason {
    /// The deployment's `lmcache: off`.
    Disabled,
    /// The host's `lmcache: off`.
    HostOff,
    /// llama.cpp, TensorFold or a custom engine.
    EngineUnsupported,
    /// vLLM before 0.32.0 or SGLang before 0.5.22.
    EngineVersionUnsupported,
    /// The registered environment holds no LMCache package.
    NotInstalled,
    /// LMCache is in the environment but was installed after `engine add`.
    NotRegistered,
    /// LMCache below 0.5.6, or not 0.5.x.
    VersionUnsupported,
    /// The launch-time check by shape failed.
    CheckFailed,
    /// The deployment has a `topology`.
    GroupUnsupported,
    /// A parking residency on a combination not qualified for it.
    ParkUnqualified,
    /// An approved non-LMCache `--kv-transfer-config` is in `extra_args`.
    KvTransferConfigSet,
    /// The host's or the deployment's `retention: zero`.
    RetentionZero,
    /// The host predates ADR 0033.
    HostCapabilityMissing,
    /// The server did not answer `GET /status` within the start bound.
    ServerStartFailed,
    /// The server listened off loopback.
    ListenerExposed,
    /// The engine did not start with the connector.
    EngineStartFailed,
}

impl LmcacheOffReason {
    pub const ALL: [Self; 16] = [
        Self::Disabled,
        Self::HostOff,
        Self::EngineUnsupported,
        Self::EngineVersionUnsupported,
        Self::NotInstalled,
        Self::NotRegistered,
        Self::VersionUnsupported,
        Self::CheckFailed,
        Self::GroupUnsupported,
        Self::ParkUnqualified,
        Self::KvTransferConfigSet,
        Self::RetentionZero,
        Self::HostCapabilityMissing,
        Self::ServerStartFailed,
        Self::ListenerExposed,
        Self::EngineStartFailed,
    ];

    pub fn code(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::HostOff => "host_off",
            Self::EngineUnsupported => "engine_unsupported",
            Self::EngineVersionUnsupported => "engine_version_unsupported",
            Self::NotInstalled => "not_installed",
            Self::NotRegistered => "not_registered",
            Self::VersionUnsupported => "version_unsupported",
            Self::CheckFailed => "check_failed",
            Self::GroupUnsupported => "group_unsupported",
            Self::ParkUnqualified => "park_unqualified",
            Self::KvTransferConfigSet => "kv_transfer_config_set",
            Self::RetentionZero => "retention_zero",
            Self::HostCapabilityMissing => "host_capability_missing",
            Self::ServerStartFailed => "server_start_failed",
            Self::ListenerExposed => "listener_exposed",
            Self::EngineStartFailed => "engine_start_failed",
        }
    }

    pub fn parse(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|reason| reason.code() == code)
    }

    /// Whether the reason is only found when a launch runs (ADR 0033 §9):
    /// under `required` it fails the launch rather than refusing placement.
    pub fn found_at_launch(self) -> bool {
        matches!(
            self,
            Self::CheckFailed
                | Self::ServerStartFailed
                | Self::ListenerExposed
                | Self::EngineStartFailed
        )
    }

    /// The refusal a `lmcache: required` deployment gets for this reason
    /// (ADR 0033 §14).
    pub fn required_refusal(self) -> String {
        match self {
            Self::RetentionZero => "retention_zero:lmcache".into(),
            Self::HostCapabilityMissing => "host_capability_missing:lmcache".into(),
            other => format!("lmcache_unavailable:{}", other.code()),
        }
    }

    /// The default sentence status shows after "LMCache off: " when the
    /// reason carries no detail of its own.
    pub fn sentence(self) -> &'static str {
        match self {
            Self::Disabled => "turned off by the deployment",
            Self::HostOff => "turned off on this host",
            Self::EngineUnsupported => "this engine has no LMCache integration",
            Self::EngineVersionUnsupported => "this engine version does not support LMCache",
            Self::NotInstalled => "LMCache is not installed in the engine's environment",
            Self::NotRegistered => {
                "LMCache was installed after the engine was registered; register the engine again"
            }
            Self::VersionUnsupported => "LMCache 0.5.6 or a later 0.5.x release is needed",
            Self::CheckFailed => "the launch-time check of the LMCache installation failed",
            Self::GroupUnsupported => "a deployment with a topology does not use LMCache",
            Self::ParkUnqualified => "not qualified with this parking residency",
            Self::KvTransferConfigSet => "the deployment sets its own --kv-transfer-config",
            Self::RetentionZero => "zero data retention",
            Self::HostCapabilityMissing => "the host predates LMCache support",
            Self::ServerStartFailed => "the LMCache server did not start",
            Self::ListenerExposed => "the LMCache server listened off loopback",
            Self::EngineStartFailed => "the engine did not start with LMCache",
        }
    }
}

impl std::fmt::Display for LmcacheOffReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

/// ADR 0033 §6: the L1 pool of an instance, by default.
pub const DEFAULT_POOL_GIB: u64 = 4;
pub const GIB: u64 = 1 << 30;

/// ADR 0033 §1: the security floor (CVE-2026-105192 is fixed in 0.5.6) and
/// the minor line accepted on a passing launch-time check.
pub const LMCACHE_FLOOR: (u64, u64, u64) = (0, 5, 6);

/// ADR 0033 §2: the first engine versions with LMCache, and the qualified ones.
pub const VLLM_FLOOR: (u64, u64, u64) = (0, 32, 0);
pub const SGLANG_FLOOR: (u64, u64, u64) = (0, 5, 22);

/// The leading `major.minor.patch` of a version string; a pre-release or
/// local suffix after the patch is ignored, a missing patch is refused.
pub fn release(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.trim().splitn(3, '.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let rest = parts.next()?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let patch = digits.parse().ok()?;
    Some((major, minor, patch))
}

/// ADR 0033 §1: LMCache 0.5.6 or a later 0.5.x release.
pub fn lmcache_version_supported(version: &str) -> bool {
    release(version).is_some_and(|(major, minor, patch)| {
        (major, minor) == (LMCACHE_FLOOR.0, LMCACHE_FLOOR.1) && patch >= LMCACHE_FLOOR.2
    })
}

/// ADR 0033 §7: one combination qualified live for LMCache beside a parking
/// residency (LC4): the exact registered engine version, the checkpoint's
/// `model_type`, the GPU's product name as the host's inventory reports it,
/// and the residency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QualifiedParking {
    pub engine: &'static str,
    pub engine_version: &'static str,
    pub model_type: &'static str,
    pub gpu: &'static str,
    pub residency: &'static str,
}

/// ADR 0033 §7: the released table. It starts with the combinations LC4
/// passes; nothing else is in it.
pub const QUALIFIED_PARKING: &[QualifiedParking] = &[];

/// Whether a parking residency may use LMCache on this combination.
pub fn parking_qualified(
    engine: &str,
    engine_version: &str,
    model_type: Option<&str>,
    gpu: Option<&str>,
    residency: &str,
) -> bool {
    QUALIFIED_PARKING.iter().any(|row| {
        row.engine == engine
            && row.engine_version == engine_version
            && Some(row.model_type) == model_type
            && Some(row.gpu) == gpu
            && row.residency == residency
    })
}

/// ADR 0033 §6, measured by live row LC1 on host A. LC1_PENDING: these three
/// figures are replaced by the measured defaults before the change merges
/// (the largest measured overhead beyond the pool plus 25 %, rounded up to a
/// whole 256 MiB; the largest per-process GPU memory chosen the same way; the
/// slowest start to `/status` times 3, rounded up to whole 10 s, at least 30 s).
pub const OVERHEAD_ALLOWANCE_BYTES: u64 = 2 * GIB; // LC1_PENDING
pub const DEVICE_ALLOWANCE_BYTES: u64 = 1 << 30; // LC1_PENDING
pub const SERVER_START_BOUND_MS: u64 = 60_000; // LC1_PENDING

#[cfg(test)]
mod tests {
    use super::*;

    // T45 (ADR 0033 §14): every reason has one code and parses back.
    #[test]
    fn reasons_round_trip() {
        for reason in LmcacheOffReason::ALL {
            assert_eq!(LmcacheOffReason::parse(reason.code()), Some(reason));
            assert_eq!(
                serde_json::to_value(reason).unwrap(),
                serde_json::json!(reason.code())
            );
        }
        assert_eq!(
            LmcacheOffReason::HostOff.required_refusal(),
            "lmcache_unavailable:host_off"
        );
        assert_eq!(
            LmcacheOffReason::RetentionZero.required_refusal(),
            "retention_zero:lmcache"
        );
        assert_eq!(
            LmcacheOffReason::HostCapabilityMissing.required_refusal(),
            "host_capability_missing:lmcache"
        );
    }

    // T45 (ADR 0033 §1): 0.5.6 and a later 0.5.x pass; below 0.5.6 and another
    // minor line do not.
    #[test]
    fn lmcache_versions() {
        assert!(lmcache_version_supported("0.5.6"));
        assert!(lmcache_version_supported("0.5.7"));
        assert!(lmcache_version_supported("0.5.10.post1"));
        assert!(!lmcache_version_supported("0.5.5"));
        assert!(!lmcache_version_supported("0.6.0"));
        assert!(!lmcache_version_supported("1.5.6"));
        assert!(!lmcache_version_supported("0.5"));
        assert_eq!(release("0.32.0rc1"), Some((0, 32, 0)));
    }
}
