//! ADR 0033: LMCache, a private KV cache server per deployment instance.
//!
//! The vocabulary every role shares: the deployment's `lmcache` and
//! `retention` settings, the host's `lmcache` and `retention` settings, the
//! closed set of reasons LMCache is off, the supported engine versions, the
//! qualified parking combinations and the figures LC1 measures.

use crate::effective::{DeploymentApi, Residency};
use crate::engine_policy::Engine;
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

/// ADR 0033 §13 (ADR 0017): the host capability a host implementing this
/// ADR declares. `capyctl_protocol::capabilities::LMCACHE` is this name.
pub const CAPABILITY: &str = "lmcache";
/// ADR 0033 §§10, 13: the host capability that says the host honours a
/// deployment's `retention: zero`. `capyctl_protocol::capabilities::RETENTION`
/// is this name.
pub const RETENTION_CAPABILITY: &str = "retention";

/// ADR 0033 §§9, 14: `lmcache: required` on a host whose `lmcache` is `off`.
pub const REFUSED_HOST_OFF: &str = "lmcache_unavailable:host_off";
/// ADR 0033 §§10, 14: `lmcache: required` where the host or the deployment
/// has `retention: zero`.
pub const REFUSED_RETENTION_ZERO: &str = "retention_zero:lmcache";
/// ADR 0033 §§13, 14: `lmcache: required` on a host that did not declare
/// `lmcache`.
pub const MISSING_LMCACHE: &str = "host_capability_missing:lmcache";
/// ADR 0033 §§10, 13, 14: `retention: zero` on a host that did not declare
/// `retention`, which would not honour it.
pub const MISSING_RETENTION: &str = "host_capability_missing:retention";
/// Every closed code [`placement_refusal`] returns.
pub const PLACEMENT_REFUSALS: [&str; 4] = [
    REFUSED_HOST_OFF,
    REFUSED_RETENTION_ZERO,
    MISSING_LMCACHE,
    MISSING_RETENTION,
];

/// ADR 0033 §§9, 10: a deployment document's declared `lmcache` and
/// `retention` (at its root), `auto` and `standard` when not stated. Read
/// defensively: a validated document never holds another value, and a
/// value this build does not know grants nothing beyond the default.
pub fn deployment_settings(document: &serde_json::Value) -> (LmcacheMode, Retention) {
    (
        LmcacheMode::deserialize(&document["lmcache"]).unwrap_or_default(),
        Retention::deserialize(&document["retention"]).unwrap_or_default(),
    )
}

/// ADR 0033 §§9, 10, 13: why a host may not take a deployment with these
/// declared settings, before anything is sent to it, or `None` when it may.
/// `declared` answers whether the host declared a capability (ADR 0017).
///
/// It is [`LmcachePlan::refusal`] for what the host alone decides (its
/// `lmcache`, its `retention`, its capabilities), judged on a profile that
/// could run LMCache: what the deployment and its profile decide is
/// resolution's ([`deployment_refusal`]). The answer is one of
/// [`PLACEMENT_REFUSALS`].
pub fn placement_refusal(
    mode: LmcacheMode,
    retention: Retention,
    host_lmcache: HostLmcache,
    host_retention: Retention,
    declared: impl Fn(&str) -> bool,
) -> Option<&'static str> {
    let registration = LmcacheRegistration::new(
        "0.5.6",
        "sha256:0000000000000000000000000000000000000000000000000000000000000000",
    );
    let inputs = PlanInputs {
        mode,
        retention,
        host_lmcache,
        host_retention,
        host_declares_lmcache: declared(CAPABILITY),
        host_declares_retention: declared(RETENTION_CAPABILITY),
        registration: Some(&registration),
        ..PlanInputs::for_profile(Engine::Vllm, "0.32.0", Residency::RestartOnly)
    };
    let code = plan(&inputs).refusal()?;
    PLACEMENT_REFUSALS.into_iter().find(|known| *known == code)
}

#[cfg(test)]
mod placement_tests {
    use super::*;
    use serde_json::json;

    // T45 T36 (ADR 0033 §§9, 10, 13): every placement refusal, in the order
    // the reasons apply, and none for what a host can run as declared.
    #[test]
    fn placement_refuses_required_and_zero_retention_where_they_cannot_run() {
        use HostLmcache as H;
        use LmcacheMode as M;
        use Retention as R;
        let every = |_: &str| true;
        let none = |_: &str| false;
        let without_lmcache = |name: &str| name != CAPABILITY;
        let cases = [
            (
                M::Required,
                R::Standard,
                H::Auto,
                R::Standard,
                &every as &dyn Fn(&str) -> bool,
                None,
            ),
            (
                M::Required,
                R::Zero,
                H::Auto,
                R::Standard,
                &every,
                Some(REFUSED_RETENTION_ZERO),
            ),
            (
                M::Required,
                R::Standard,
                H::Auto,
                R::Zero,
                &every,
                Some(REFUSED_RETENTION_ZERO),
            ),
            (
                M::Required,
                R::Standard,
                H::Off,
                R::Zero,
                &every,
                Some(REFUSED_RETENTION_ZERO),
            ),
            (
                M::Required,
                R::Standard,
                H::Off,
                R::Standard,
                &every,
                Some(REFUSED_HOST_OFF),
            ),
            (
                M::Required,
                R::Standard,
                H::Off,
                R::Standard,
                &none,
                Some(REFUSED_HOST_OFF),
            ),
            (
                M::Required,
                R::Standard,
                H::Auto,
                R::Standard,
                &without_lmcache,
                Some(MISSING_LMCACHE),
            ),
            (
                M::Required,
                R::Standard,
                H::Auto,
                R::Standard,
                &none,
                Some(MISSING_LMCACHE),
            ),
            (M::Auto, R::Standard, H::Off, R::Zero, &none, None),
            (M::Off, R::Standard, H::Auto, R::Standard, &none, None),
            (
                M::Auto,
                R::Zero,
                H::Auto,
                R::Standard,
                &none,
                Some(MISSING_RETENTION),
            ),
            (M::Off, R::Zero, H::Off, R::Standard, &without_lmcache, None),
            (M::Auto, R::Zero, H::Auto, R::Standard, &every, None),
        ];
        for (mode, retention, host_lmcache, host_retention, declared, expected) in cases {
            assert_eq!(
                placement_refusal(mode, retention, host_lmcache, host_retention, declared),
                expected,
                "{mode:?} {retention:?} on {host_lmcache:?} {host_retention:?}"
            );
        }
        // The codes are the ones the reasons give under `required` (§14).
        assert_eq!(
            REFUSED_HOST_OFF,
            LmcacheOffReason::HostOff.required_refusal()
        );
        assert_eq!(
            REFUSED_RETENTION_ZERO,
            LmcacheOffReason::RetentionZero.required_refusal()
        );
        assert_eq!(
            MISSING_LMCACHE,
            LmcacheOffReason::HostCapabilityMissing.required_refusal()
        );
    }

    // T45 (ADR 0033 §§9, 10): a deployment document's settings read from its
    // root, with the defaults when absent or not a value this build knows.
    #[test]
    fn deployment_settings_are_read_from_the_document_root_with_defaults() {
        assert_eq!(
            deployment_settings(&json!({"lmcache": "required", "retention": "zero"})),
            (LmcacheMode::Required, Retention::Zero)
        );
        assert_eq!(
            deployment_settings(&json!({"model": {}})),
            (LmcacheMode::Auto, Retention::Standard)
        );
        assert_eq!(
            deployment_settings(&json!({"lmcache": "sometimes", "retention": 3})),
            (LmcacheMode::Auto, Retention::Standard)
        );
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
    parking_qualified_in(
        QUALIFIED_PARKING,
        engine,
        engine_version,
        model_type,
        gpu,
        residency,
    )
}

/// [`parking_qualified`] against `table` (the released table, or a test's).
pub fn parking_qualified_in(
    table: &[QualifiedParking],
    engine: &str,
    engine_version: &str,
    model_type: Option<&str>,
    gpu: Option<&str>,
    residency: &str,
) -> bool {
    table.iter().any(|row| {
        row.engine == engine
            && row.engine_version == engine_version
            && Some(row.model_type) == model_type
            && Some(row.gpu) == gpu
            && row.residency == residency
    })
}

/// ADR 0033 §6: the defaults live row LC1 set on host A (GB10, 2026-10-22;
/// LMCache 0.5.6, Qwen3-4B chat on vLLM 0.32.0 and SGLang 0.5.22, pools of 4
/// and 8 GiB; 5 engine runs and 20 bare starts).
///
/// Overhead allowance: the largest resident memory LC1 measured beyond the
/// pool, 824.3 MiB, plus 25 % (1030.4 MiB), rounded up to a whole 256 MiB.
pub const OVERHEAD_ALLOWANCE_BYTES: u64 = 1280 << 20;
/// Device allowance: the largest per-process GPU memory LC1 measured for the
/// server while its engine ran, 408 MiB, plus 25 % (510 MiB), rounded up to a
/// whole 256 MiB. On unified memory it is outside the resident set and inside
/// the same pool (measured), so it is charged there beside the overhead.
pub const DEVICE_ALLOWANCE_BYTES: u64 = 512 << 20;
/// Start bound: the slowest start to `GET /status` LC1 measured, 4.80 s,
/// times 3 (14.4 s), rounded up to whole 10 s (20 s), and never below 30 s.
pub const SERVER_START_BOUND_MS: u64 = 30_000;

/// ADR 0033 §1, contract decision 3: the LMCache package `capyctl engine add`
/// found in the registered environment (`lmcache-*.dist-info`), recorded on
/// the profile as `lmcache: {version, digest}`. Absent: not registered, so a
/// package installed after registration is never loaded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LmcacheRegistration {
    /// The package's version as its `METADATA` states it.
    pub version: String,
    /// ADR 0008: `sha256:<64 lowercase hex>` of the package's files.
    pub digest: String,
}

