//! ADR 0017 (owner decision 2026-09-24): the host/server version skew policy.
//!
//! SPEC §13.1: incompatible peers are refused. Within protocol version 2 the
//! compatibility of two builds is judged from their SemVer release versions:
//!
//! - Patch, pre-release and build-metadata differences on the same
//!   `major.minor` line are fully compatible.
//! - A host one minor release behind the server (N-1, same major) is
//!   supported, with an upgrade recommended.
//! - A host older than N-1, on another major, or whose version is missing or
//!   not strict SemVer is connected drain-only: the server may still stop,
//!   drain, revoke, terminate, probe and inspect what it owns there, but never
//!   places, starts, wakes, parks, digests or materializes anything on it.
//! - A host newer than the server (any minor or major ahead) is refused: the
//!   server cannot know what the newer host expects. Upgrade the server first.
//!
//! Release rule (0.x included): a change that affects the protocol or durable
//! state ships only in a minor (or major) release. A patch release never
//! changes the protocol, so the patch level plays no part in the policy.
use std::cmp::Ordering;

/// This build's release version (the workspace's Cargo package version). The
/// agent reports it in `Connect.binary_version`; the server judges hosts
/// against it.
pub const BINARY_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The longest version string accepted (and echoed in a reason).
pub const MAX_VERSION_LEN: usize = 128;

/// A strict SemVer 2.0 version. Build metadata is kept for display only; it
/// never takes part in compatibility or ordering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub pre: Vec<String>,
    pub build: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("not a strict semantic version")]
pub struct VersionError;

fn numeric(part: &str) -> Result<u64, VersionError> {
    if part.is_empty()
        || !part.bytes().all(|b| b.is_ascii_digit())
        || (part.len() > 1 && part.starts_with('0'))
    {
        return Err(VersionError);
    }
    part.parse().map_err(|_| VersionError)
}

fn identifiers(text: &str, prerelease: bool) -> Result<Vec<String>, VersionError> {
    text.split('.')
        .map(|id| {
            if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                return Err(VersionError);
            }
            // SemVer §9: numeric pre-release identifiers have no leading zeros.
            if prerelease && id.bytes().all(|b| b.is_ascii_digit()) {
                numeric(id)?;
            }
            Ok(id.to_owned())
        })
        .collect()
}

impl Version {
    /// Strict SemVer 2.0: `MAJOR.MINOR.PATCH[-PRERELEASE][+BUILD]`, no
    /// leading `v`, no leading zeros, no surrounding whitespace.
    pub fn parse(text: &str) -> Result<Self, VersionError> {
        if text.is_empty() || text.len() > MAX_VERSION_LEN {
            return Err(VersionError);
        }
        let (rest, build) = match text.split_once('+') {
            Some((rest, build)) => (rest, identifiers(build, false)?),
            None => (text, Vec::new()),
        };
        let (core, pre) = match rest.split_once('-') {
            Some((core, pre)) => (core, identifiers(pre, true)?),
            None => (rest, Vec::new()),
        };
        let mut parts = core.split('.');
        let (Some(major), Some(minor), Some(patch), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(VersionError);
        };
        Ok(Self {
            major: numeric(major)?,
            minor: numeric(minor)?,
            patch: numeric(patch)?,
            pre,
            build,
        })
    }

    /// SemVer precedence (§11): build metadata is ignored.
    pub fn precedence(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    for (a, b) in self.pre.iter().zip(&other.pre) {
                        let order = match (a.parse::<u64>(), b.parse::<u64>()) {
                            (Ok(a), Ok(b)) => a.cmp(&b),
                            (Ok(_), Err(_)) => Ordering::Less,
                            (Err(_), Ok(_)) => Ordering::Greater,
                            (Err(_), Err(_)) => a.cmp(b),
                        };
                        if order != Ordering::Equal {
                            return order;
                        }
                    }
                    self.pre.len().cmp(&other.pre.len())
                }
            })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            write!(f, "-{}", self.pre.join("."))?;
        }
        if !self.build.is_empty() {
            write!(f, "+{}", self.build.join("."))?;
        }
        Ok(())
    }
}

/// The compatibility state of one host against this server.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Compatibility {
    /// Same `major.minor` line.
    Supported,
    /// One minor release behind (N-1): fully supported, upgrade recommended.
    UpgradeRecommended,
    /// Older than N-1, another major, or an unreadable version: drain-only.
    UpgradeRequired,
    /// Newer than the server: the session is refused.
    Refused,
}

impl Compatibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::UpgradeRecommended => "upgrade_recommended",
            Self::UpgradeRequired => "upgrade_required",
            Self::Refused => "refused",
        }
    }
    pub fn parse(text: &str) -> Option<Self> {
        [
            Self::Supported,
            Self::UpgradeRecommended,
            Self::UpgradeRequired,
            Self::Refused,
        ]
        .into_iter()
        .find(|state| state.as_str() == text)
    }
    /// Drain-only: existing work may be stopped, probed and inspected; no new
    /// placement, start, wake, park, digest or materialization.
    pub fn drain_only(self) -> bool {
        matches!(self, Self::UpgradeRequired)
    }
}

/// The policy's verdict on one host, with the reason status shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assessment {
    pub state: Compatibility,
    /// Empty when supported on the same line; otherwise one line for the
    /// operator. It quotes the host's version only when that version parsed.
    pub reason: String,
}

/// ADR 0017: judge a host's reported version against the server's.
pub fn assess(host: &str, server: &str) -> Assessment {
    let verdict = |state, reason: String| Assessment { state, reason };
    let Ok(server_version) = Version::parse(server) else {
        // Never expected: the server's own version is its Cargo version.
        return verdict(
            Compatibility::UpgradeRequired,
            "the server's own version is not strict semver; hosts are drain-only".into(),
        );
    };
    if host.is_empty() {
        return verdict(
            Compatibility::UpgradeRequired,
            format!(
                "the host reports no version (it predates the version skew policy); \
                 it is drain-only until it is upgraded to {}.{}",
                server_version.major, server_version.minor
            ),
        );
    }
    let Ok(host_version) = Version::parse(host) else {
        return verdict(
            Compatibility::UpgradeRequired,
            format!(
                "the host's version is not strict semver; it is drain-only until it is upgraded to {}.{}",
                server_version.major, server_version.minor
            ),
        );
    };
    let (h, s) = (&host_version, &server_version);
    if (h.major, h.minor) > (s.major, s.minor) {
        return verdict(
            Compatibility::Refused,
            format!("host {h} is newer than server {s}; upgrade the server first"),
        );
    }
    if h.major != s.major {
        return verdict(
            Compatibility::UpgradeRequired,
            format!("host {h} is on another major release than server {s}; it is drain-only until it is upgraded"),
        );
    }
    match s.minor - h.minor {
        0 => verdict(Compatibility::Supported, String::new()),
        1 => verdict(
            Compatibility::UpgradeRecommended,
            format!("host {h} is one minor release behind server {s}; upgrade the host"),
        ),
        _ => verdict(
            Compatibility::UpgradeRequired,
            format!("host {h} is more than one minor release behind server {s}; it is drain-only until it is upgraded"),
        ),
    }
}

/// The fixed prefix of a newer host's session refusal, which the agent
/// recognizes to log "upgrade the server first" instead of a generic refusal.
pub const NEWER_HOST_REFUSAL: &str = "host_version_newer_than_server";