impl LmcacheRegistration {
    /// What `capyctl engine add` records when it writes the profile.
    pub fn new(version: impl Into<String>, digest: impl Into<String>) -> Self {
        Self {
            version: version.into(),
            digest: digest.into(),
        }
    }

    /// The shape a profile may carry: a version of at most 64 printable
    /// characters and an ADR 0008 digest. Whether the version is supported
    /// is the plan's question (`version_unsupported`), not the profile's.
    pub fn check(&self) -> Result<(), &'static str> {
        let version_ok = !self.version.is_empty()
            && self.version.len() <= 64
            && self.version.bytes().all(|b| b.is_ascii_graphic());
        if !version_ok {
            return Err("lmcache.version must be a version of at most 64 printable characters");
        }
        let digest_ok = self.digest.strip_prefix("sha256:").is_some_and(|hex| {
            hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        });
        if !digest_ok {
            return Err("lmcache.digest must be `sha256:` and 64 lowercase hexadecimal digits");
        }
        Ok(())
    }
}

/// ADR 0033 §6: a deployment's `lmcache_pool`, in bytes: a size in the
/// configuration's byte syntax that is a whole number of GiB, at least 1 GiB.
pub fn parse_pool(text: &str) -> Result<u64, &'static str> {
    const REFUSAL: &str =
        "lmcache_pool must be a whole number of GiB, at least 1GiB (for example `8GiB`)";
    let bytes = crate::effective::parse_bytes(text).map_err(|_| REFUSAL)?;
    u64::try_from(bytes)
        .ok()
        .filter(|bytes| *bytes >= GIB && bytes % GIB == 0)
        .ok_or(REFUSAL)
}

/// Why LMCache is off, with the sentence status shows after "LMCache off: ".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LmcacheOff {
    pub reason: LmcacheOffReason,
    pub detail: String,
}

impl LmcacheOff {
    /// `reason` with its default sentence.
    pub fn new(reason: LmcacheOffReason) -> Self {
        Self::with_detail(reason, reason.sentence())
    }

    pub fn with_detail(reason: LmcacheOffReason, detail: impl Into<String>) -> Self {
        Self {
            reason,
            detail: detail.into(),
        }
    }

    /// The status text (contract decision 4): `LMCache off: <detail>`.
    pub fn message(&self) -> String {
        format!("LMCache off: {}", self.detail)
    }
}

/// Whether a launch of the plan starts LMCache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LmcacheState {
    On,
    Off(LmcacheOff),
}

/// ADR 0033 §2: the engine family and registered build (`build_fingerprint`,
/// which `engine add` writes as the reported version) gate: llama.cpp,
/// TensorFold and custom engines have no integration; vLLM before 0.32.0 and
/// SGLang before 0.5.22 (a pre-release of either is older) do not support
/// it, and neither does a build that names no version. `None` when the
/// engine and version support LMCache.
pub fn engine_gate(engine: Engine, build_fingerprint: &str) -> Option<LmcacheOff> {
    use crate::decisions::EngineVersion;
    let unsupported = |detail: &str| {
        Some(LmcacheOff::with_detail(
            LmcacheOffReason::EngineUnsupported,
            detail,
        ))
    };
    let (name, floor) = match engine {
        Engine::Llamacpp => return unsupported("llama.cpp has no LMCache integration"),
        Engine::Tensorfold => return unsupported("TensorFold has no LMCache integration"),
        Engine::Custom => {
            return unsupported("CapyCTL does not render options for a custom engine")
        }
        Engine::Vllm => ("vLLM", VLLM_FLOOR),
        Engine::Sglang => ("SGLang", SGLANG_FLOOR),
    };
    let floor = EngineVersion::new(floor.0, floor.1, floor.2);
    let supported =
        EngineVersion::parse(build_fingerprint).is_some_and(|version| version.at_least(&floor));
    (!supported).then(|| {
        LmcacheOff::with_detail(
            LmcacheOffReason::EngineVersionUnsupported,
            format!("LMCache needs {name} {floor} or newer"),
        )
    })
}

/// The upstream report of SGLang failing with LMCache while its radix cache
/// is disabled (LC7).
pub const SGLANG_RADIX_ISSUE: &str = "https://github.com/sgl-project/sglang/issues/45929";

/// ADR 0033 amendment of 2026-10-22 (owner decision, LC7): SGLang with its
/// radix cache disabled fails with LMCache at its first idle check
/// ([`SGLANG_RADIX_ISSUE`]). A decision deployment (SGLang disables the
/// radix cache for the embedding models that serve decisions) and one whose
/// arguments state `--disable-radix-cache` are planned without LMCache,
/// `engine_unsupported`, so no start fails on it. Other models SGLang
/// disables the radix cache for are left to the `engine_start_failed`
/// fallback.
fn sglang_radix_gate(inputs: &PlanInputs<'_>) -> Option<LmcacheOff> {
    if inputs.engine != Engine::Sglang {
        return None;
    }
    let why = if inputs.api == DeploymentApi::Decisions {
        "decision deployments on SGLang run without LMCache"
    } else if inputs.radix_cache_disabled {
        "SGLang with --disable-radix-cache runs without LMCache"
    } else {
        return None;
    };
    Some(LmcacheOff::with_detail(
        LmcacheOffReason::EngineUnsupported,
        format!(
            "{why}: SGLang fails with LMCache while its radix cache is disabled ({SGLANG_RADIX_ISSUE})"
        ),
    ))
}

/// Contract decision 2: everything [`plan`] decides from. Every role fills it
/// from what it holds; [`PlanInputs::for_profile`] gives the host-neutral
/// start resolution uses.
#[derive(Debug, Clone, Copy)]
pub struct PlanInputs<'a> {
    /// The deployment's declared `lmcache`.
    pub mode: LmcacheMode,
    /// The deployment's declared `lmcache_pool`, in bytes; `None` is the
    /// default 4 GiB.
    pub pool_bytes: Option<u64>,
    /// The deployment's `retention`.
    pub retention: Retention,
    /// The host's `lmcache` and `retention` (its published document).
    pub host_lmcache: HostLmcache,
    pub host_retention: Retention,
    /// ADR 0033 §13: whether the host declared the `lmcache` capability,
    /// and the `retention` one.
    pub host_declares_lmcache: bool,
    pub host_declares_retention: bool,
    pub engine: Engine,
    /// The registered build (`build_fingerprint`), the engine's version.
    pub engine_version: &'a str,
    /// ADR 0033 §2: decision deployments use LMCache like chat ones on vLLM;
    /// on SGLang they run without it (amendment of 2026-10-22, LC7).
    pub api: DeploymentApi,
    /// The deployment declares a `topology`.
    pub topology: bool,
    pub residency: Residency,
    /// The checkpoint's `config.json` `model_type` and the GPU's product
    /// name as the host's inventory reports it, for the qualified table.
    pub model_type: Option<&'a str>,
    pub gpu: Option<&'a str>,
    /// The profile's `lmcache` registration.
    pub registration: Option<&'a LmcacheRegistration>,
    /// An approved non-LMCache `--kv-transfer-config` is in `extra_args`
    /// (`engine_policy::kv_transfer_config_set`).
    pub kv_transfer_config_set: bool,
    /// SGLang's `--disable-radix-cache` is in the profile's arguments or
    /// `extra_args` (`engine_policy::radix_cache_disabled`): ADR 0033
    /// amendment of 2026-10-22 (LC7).
    pub radix_cache_disabled: bool,
    /// ADR 0033 §8: a sticky `engine_start_failed` fallback is recorded for
    /// this revision on this profile.
    pub engine_start_failed: bool,
    /// The launch's GPU is discrete (its own device domain): the server's
    /// CUDA context is charged there.
    pub discrete_gpu: bool,
}

impl<'a> PlanInputs<'a> {
    /// An `auto` deployment with the default pool and standard retention on
    /// this profile and residency, on a host that leaves LMCache on, keeps
    /// standard retention and declares both capabilities; no topology, no
    /// registration, no `--kv-transfer-config` or `--disable-radix-cache`,
    /// no fallback, no model type or GPU known.
    pub fn for_profile(engine: Engine, engine_version: &'a str, residency: Residency) -> Self {
        Self {
            mode: LmcacheMode::Auto,
            pool_bytes: None,
            retention: Retention::Standard,
            host_lmcache: HostLmcache::Auto,
            host_retention: Retention::Standard,
            host_declares_lmcache: true,
            host_declares_retention: true,
            engine,
            engine_version,
            api: DeploymentApi::Chat,
            topology: false,
            residency,
            model_type: None,
            gpu: None,
            registration: None,
            kv_transfer_config_set: false,
            radix_cache_disabled: false,
            engine_start_failed: false,
            discrete_gpu: false,
        }
    }
}

/// What [`plan`] decided for one deployment on one host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LmcachePlan {
    pub mode: LmcacheMode,
    /// The pool in bytes: declared, else 4 GiB.
    pub pool_bytes: u64,
    /// ADR 0033 §10: the host's or the deployment's `retention: zero`
    /// (engine output in memory, `RLIMIT_CORE` 0, LMCache never started).
    pub retention_zero: bool,
    pub state: LmcacheState,
    /// ADR 0033 §10, §13: the deployment states `retention: zero` and the
    /// host did not declare the `retention` capability, so it would not
    /// honour it.
    pub retention_unhonoured: bool,
    pub discrete_gpu: bool,
}

/// ADR 0033 §6: what an instance using LMCache reserves beside its engine.
/// [`apply_charge`] lays it over a launch's phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LmcacheCharge {
    /// The L1 pool, in the host's system domain (on a unified-memory machine,
    /// the domain the GPU uses); it also counts against `host_kv_limit`.
    pub pool_bytes: i64,
    /// The server's footprint beyond its pool, in the same domain: the
    /// allowance, or the deployment's measured figure.
    pub overhead_bytes: i64,
    /// The server's per-process GPU memory: the allowance, or the measured
    /// figure. On a discrete GPU it is charged to the device domain; on a
    /// unified-memory machine it lies outside the server's resident set and
    /// inside the one shared pool (measured by LC1), so it is charged there.
    pub device_bytes: i64,
    /// The engine KV cache the server keeps mapped once its engine has exited
    /// (measured by LC1: a stopped engine's whole KV cache stays held until
    /// the next engine registers or the server stops). Charged only in the
    /// parked phase of a `restart_only` park, in the device domain on a
    /// discrete GPU, the shared pool elsewhere. The caller sets it from the
    /// engine's KV cache; [`LmcachePlan::charge`] leaves it zero.
    pub held_kv_bytes: i64,
}

impl LmcachePlan {
    pub fn is_on(&self) -> bool {
        self.state == LmcacheState::On
    }

    /// Why LMCache is off, `None` when it is on.
    pub fn off(&self) -> Option<&LmcacheOff> {
        match &self.state {
            LmcacheState::On => None,
            LmcacheState::Off(off) => Some(off),
        }
    }

    /// ADR 0033 §6: the charge before the deployment's first measurement:
    /// the pool plus the overhead allowance, and the device allowance on a
    /// discrete GPU. `None` when LMCache is off.
    pub fn charge(&self) -> Option<LmcacheCharge> {
        self.charge_measured(None, None)
    }

    /// [`LmcachePlan::charge`] with the overhead beyond the pool and the
    /// device memory measured on earlier launches in place of the
    /// allowances where known. A figure of zero is no reading (a launch
    /// whose GPU memory nothing could read records none), so it keeps its
    /// allowance: a measurement never lowers a charge to what no source saw.
    pub fn charge_measured(
        &self,
        overhead_bytes: Option<i64>,
        device_bytes: Option<i64>,
    ) -> Option<LmcacheCharge> {
        let figure = |bytes: u64| i64::try_from(bytes).unwrap_or(i64::MAX);
        let read = |bytes: Option<i64>| bytes.filter(|bytes| *bytes > 0);
        self.is_on().then(|| LmcacheCharge {
            pool_bytes: figure(self.pool_bytes),
            overhead_bytes: read(overhead_bytes).unwrap_or(figure(OVERHEAD_ALLOWANCE_BYTES)),
            device_bytes: read(device_bytes).unwrap_or(figure(DEVICE_ALLOWANCE_BYTES)),
            held_kv_bytes: 0,
        })
    }

    /// ADR 0033 §9, §10, §14: the refusal placement gives before anything
    /// starts: `host_capability_missing:retention` for a deployment
    /// `retention: zero` the host would not honour; under `lmcache:
    /// required`, the code of a reason known before launch. `None` when the
    /// deployment may be placed (a launch-time reason fails the launch).
    pub fn refusal(&self) -> Option<String> {
        if self.retention_unhonoured {
            return Some("host_capability_missing:retention".into());
        }
        match (&self.state, self.mode) {
            (LmcacheState::Off(off), LmcacheMode::Required) if !off.reason.found_at_launch() => {
                Some(off.reason.required_refusal())
            }
            _ => None,
        }
    }
}

/// ADR 0033 §6: the host domains a launch's LMCache charge lands in: the
/// system domain (on a unified-memory machine, the one shared domain), and
/// on a discrete GPU the device domain the server's CUDA context uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChargeDomains {
    pub system: String,
    pub device: Option<String>,
}

/// The [`ChargeDomains`] of a launch on `host` holding `devices` (its
/// `selected_devices`): a selected GPU whose domain is a `device` domain is
/// discrete, charged there, with the pool in the host's `distinct` system
/// domain; otherwise the selected GPU's own (unified) domain takes it all.
/// Without a selected GPU, the host's one non-device domain. `None` when
/// no domain can be named.
pub fn charge_domains(
    host: &crate::effective::HostPolicy,
    devices: &[crate::effective::DeviceClaim],
) -> Option<ChargeDomains> {
    use crate::effective::DomainMemory;
    let memory = |domain: &str| host.domains.get(domain).map(|policy| policy.memory);
    let system = || {
        let mut found = host
            .domains
            .iter()
            .filter(|(_, policy)| policy.memory != DomainMemory::Device)
            .map(|(name, _)| name.clone());
        let first = found.next();
        first.filter(|_| found.next().is_none())
    };
    let selected = devices
        .iter()
        .find_map(|claim| host.devices.get(&claim.id))
        .map(|device| device.domain.clone());
    match selected {
        Some(domain) if memory(&domain) == Some(DomainMemory::Device) => Some(ChargeDomains {
            system: system()?,
            device: Some(domain),
        }),
        Some(domain) if memory(&domain).is_some() => Some(ChargeDomains {
            system: domain,
            device: None,
        }),
        _ => system().map(|system| ChargeDomains {
            system,
            device: None,
        }),
    }
}

/// One phase's allocations, in either footprint shape a caller holds.
pub trait ChargeAllocations {
    /// Add `bytes` and `host_kv_bytes` to `domain`'s allocation, opening one
    /// when the phase has none there.
    fn add_charge(&mut self, domain: &str, bytes: i64, host_kv_bytes: i64);
    fn clear_allocations(&mut self);
}

impl ChargeAllocations for Vec<crate::effective::Allocation> {
    fn add_charge(&mut self, domain: &str, bytes: i64, host_kv_bytes: i64) {
        match self
            .iter_mut()
            .find(|allocation| allocation.domain == domain)
        {
            Some(allocation) => {
                allocation.bytes = allocation.bytes.saturating_add(bytes);
                allocation.host_kv_bytes = allocation.host_kv_bytes.saturating_add(host_kv_bytes);
            }
            None => self.push(crate::effective::Allocation {
                domain: domain.to_owned(),
                bytes,
                host_kv_bytes,
            }),
        }
    }

    fn clear_allocations(&mut self) {
        self.clear();
    }
}

impl ChargeAllocations for Vec<capyctl_domain::resources::Allocation> {
    fn add_charge(&mut self, domain: &str, bytes: i64, host_kv_bytes: i64) {
        match self
            .iter_mut()
            .find(|allocation| allocation.domain == domain)
        {
            Some(allocation) => {
                allocation.bytes = allocation.bytes.saturating_add(bytes);
                allocation.host_kv_bytes = allocation.host_kv_bytes.saturating_add(host_kv_bytes);
            }
            None => self.push(capyctl_domain::resources::Allocation {
                domain: domain.to_owned(),
                bytes,
                host_kv_bytes,
            }),
        }
    }

    fn clear_allocations(&mut self) {
        self.clear();
    }
}

/// A launch's five phases, in either footprint shape: the resolved
/// revision's (`EffectiveDeployment::resources`) or the ledger's
/// (`capyctl_domain::resources::RecipeFootprints`).
pub trait ChargeFootprints {
    /// Each phase's allocations, and whether it is the parked phase.
    fn phases(&mut self) -> [(&mut dyn ChargeAllocations, bool); 5];
}

impl ChargeFootprints for crate::effective::RecipeFootprints {
    fn phases(&mut self) -> [(&mut dyn ChargeAllocations, bool); 5] {
        [
            (&mut self.cold.allocations, false),
            (&mut self.ready.allocations, false),
            (&mut self.parking.allocations, false),
            (&mut self.parked.allocations, true),
            (&mut self.wake.allocations, false),
        ]
    }
}

impl ChargeFootprints for capyctl_domain::resources::RecipeFootprints {
    fn phases(&mut self) -> [(&mut dyn ChargeAllocations, bool); 5] {
        [
            (&mut self.cold.allocations, false),
            (&mut self.ready.allocations, false),
            (&mut self.parking.allocations, false),
            (&mut self.parked.allocations, true),
            (&mut self.wake.allocations, false),
        ]
    }
}

/// ADR 0033 §6, §7 (lifecycle design §7): lay an instance's LMCache charge
/// over its phases, the one overlay the store and the host both apply so
/// they reserve the same bytes. Every phase gains the pool plus the
/// overhead in the system domain, with the pool added to `host_kv_bytes`,
/// and on a discrete GPU the device figure in the device domain: the server
/// runs from Initialize to the instance's stop, parked or not. With
/// `restart_park` (a `restart_only` park, which stops the engine and keeps
/// the server) the parked phase is the charge alone.
pub fn apply_charge<F: ChargeFootprints + ?Sized>(
    resources: &mut F,
    charge: &LmcacheCharge,
    restart_park: bool,
    domains: &ChargeDomains,
) {
    let system = charge.pool_bytes.saturating_add(charge.overhead_bytes);
    for (allocations, parked) in resources.phases() {
        if parked && restart_park {
            allocations.clear_allocations();
        }
        allocations.add_charge(&domains.system, system, charge.pool_bytes);
        // LC1: the server's GPU memory, and while its engine is stopped the
        // engine KV it keeps mapped, land in the device domain on a discrete
        // GPU and in the one shared pool on unified memory.
        let gpu = charge
            .device_bytes
            .saturating_add(if parked && restart_park {
                charge.held_kv_bytes
            } else {
                0
            });
        if gpu > 0 {
            let domain = domains.device.as_deref().unwrap_or(&domains.system);
            allocations.add_charge(domain, gpu, 0);
        }
    }
}

/// ADR 0033 §§2, 7–11, 13, contract decision 2: whether LMCache runs for this
/// deployment on this host, against the released qualified table.
pub fn plan(inputs: &PlanInputs<'_>) -> LmcachePlan {
    plan_with_table(inputs, QUALIFIED_PARKING)
}

/// [`plan`] against `table` in place of [`QUALIFIED_PARKING`].
pub fn plan_with_table(inputs: &PlanInputs<'_>, table: &[QualifiedParking]) -> LmcachePlan {
    let retention_zero =
        inputs.retention == Retention::Zero || inputs.host_retention == Retention::Zero;
    LmcachePlan {
        mode: inputs.mode,
        pool_bytes: inputs.pool_bytes.unwrap_or(DEFAULT_POOL_GIB * GIB),
        retention_zero,
        state: match off_reason(inputs, table) {
            Some(off) => LmcacheState::Off(off),
            None => LmcacheState::On,
        },
        retention_unhonoured: inputs.retention == Retention::Zero
            && !inputs.host_declares_retention,
        discrete_gpu: inputs.discrete_gpu,
    }
}

/// The first applicable reason, in contract decision 2's order.
fn off_reason(inputs: &PlanInputs<'_>, table: &[QualifiedParking]) -> Option<LmcacheOff> {
    use LmcacheOffReason as R;
    if inputs.mode == LmcacheMode::Off {
        return Some(LmcacheOff::new(R::Disabled));
    }
    if inputs.host_retention == Retention::Zero {
        return Some(LmcacheOff::with_detail(
            R::RetentionZero,
            "zero data retention on this host",
        ));
    }
    if inputs.retention == Retention::Zero {
        return Some(LmcacheOff::with_detail(
            R::RetentionZero,
            "the deployment keeps zero data retention",
        ));
    }
    if inputs.host_lmcache == HostLmcache::Off {
        return Some(LmcacheOff::new(R::HostOff));
    }
    if !inputs.host_declares_lmcache {
        return Some(LmcacheOff::new(R::HostCapabilityMissing));
    }
    if let Some(off) = engine_gate(inputs.engine, inputs.engine_version) {
        return Some(off);
    }
    if let Some(off) = sglang_radix_gate(inputs) {
        return Some(off);
    }
    if inputs.topology {
        return Some(LmcacheOff::new(R::GroupUnsupported));
    }
    if inputs.kv_transfer_config_set {
        return Some(LmcacheOff::new(R::KvTransferConfigSet));
    }
    let Some(registration) = inputs.registration else {
        return Some(LmcacheOff::new(R::NotInstalled));
    };
    if !lmcache_version_supported(&registration.version) {
        return Some(LmcacheOff::with_detail(
            R::VersionUnsupported,
            format!(
                "LMCache {} is registered; LMCache 0.5.6 or a later 0.5.x release is needed",
                registration.version
            ),
        ));
    }
    if inputs.residency.parks()
        && !parking_qualified_in(
            table,
            inputs.engine.name(),
            inputs.engine_version,
            inputs.model_type,
            inputs.gpu,
            residency_name(inputs.residency),
        )
    {
        return Some(LmcacheOff::with_detail(
            R::ParkUnqualified,
            format!(
                "not qualified with {} park on {} {} / {} / {}",
                match inputs.residency {
                    Residency::HostBacked => "host_backed",
                    _ => "deep",
                },
                engine_display(inputs.engine),
                inputs.engine_version,
                inputs.model_type.unwrap_or("an unknown model type"),
                inputs.gpu.unwrap_or("an unknown GPU"),
            ),
        ));
    }
    if inputs.engine_start_failed {
        return Some(LmcacheOff::new(R::EngineStartFailed));
    }
    None
}

/// ADR 0033 §9, §14: the refusal of a `lmcache: required` deployment that
/// its declaration and its profile alone decide, so resolution gives it
/// before any host is asked: the engine, its version, a topology, a
/// `--kv-transfer-config`, the registration, and a parking residency no row
/// of `table` qualifies on this engine version for any model or GPU. The
/// host's settings and capabilities, the model type and the GPU are
/// placement's; `inputs`' host fields are ignored.
pub fn deployment_refusal(inputs: &PlanInputs<'_>, table: &[QualifiedParking]) -> Option<String> {
    use LmcacheOffReason as R;
    if inputs.mode != LmcacheMode::Required {
        return None;
    }
    let neutral = PlanInputs {
        host_lmcache: HostLmcache::Auto,
        host_retention: Retention::Standard,
        host_declares_lmcache: true,
        host_declares_retention: true,
        engine_start_failed: false,
        ..*inputs
    };
    let off = off_reason(&neutral, table)?;
    let known = match off.reason {
        R::EngineUnsupported
        | R::EngineVersionUnsupported
        | R::GroupUnsupported
        | R::KvTransferConfigSet
        | R::NotInstalled
        | R::VersionUnsupported => true,
        R::ParkUnqualified => !table.iter().any(|row| {
            row.engine == inputs.engine.name()
                && row.engine_version == inputs.engine_version
                && row.residency == residency_name(inputs.residency)
        }),
        _ => false,
    };
    known.then(|| format!("{}: {}", off.reason.required_refusal(), off.detail))
}

fn residency_name(residency: Residency) -> &'static str {
    match residency {
        Residency::RestartOnly => "restart_only",
        Residency::HostBacked => "host_backed",
        Residency::Deep => "deep",
    }
}

fn engine_display(engine: Engine) -> &'static str {
    match engine {
        Engine::Vllm => "vLLM",
        Engine::Sglang => "SGLang",
        Engine::Tensorfold => "TensorFold",
        Engine::Llamacpp => "llama.cpp",
        Engine::Custom => "custom engine",
    }
}

/// ADR 0033 §9, §10: a host's `lmcache` and `retention` as a document states
/// them at its root (a host role document, the `host` block of a standalone
/// one, the document a host publishes). An absent or unreadable value reads
/// as the default: a listing never refuses.
pub fn host_settings(document: &serde_json::Value) -> (HostLmcache, Retention) {
    let lmcache = serde_json::from_value(document["lmcache"].clone()).unwrap_or_default();
    let retention = serde_json::from_value(document["retention"].clone()).unwrap_or_default();
    (lmcache, retention)
}

/// ADR 0033 §9: why a profile cannot use LMCache before its environment is
/// read, in the order every role applies (contract 2): `retention_zero`,
/// `host_off`, then the engine gate.
pub fn profile_gate(
    host: (HostLmcache, Retention),
    engine: Engine,
    build_fingerprint: &str,
) -> Option<LmcacheOff> {
    if host.1 == Retention::Zero {
        return Some(LmcacheOff::new(LmcacheOffReason::RetentionZero));
    }
    if host.0 == HostLmcache::Off {
        return Some(LmcacheOff::new(LmcacheOffReason::HostOff));
    }
    engine_gate(engine, build_fingerprint)
}

/// ADR 0033 §9: what `engine list` shows for a profile from what is known
/// without its environment's files: [`profile_gate`], then the profile's
/// registration (`not_installed` without one, `version_unsupported` for a
/// registered release outside 0.5.x from 0.5.6). `Ok` is the registered
/// version. A role that can read the environment shows its launch-time
/// check instead of the registration.
pub fn listed(
    host: (HostLmcache, Retention),
    engine: Engine,
    build_fingerprint: &str,
    registration: Option<&LmcacheRegistration>,
) -> Result<String, LmcacheOff> {
    if let Some(off) = profile_gate(host, engine, build_fingerprint) {
        return Err(off);
    }
    match registration {
        None => Err(LmcacheOff::new(LmcacheOffReason::NotInstalled)),
        Some(registration) if !lmcache_version_supported(&registration.version) => {
            Err(LmcacheOff::with_detail(
                LmcacheOffReason::VersionUnsupported,
                format!(
                    "LMCache {} is registered; {}",
                    registration.version,
                    LmcacheOffReason::VersionUnsupported.sentence()
                ),
            ))
        }
        Some(registration) => Ok(registration.version.clone()),
    }
}

/// ADR 0033 §9 (contract 8): the `lmcache` field of an `engine list` row:
/// `{"state": "on", "version"}` or `{"state": "off", "reason", "detail"}`.
pub fn listed_view(listed: &Result<String, LmcacheOff>) -> serde_json::Value {
    match listed {
        Ok(version) => serde_json::json!({"state": "on", "version": version}),
        Err(off) => serde_json::json!({
            "state": "off", "reason": off.reason.code(), "detail": off.detail,
        }),
    }
}

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

    const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn registered() -> LmcacheRegistration {
        LmcacheRegistration::new("0.5.6", DIGEST)
    }

    /// vLLM 0.32.0 with LMCache 0.5.6 registered, restart_only, `auto`, on a
    /// host that leaves it on: every reason off.
    fn on<'a>(registration: &'a LmcacheRegistration) -> PlanInputs<'a> {
        PlanInputs {
            registration: Some(registration),
            ..PlanInputs::for_profile(Engine::Vllm, "0.32.0", Residency::RestartOnly)
        }
    }

    fn reason(inputs: &PlanInputs<'_>) -> Option<LmcacheOffReason> {
        plan(inputs).off().map(|off| off.reason)
    }

    /// LC4's kind of row, for the table tests (the released table is empty).
    const QWEN_ON_GB10: QualifiedParking = QualifiedParking {
        engine: "vllm",
        engine_version: "0.32.0",
        model_type: "qwen3",
        gpu: "NVIDIA GB10",
        residency: "deep",
    };

    // T45 (ADR 0033 §2, §9): a supported engine, registered and restarting,
    // is on, chat deployments on SGLang 0.5.22 included, and a newer engine
    // version too.
    #[test]
    fn a_supported_deployment_is_on() {
        let registration = registered();
        assert_eq!(plan(&on(&registration)).state, LmcacheState::On);
        for (engine, version, api) in [
            (Engine::Sglang, "0.5.22", DeploymentApi::Chat),
            (Engine::Sglang, "0.5.23", DeploymentApi::Chat),
            (Engine::Vllm, "0.33.1", DeploymentApi::Chat),
        ] {
            let inputs = PlanInputs {
                engine,
                engine_version: version,
                api,
                ..on(&registration)
            };
            assert!(plan(&inputs).is_on(), "{engine:?} {version} {api:?}");
        }
    }

    // T45 (ADR 0033 amendment of 2026-10-22, LC7): SGLang fails with LMCache
    // while its radix cache is disabled, so a decision deployment on SGLang
    // (any version) and one stating `--disable-radix-cache` are planned off,
    // `engine_unsupported`, with the upstream issue in the detail; `required`
    // is refused for it before any host is asked. A chat deployment on
    // SGLang, and the same flag's absence, leave LMCache on.
    #[test]
    fn sglang_with_its_radix_cache_disabled_is_off() {
        let registration = registered();
        let sglang = PlanInputs {
            engine: Engine::Sglang,
            engine_version: "0.5.22",
            ..on(&registration)
        };
        assert!(plan(&sglang).is_on(), "chat on SGLang");
        for (inputs, starts) in [
            (
                PlanInputs {
                    api: DeploymentApi::Decisions,
                    ..sglang
                },
                "decision deployments on SGLang run without LMCache: ",
            ),
            (
                PlanInputs {
                    api: DeploymentApi::Decisions,
                    engine_version: "0.5.23",
                    ..sglang
                },
                "decision deployments on SGLang run without LMCache: ",
            ),
            (
                PlanInputs {
                    radix_cache_disabled: true,
                    ..sglang
                },
                "SGLang with --disable-radix-cache runs without LMCache: ",
            ),
        ] {
            let planned = plan(&inputs);
            let off = planned.off().expect("off");
            assert_eq!(off.reason, LmcacheOffReason::EngineUnsupported);
            assert!(off.detail.starts_with(starts), "{}", off.detail);
            assert!(off.detail.ends_with(&format!("({SGLANG_RADIX_ISSUE})")));
            assert_eq!(planned.charge(), None);
            let required = PlanInputs {
                mode: LmcacheMode::Required,
                ..inputs
            };
            assert!(deployment_refusal(&required, &[])
                .is_some_and(|code| code.starts_with("lmcache_unavailable:engine_unsupported: ")));
        }
        // The radix gate is SGLang's alone: vLLM is planned as before.
        let vllm = PlanInputs {
            radix_cache_disabled: true,
            ..on(&registration)
        };
        assert!(plan(&vllm).is_on());
        let args = |list: &[&str]| list.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>();
        for flag in [
            &["--disable-radix-cache"][..],
            &["--disable_radix_cache"][..],
            &["--mem-fraction-static", "0.5", "--disable-radix-cache"][..],
        ] {
            assert!(
                crate::engine_policy::radix_cache_disabled(Engine::Sglang, &args(flag)),
                "{flag:?}"
            );
            assert!(!crate::engine_policy::radix_cache_disabled(
                Engine::Vllm,
                &args(flag)
            ));
        }
        assert!(!crate::engine_policy::radix_cache_disabled(
            Engine::Sglang,
            &args(&["--disable-cuda-graph"])
        ));
    }

    // T45 (ADR 0033 §2, contract decision 4): each engine that cannot use
    // LMCache, with its sentence; a pre-release of the floor and a build
    // that names no version are older.
    #[test]
    fn engine_and_version_reasons() {
        let registration = registered();
        for (engine, version, code, detail) in [
            (
                Engine::Vllm,
                "0.31.0",
                LmcacheOffReason::EngineVersionUnsupported,
                "LMCache needs vLLM 0.32.0 or newer",
            ),
            (
                Engine::Vllm,
                "0.32.0rc1",
                LmcacheOffReason::EngineVersionUnsupported,
                "LMCache needs vLLM 0.32.0 or newer",
            ),
            (
                Engine::Vllm,
                "vllm-build-1",
                LmcacheOffReason::EngineVersionUnsupported,
                "LMCache needs vLLM 0.32.0 or newer",
            ),
            (
                Engine::Sglang,
                "0.5.21",
                LmcacheOffReason::EngineVersionUnsupported,
                "LMCache needs SGLang 0.5.22 or newer",
            ),
            (
                Engine::Llamacpp,
                "0.7.0+d812350",
                LmcacheOffReason::EngineUnsupported,
                "llama.cpp has no LMCache integration",
            ),
            (
                Engine::Tensorfold,
                "0.6.6",
                LmcacheOffReason::EngineUnsupported,
                "TensorFold has no LMCache integration",
            ),
            (
                Engine::Custom,
                "basal-2.2.0",
                LmcacheOffReason::EngineUnsupported,
                "CapyCTL does not render options for a custom engine",
            ),
        ] {
            let inputs = PlanInputs {
                engine,
                engine_version: version,
                ..on(&registration)
            };
            let planned = plan(&inputs);
            let off = planned.off().expect("off");
            assert_eq!(
                (off.reason, off.detail.as_str()),
                (code, detail),
                "{version}"
            );
            assert_eq!(off.message(), format!("LMCache off: {detail}"));
            assert_eq!(planned.charge(), None);
            assert_eq!(engine_gate(engine, version).as_ref(), Some(off));
        }
        assert_eq!(engine_gate(Engine::Sglang, "0.5.22"), None);
    }

    // T45 (ADR 0033 §§7–11, §13, contract decision 2): every reason the
    // plan decides, each alone.
    #[test]
    fn every_planned_reason() {
        let registration = registered();
        let old = LmcacheRegistration::new("0.5.5", DIGEST);
        let other_line = LmcacheRegistration::new("0.6.0", DIGEST);
        let base = on(&registration);
        let cases: Vec<(PlanInputs<'_>, LmcacheOffReason, &str)> = vec![
            (
                PlanInputs {
                    mode: LmcacheMode::Off,
                    ..base
                },
                LmcacheOffReason::Disabled,
                "turned off by the deployment",
            ),
            (
                PlanInputs {
                    host_retention: Retention::Zero,
                    ..base
                },
                LmcacheOffReason::RetentionZero,
                "zero data retention on this host",
            ),
            (
                PlanInputs {
                    retention: Retention::Zero,
                    ..base
                },
                LmcacheOffReason::RetentionZero,
                "the deployment keeps zero data retention",
            ),
            (
                PlanInputs {
                    host_lmcache: HostLmcache::Off,
                    ..base
                },
                LmcacheOffReason::HostOff,
                "turned off on this host",
            ),
            (
                PlanInputs {
                    host_declares_lmcache: false,
                    ..base
                },
                LmcacheOffReason::HostCapabilityMissing,
                "the host predates LMCache support",
            ),
            (
                PlanInputs {
                    topology: true,
                    ..base
                },
                LmcacheOffReason::GroupUnsupported,
                "a deployment with a topology does not use LMCache",
            ),
            (
                PlanInputs {
                    kv_transfer_config_set: true,
                    ..base
                },
                LmcacheOffReason::KvTransferConfigSet,
                "the deployment sets its own --kv-transfer-config",
            ),
            (
                PlanInputs {
                    registration: None,
                    ..base
                },
                LmcacheOffReason::NotInstalled,
                "LMCache is not installed in the engine's environment",
            ),
            (
                PlanInputs {
                    registration: Some(&old),
                    ..base
                },
                LmcacheOffReason::VersionUnsupported,
                "LMCache 0.5.5 is registered; LMCache 0.5.6 or a later 0.5.x release is needed",
            ),
            (
                PlanInputs {
                    registration: Some(&other_line),
                    ..base
                },
                LmcacheOffReason::VersionUnsupported,
                "LMCache 0.6.0 is registered; LMCache 0.5.6 or a later 0.5.x release is needed",
            ),
            (
                PlanInputs {
                    residency: Residency::Deep,
                    model_type: Some("qwen3"),
                    gpu: Some("NVIDIA GB10"),
                    ..base
                },
                LmcacheOffReason::ParkUnqualified,
                "not qualified with deep park on vLLM 0.32.0 / qwen3 / NVIDIA GB10",
            ),
            (
                PlanInputs {
                    residency: Residency::HostBacked,
                    ..base
                },
                LmcacheOffReason::ParkUnqualified,
                "not qualified with host_backed park on vLLM 0.32.0 / an unknown model type \
                 / an unknown GPU",
            ),
            (
                PlanInputs {
                    engine_start_failed: true,
                    ..base
                },
                LmcacheOffReason::EngineStartFailed,
                "the engine did not start with LMCache",
            ),
        ];
        for (inputs, code, detail) in cases {
            let planned = plan(&inputs);
            assert_eq!(
                planned.state,
                LmcacheState::Off(LmcacheOff::with_detail(code, detail)),
                "{code}"
            );
        }
    }

    // T45 (contract decision 2): the first applicable reason wins, in the
    // fixed order; removing each in turn reveals the next.
    #[test]
    fn the_first_reason_wins_in_order() {
        let registration = registered();
        let old = LmcacheRegistration::new("0.5.5", DIGEST);
        let mut inputs = PlanInputs {
            mode: LmcacheMode::Off,
            host_retention: Retention::Zero,
            host_lmcache: HostLmcache::Off,
            host_declares_lmcache: false,
            engine: Engine::Vllm,
            engine_version: "0.31.0",
            topology: true,
            kv_transfer_config_set: true,
            registration: None,
            residency: Residency::Deep,
            engine_start_failed: true,
            ..on(&registration)
        };
        let order = [
            LmcacheOffReason::Disabled,
            LmcacheOffReason::RetentionZero,
            LmcacheOffReason::HostOff,
            LmcacheOffReason::HostCapabilityMissing,
            LmcacheOffReason::EngineVersionUnsupported,
            LmcacheOffReason::GroupUnsupported,
            LmcacheOffReason::KvTransferConfigSet,
            LmcacheOffReason::NotInstalled,
            LmcacheOffReason::VersionUnsupported,
            LmcacheOffReason::ParkUnqualified,
            LmcacheOffReason::EngineStartFailed,
        ];
        for expected in order {
            assert_eq!(reason(&inputs), Some(expected));
            match expected {
                LmcacheOffReason::Disabled => inputs.mode = LmcacheMode::Auto,
                LmcacheOffReason::RetentionZero => inputs.host_retention = Retention::Standard,
                LmcacheOffReason::HostOff => inputs.host_lmcache = HostLmcache::Auto,
                LmcacheOffReason::HostCapabilityMissing => inputs.host_declares_lmcache = true,
                LmcacheOffReason::EngineVersionUnsupported => inputs.engine_version = "0.32.0",
                LmcacheOffReason::GroupUnsupported => inputs.topology = false,
                LmcacheOffReason::KvTransferConfigSet => inputs.kv_transfer_config_set = false,
                LmcacheOffReason::NotInstalled => inputs.registration = Some(&old),
                LmcacheOffReason::VersionUnsupported => inputs.registration = Some(&registration),
                LmcacheOffReason::ParkUnqualified => inputs.residency = Residency::RestartOnly,
                _ => inputs.engine_start_failed = false,
            }
        }
        assert_eq!(reason(&inputs), None);
        // The engine family is decided before its version.
        inputs.engine = Engine::Tensorfold;
        inputs.topology = true;
        assert_eq!(reason(&inputs), Some(LmcacheOffReason::EngineUnsupported));
    }

    // T45 (ADR 0033 §7): a parking residency uses LMCache exactly on a
    // qualified combination; restart_only needs none. The released table is
    // empty, so a parking deployment is off with it.
    #[test]
    fn the_qualified_table_hit_and_miss() {
        let registration = registered();
        let deep = PlanInputs {
            residency: Residency::Deep,
            model_type: Some("qwen3"),
            gpu: Some("NVIDIA GB10"),
            ..on(&registration)
        };
        assert!(plan_with_table(&deep, &[QWEN_ON_GB10]).is_on());
        assert_eq!(reason(&deep), Some(LmcacheOffReason::ParkUnqualified));
        assert!(QUALIFIED_PARKING.is_empty());
        for miss in [
            PlanInputs {
                model_type: Some("llama"),
                ..deep
            },
            PlanInputs {
                gpu: Some("NVIDIA RTX 5080 Laptop GPU"),
                ..deep
            },
            PlanInputs {
                engine_version: "0.32.1",
                ..deep
            },
            PlanInputs {
                residency: Residency::HostBacked,
                ..deep
            },
            PlanInputs {
                model_type: None,
                ..deep
            },
        ] {
            assert_eq!(
                plan_with_table(&miss, &[QWEN_ON_GB10])
                    .off()
                    .map(|off| off.reason),
                Some(LmcacheOffReason::ParkUnqualified)
            );
        }
        let restart = PlanInputs {
            residency: Residency::RestartOnly,
            ..deep
        };
        assert!(plan_with_table(&restart, &[]).is_on());
    }

    // T45 T37 (ADR 0033 §9, §10, §13, §14): `required` is refused with its
    // code for a reason known before launch; a launch-time reason is the
    // launch's; `auto` is never refused; an unhonoured deployment retention
    // is refused whatever the mode.
    #[test]
    fn required_refusals() {
        let registration = registered();
        let required = PlanInputs {
            mode: LmcacheMode::Required,
            ..on(&registration)
        };
        assert_eq!(plan(&required).refusal(), None);
        for (inputs, code) in [
            (
                PlanInputs {
                    host_lmcache: HostLmcache::Off,
                    ..required
                },
                "lmcache_unavailable:host_off",
            ),
            (
                PlanInputs {
                    host_retention: Retention::Zero,
                    ..required
                },
                "retention_zero:lmcache",
            ),
            (
                PlanInputs {
                    host_declares_lmcache: false,
                    ..required
                },
                "host_capability_missing:lmcache",
            ),
            (
                PlanInputs {
                    residency: Residency::Deep,
                    ..required
                },
                "lmcache_unavailable:park_unqualified",
            ),
            (
                PlanInputs {
                    engine_version: "0.31.0",
                    ..required
                },
                "lmcache_unavailable:engine_version_unsupported",
            ),
            (
                PlanInputs {
                    registration: None,
                    ..required
                },
                "lmcache_unavailable:not_installed",
            ),
            (
                PlanInputs {
                    retention: Retention::Zero,
                    host_declares_retention: false,
                    ..on(&registration)
                },
                "host_capability_missing:retention",
            ),
        ] {
            assert_eq!(plan(&inputs).refusal().as_deref(), Some(code));
        }
        // A launch-time reason fails the launch instead.
        let sticky = PlanInputs {
            engine_start_failed: true,
            ..required
        };
        assert_eq!(plan(&sticky).refusal(), None);
        // `auto` runs without LMCache instead.
        let auto = PlanInputs {
            host_lmcache: HostLmcache::Off,
            ..on(&registration)
        };
        assert_eq!(plan(&auto).refusal(), None);
    }

    // T45 (ADR 0033 §9, §14): resolution refuses `required` for what the
    // deployment and profile decide, and leaves the host's reasons to
    // placement; a parking residency is refused only when no row could
    // qualify it on any model or GPU.
    #[test]
    fn deployment_refusals_ignore_the_host() {
        let registration = registered();
        let required = PlanInputs {
            mode: LmcacheMode::Required,
            host_lmcache: HostLmcache::Off,
            host_retention: Retention::Zero,
            host_declares_lmcache: false,
            engine_start_failed: true,
            ..on(&registration)
        };
        assert_eq!(deployment_refusal(&required, &[]), None);
        let code = |inputs: PlanInputs<'_>, table: &[QualifiedParking]| {
            deployment_refusal(&inputs, table)
                .map(|text| text.split(": ").next().unwrap().to_owned())
        };
        for (inputs, expected) in [
            (
                PlanInputs {
                    engine: Engine::Llamacpp,
                    ..required
                },
                "lmcache_unavailable:engine_unsupported",
            ),
            (
                PlanInputs {
                    engine_version: "0.31.0",
                    ..required
                },
                "lmcache_unavailable:engine_version_unsupported",
            ),
            (
                PlanInputs {
                    topology: true,
                    ..required
                },
                "lmcache_unavailable:group_unsupported",
            ),
            (
                PlanInputs {
                    kv_transfer_config_set: true,
                    ..required
                },
                "lmcache_unavailable:kv_transfer_config_set",
            ),
            (
                PlanInputs {
                    registration: None,
                    ..required
                },
                "lmcache_unavailable:not_installed",
            ),
            (
                PlanInputs {
                    residency: Residency::Deep,
                    ..required
                },
                "lmcache_unavailable:park_unqualified",
            ),
        ] {
            assert_eq!(code(inputs, &[]).as_deref(), Some(expected));
        }
        let deep = PlanInputs {
            residency: Residency::Deep,
            ..required
        };
        assert_eq!(code(deep, &[QWEN_ON_GB10]), None, "a GPU may qualify it");
        let auto = PlanInputs {
            mode: LmcacheMode::Auto,
            registration: None,
            ..required
        };
        assert_eq!(deployment_refusal(&auto, &[]), None);
    }

    // T45 T25 (ADR 0033 §6, A1): the charge is the pool plus the overhead
    // allowance in the system domain, the pool against host_kv_limit, the
    // device allowance on unified memory and on a discrete GPU alike (LC1);
    // measured figures replace the allowances, but a figure of zero (no
    // reading) never does; nothing is charged while off.
    #[test]
    fn the_charge() {
        let registration = registered();
        let planned = plan(&on(&registration));
        assert_eq!(planned.pool_bytes, 4 * GIB);
        let allowance = |bytes: u64| bytes as i64;
        assert_eq!(
            planned.charge(),
            Some(LmcacheCharge {
                pool_bytes: 4 << 30,
                overhead_bytes: allowance(OVERHEAD_ALLOWANCE_BYTES),
                device_bytes: allowance(DEVICE_ALLOWANCE_BYTES),
                held_kv_bytes: 0,
            })
        );
        let discrete = plan(&PlanInputs {
            pool_bytes: Some(8 * GIB),
            discrete_gpu: true,
            ..on(&registration)
        });
        assert_eq!(
            discrete.charge(),
            Some(LmcacheCharge {
                pool_bytes: 8 << 30,
                overhead_bytes: allowance(OVERHEAD_ALLOWANCE_BYTES),
                device_bytes: allowance(DEVICE_ALLOWANCE_BYTES),
                held_kv_bytes: 0,
            })
        );
        assert_eq!(
            discrete.charge_measured(Some(700 << 20), Some(300 << 20)),
            Some(LmcacheCharge {
                pool_bytes: 8 << 30,
                overhead_bytes: 700 << 20,
                device_bytes: 300 << 20,
                held_kv_bytes: 0,
            })
        );
        // Found live 2026-10-22 (GB10, SGLang): a launch whose server's GPU
        // memory nothing read recorded device 0, and the next launch charged
        // no GPU memory at all. No reading keeps the allowance.
        assert_eq!(
            planned.charge_measured(Some(800 << 20), Some(0)),
            Some(LmcacheCharge {
                pool_bytes: 4 << 30,
                overhead_bytes: 800 << 20,
                device_bytes: allowance(DEVICE_ALLOWANCE_BYTES),
                held_kv_bytes: 0,
            })
        );
        assert_eq!(
            planned.charge_measured(Some(0), Some(400 << 20)),
            Some(LmcacheCharge {
                pool_bytes: 4 << 30,
                overhead_bytes: allowance(OVERHEAD_ALLOWANCE_BYTES),
                device_bytes: 400 << 20,
                held_kv_bytes: 0,
            })
        );
        let off = plan(&PlanInputs {
            host_retention: Retention::Zero,
            ..on(&registration)
        });
        assert!(off.retention_zero);
        assert_eq!(off.charge(), None);
    }

    fn host(
        domains: serde_json::Value,
        devices: serde_json::Value,
    ) -> crate::effective::HostPolicy {
        crate::effective::normalize_host_policy(&serde_json::json!({
            "schema_version": 1, "kind": "host", "name": "lab",
            "hardware_fingerprint": "hw", "environment_fingerprint": "env",
            "model_store": {"path": "/srv/models"},
            "resource_policy": {
                "domains": domains, "devices": devices, "device_sharing": "shared",
                "endpoint_port_range": {"start": 8100, "end": 8199}
            },
            "runtime_profiles": {}
        }))
        .unwrap()
    }

    fn unified_host() -> crate::effective::HostPolicy {
        host(
            serde_json::json!({"unified": {"managed_limit": "96GiB", "free_reserve": "12GiB",
                "host_kv_limit": "16GiB", "memory": "unified"}}),
            serde_json::json!({"gpu0": {"domain": "unified", "sharing": "shared"}}),
        )
    }

    fn discrete_host() -> crate::effective::HostPolicy {
        host(
            serde_json::json!({
                "system": {"managed_limit": "30GiB", "free_reserve": "12GiB", "memory": "distinct"},
                "gpu0": {"managed_limit": "14GiB", "free_reserve": "1GiB", "memory": "device",
                    "device": "gpu0"}
            }),
            serde_json::json!({"gpu0": {"domain": "gpu0", "sharing": "shared"}}),
        )
    }

    fn gpu0() -> Vec<crate::effective::DeviceClaim> {
        vec![crate::effective::DeviceClaim {
            id: "gpu0".into(),
            sharing: crate::effective::Sharing::Shared,
        }]
    }

    fn phase(allocations: &[(&str, i64, i64)]) -> crate::effective::PhaseFootprint {
        crate::effective::PhaseFootprint {
            allocations: allocations
                .iter()
                .map(|(domain, bytes, kv)| crate::effective::Allocation {
                    domain: (*domain).into(),
                    bytes: *bytes,
                    host_kv_bytes: *kv,
                })
                .collect(),
            devices: gpu0(),
        }
    }

    fn every_allocation(
        resources: &crate::effective::RecipeFootprints,
    ) -> impl Iterator<Item = &crate::effective::Allocation> {
        [
            &resources.cold,
            &resources.ready,
            &resources.parking,
            &resources.parked,
            &resources.wake,
        ]
        .into_iter()
        .flat_map(|phase| phase.allocations.iter())
    }

    const G: i64 = 1 << 30;
    const CHARGE: LmcacheCharge = LmcacheCharge {
        pool_bytes: 4 * G,
        overhead_bytes: 2 * G,
        device_bytes: G,
        held_kv_bytes: 2 * G,
    };

    // T45 T24 (ADR 0033 §6, §7, LC1): on unified memory every phase, parked
    // included, gains pool, overhead and the server's GPU memory in the one
    // shared domain with the pool as host KV; a restart_only park is the
    // charge alone plus the engine KV the server keeps mapped.
    #[test]
    fn the_overlay_on_unified_memory() {
        let domains = charge_domains(&unified_host(), &gpu0()).unwrap();
        assert_eq!(
            domains,
            ChargeDomains {
                system: "unified".into(),
                device: None
            }
        );
        let engine = crate::effective::RecipeFootprints {
            cold: phase(&[("unified", 20 * G, G)]),
            ready: phase(&[("unified", 18 * G, G)]),
            parking: phase(&[("unified", 19 * G, G)]),
            parked: phase(&[("unified", 2 * G, 0)]),
            wake: phase(&[("unified", 20 * G, G)]),
        };
        let mut deep = engine.clone();
        apply_charge(&mut deep, &CHARGE, false, &domains);
        assert_eq!(deep.cold.allocations[0].bytes, 27 * G);
        assert_eq!(deep.cold.allocations[0].host_kv_bytes, 5 * G);
        assert_eq!(deep.parked.allocations[0].bytes, 9 * G);
        assert_eq!(deep.parked.allocations[0].host_kv_bytes, 4 * G);
        assert_eq!(deep.wake.allocations.len(), 1);
        let mut restart = engine.clone();
        apply_charge(&mut restart, &CHARGE, true, &domains);
        assert_eq!(restart.ready, deep.ready);
        assert_eq!(
            restart.parked.allocations,
            vec![crate::effective::Allocation {
                domain: "unified".into(),
                bytes: 9 * G,
                host_kv_bytes: 4 * G,
            }]
        );
        assert_eq!(restart.parked.devices, engine.parked.devices);
        for allocation in every_allocation(&deep).chain(every_allocation(&restart)) {
            assert!(
                allocation.host_kv_bytes <= allocation.bytes,
                "{allocation:?}"
            );
        }
    }

    // T45 T24 (ADR 0033 §6, ADR 0019): on a discrete GPU the pool and
    // overhead go to the distinct system domain (opened where a phase had
    // none) and the device figure to the GPU's domain; the ledger's own
    // footprint shape gets the same bytes.
    #[test]
    fn the_overlay_on_a_discrete_gpu() {
        let domains = charge_domains(&discrete_host(), &gpu0()).unwrap();
        assert_eq!(
            domains,
            ChargeDomains {
                system: "system".into(),
                device: Some("gpu0".into())
            }
        );
        let engine = crate::effective::RecipeFootprints {
            cold: phase(&[("gpu0", 12 * G, 0), ("system", 3 * G, 0)]),
            ready: phase(&[("gpu0", 10 * G, 0), ("system", 2 * G, 0)]),
            parking: phase(&[("gpu0", 11 * G, 0), ("system", 9 * G, 0)]),
            parked: phase(&[("system", 8 * G, 0)]),
            wake: phase(&[("gpu0", 12 * G, 0)]),
        };
        let mut charged = engine.clone();
        apply_charge(&mut charged, &CHARGE, false, &domains);
        let find = |phase: &crate::effective::PhaseFootprint, domain: &str| {
            phase
                .allocations
                .iter()
                .find(|a| a.domain == domain)
                .map(|a| (a.bytes, a.host_kv_bytes))
        };
        assert_eq!(find(&charged.cold, "gpu0"), Some((13 * G, 0)));
        assert_eq!(find(&charged.cold, "system"), Some((9 * G, 4 * G)));
        assert_eq!(find(&charged.parked, "system"), Some((14 * G, 4 * G)));
        assert_eq!(find(&charged.parked, "gpu0"), Some((G, 0)));
        assert_eq!(find(&charged.wake, "system"), Some((6 * G, 4 * G)));
        let mut restart = engine.clone();
        apply_charge(&mut restart, &CHARGE, true, &domains);
        assert_eq!(find(&restart.parked, "system"), Some((6 * G, 4 * G)));
        assert_eq!(find(&restart.parked, "gpu0"), Some((3 * G, 0)));
        for allocation in every_allocation(&charged).chain(every_allocation(&restart)) {
            assert!(
                allocation.host_kv_bytes <= allocation.bytes,
                "{allocation:?}"
            );
        }
        // The ledger's shape: the same overlay, the same bytes.
        use capyctl_domain::resources as ledger;
        let to_ledger = |phase: &crate::effective::PhaseFootprint, kind| ledger::PhaseFootprint {
            phase: kind,
            allocations: phase
                .allocations
                .iter()
                .map(|a| ledger::Allocation {
                    domain: a.domain.clone(),
                    bytes: a.bytes,
                    host_kv_bytes: a.host_kv_bytes,
                })
                .collect(),
            devices: Vec::new(),
        };
        let convert = |r: &crate::effective::RecipeFootprints| ledger::RecipeFootprints {
            cold: to_ledger(&r.cold, ledger::ResourcePhase::Cold),
            ready: to_ledger(&r.ready, ledger::ResourcePhase::Ready),
            parking: to_ledger(&r.parking, ledger::ResourcePhase::Parking),
            parked: to_ledger(&r.parked, ledger::ResourcePhase::Parked),
            wake: to_ledger(&r.wake, ledger::ResourcePhase::Wake),
        };
        let mut ledger_side = convert(&engine);
        apply_charge(&mut ledger_side, &CHARGE, true, &domains);
        assert_eq!(ledger_side, convert(&restart));
        // Without a device figure (and outside a restart park) no device
        // allocation is opened.
        let mut no_device = engine.clone();
        let unified_charge = LmcacheCharge {
            device_bytes: 0,
            ..CHARGE
        };
        apply_charge(&mut no_device, &unified_charge, false, &domains);
        assert_eq!(find(&no_device.parked, "gpu0"), None);
    }

    // T45 (ADR 0033 §6): the pool is a whole number of GiB, at least 1 GiB.
    #[test]
    fn pool_sizes() {
        assert_eq!(parse_pool("8GiB"), Ok(8 * GIB));
        assert_eq!(parse_pool("1GiB"), Ok(GIB));
        assert_eq!(parse_pool("2048MiB"), Ok(2 * GIB));
        for bad in [
            "0GiB", "512MiB", "1.5GiB", "1536MiB", "8", "8GB", "-1GiB", "",
        ] {
            assert!(parse_pool(bad).is_err(), "{bad}");
        }
    }

    // T45 (ADR 0033 §1, contract decision 3): a registration names a version
    // and an ADR 0008 digest.
    #[test]
    fn registration_shape() {
        assert_eq!(registered().check(), Ok(()));
        for bad in [
            LmcacheRegistration::new("", DIGEST),
            LmcacheRegistration::new("0.5 6", DIGEST),
            LmcacheRegistration::new("0.5.6", "sha256:abc"),
            LmcacheRegistration::new("0.5.6", DIGEST.replace("sha256", "md5")),
            LmcacheRegistration::new("0.5.6", DIGEST.to_uppercase()),
        ] {
            assert!(bad.check().is_err(), "{bad:?}");
        }
    }
}
