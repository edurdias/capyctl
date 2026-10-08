//! Host memory observation (SPEC §7.2, T26): `/proc/meminfo`, bounded by the
//! process's cgroup v2 memory limits.
//!
//! In a container, or a service unit with `MemoryMax=`, the kernel enforces the
//! cgroup's `memory.max` long before `MemTotal` is reached, and its OOM killer
//! acts on the cgroup. So the capacity is `min(MemTotal, memory.max)` and the
//! availability `min(MemAvailable, memory.max − usage)`, for the tightest of the
//! process's cgroup and every ancestor (each one's limit applies). Usage is
//! `memory.current` less the cgroup's `inactive_file` pages: the kernel reclaims
//! that page cache before it OOM-kills, as `MemAvailable` counts reclaimable
//! cache on the whole host, and a model's weights read into the page cache would
//! otherwise read as memory in use.
//!
//! cgroup v1 limits are not read. Under v1 (or a hybrid hierarchy whose memory
//! controller is v1) the reading is `/proc/meminfo` alone, and its source says
//! so. Every reading names the source that bounded it ([`MemorySource`]).

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use capyctl_domain::resources::MemoryObservation;

/// Where the unified (v2) cgroup hierarchy is mounted.
pub const CGROUP_ROOT: &str = "/sys/fs/cgroup";
/// The most one cgroup or `/proc` file may hold; more is not a reading.
const MAX_FILE: u64 = 65_536;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostMemorySample {
    pub memory: MemoryObservation,
    pub swap_used_bytes: i64,
    /// What bounded `memory`.
    pub source: MemorySource,
}

/// SPEC §7.2: which source bounded a host memory reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemorySource {
    /// `/proc/meminfo`: no cgroup v2 limit is tighter (or none is set).
    Meminfo,
    /// The `memory.max` of this cgroup v2 cgroup (its path under the hierarchy
    /// root) bounded the capacity, or else the availability.
    CgroupV2 { cgroup: String },
    /// `/proc/meminfo` alone: the memory controller is on cgroup v1, whose
    /// limits are not read.
    CgroupV1Unread,
    /// `/proc/meminfo` alone: the process is in a cgroup v2 cgroup whose limits
    /// could not be read (the hierarchy is not mounted at [`CGROUP_ROOT`], the
    /// cgroup lies outside this cgroup namespace, or a file is malformed).
    CgroupV2Unreadable,
}

impl MemorySource {
    /// The bounded token published with a domain observation (`memory_source`).
    pub fn token(&self) -> String {
        match self {
            Self::Meminfo => "meminfo".into(),
            Self::CgroupV2 { cgroup } => format!("cgroup_v2:{cgroup}"),
            Self::CgroupV1Unread => "meminfo:cgroup_v1_unread".into(),
            Self::CgroupV2Unreadable => "meminfo:cgroup_v2_unreadable".into(),
        }
    }

    /// One sentence for the role log.
    pub fn describe(&self) -> String {
        match self {
            Self::Meminfo => "Host memory is read from /proc/meminfo; no cgroup v2 memory limit is tighter.".into(),
            Self::CgroupV2 { cgroup } => format!(
                "Host memory is bounded by the cgroup v2 memory.max of {}.",
                Path::new(CGROUP_ROOT)
                    .join(cgroup.trim_start_matches('/'))
                    .display()
            ),
            Self::CgroupV1Unread => "Host memory is read from /proc/meminfo; cgroup v1 memory limits are not read, so a v1 container limit is not honoured.".into(),
            Self::CgroupV2Unreadable => format!(
                "Host memory is read from /proc/meminfo; this process's cgroup v2 limits could not be read under {CGROUP_ROOT}."
            ),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MemoryReadError {
    #[error("invalid or incomplete host memory observation")]
    Invalid,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub fn parse_meminfo(input: &str, sampled_at_ms: i64) -> Result<HostMemorySample, MemoryReadError> {
    if sampled_at_ms < 0 || input.len() > 65_536 {
        return Err(MemoryReadError::Invalid);
    }
    let mut fields = BTreeMap::new();
    for line in input.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if !matches!(name, "MemTotal" | "MemAvailable" | "SwapTotal" | "SwapFree") {
            continue;
        }
        let words: Vec<_> = value.split_whitespace().collect();
        if words.len() != 2 || words[1] != "kB" {
            return Err(MemoryReadError::Invalid);
        }
        let kib: i64 = words[0].parse().map_err(|_| MemoryReadError::Invalid)?;
        if kib < 0 {
            return Err(MemoryReadError::Invalid);
        }
        let bytes = kib.checked_mul(1024).ok_or(MemoryReadError::Invalid)?;
        if fields.insert(name, bytes).is_some() {
            return Err(MemoryReadError::Invalid);
        }
    }
    let get = |name| fields.get(name).copied().ok_or(MemoryReadError::Invalid);
    let total = get("MemTotal")?;
    let available = get("MemAvailable")?;
    let swap_total = get("SwapTotal")?;
    let swap_free = get("SwapFree")?;
    if total == 0 || available > total || swap_free > swap_total {
        return Err(MemoryReadError::Invalid);
    }
    Ok(HostMemorySample {
        memory: MemoryObservation {
            domain: "system".into(),
            capacity_bytes: total,
            available_bytes: available,
            sampled_at_ms,
        },
        swap_used_bytes: swap_total - swap_free,
        source: MemorySource::Meminfo,
    })
}

/// Test-only, never a user setting: the environment variable the `capyctl`
/// integration tests set on every process they spawn, so a role observes a
/// stated host memory (`<capacity_bytes>:<available_bytes>`, no swap) instead
/// of `/proc/meminfo`. SPEC §7: standalone derives its limits from observed
/// capacity and every role admits against observed free memory, so a suite
/// reading the real machine failed every deploy on a machine with little
/// memory free. Only debug builds (the test profile) read it; a release build
/// compiles the read out and ignores the variable. A malformed value is an
/// invalid observation, never a fallback to the real one.
pub const TEST_PINNED_HOST_MEMORY_ENV: &str = "CAPYCTL_TEST_PINNED_HOST_MEMORY";

/// Which cgroups `/proc/self/cgroup` places this process in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CgroupMembership {
    /// The unified (v2) cgroup path, from the `0::<path>` line.
    pub unified: Option<String>,
    /// A v1 hierarchy carries the memory controller.
    pub v1_memory: bool,
}

/// Parse `/proc/self/cgroup` (`hierarchy:controllers:path` per line).
pub fn parse_proc_cgroup(raw: &str) -> Result<CgroupMembership, MemoryReadError> {
    let mut membership = CgroupMembership {
        unified: None,
        v1_memory: false,
    };
    for line in raw.lines().filter(|line| !line.is_empty()) {
        let mut fields = line.splitn(3, ':');
        let (Some(hierarchy), Some(controllers), Some(path)) =
            (fields.next(), fields.next(), fields.next())
        else {
            return Err(MemoryReadError::Invalid);
        };
        if hierarchy == "0" && controllers.is_empty() {
            if membership.unified.replace(path.to_owned()).is_some() {
                return Err(MemoryReadError::Invalid);
            }
        } else if controllers.split(',').any(|c| c == "memory") {
            membership.v1_memory = true;
        }
    }
    Ok(membership)
}

/// `memory.max`: `max` is no limit.
pub fn parse_memory_max(raw: &str) -> Result<Option<i64>, MemoryReadError> {
    match raw.trim() {
        "max" => Ok(None),
        value => parse_bytes(value).map(Some),
    }
}

fn parse_bytes(value: &str) -> Result<i64, MemoryReadError> {
    value
        .trim()
        .parse::<i64>()
        .ok()
        .filter(|bytes| *bytes >= 0)
        .ok_or(MemoryReadError::Invalid)
}

/// `inactive_file` from `memory.stat`, or `None` when it is not listed.
pub fn parse_inactive_file(raw: &str) -> Result<Option<i64>, MemoryReadError> {
    raw.lines()
        .find_map(|line| line.strip_prefix("inactive_file "))
        .map(parse_bytes)
        .transpose()
}

/// One cgroup on the way from the process's cgroup to the hierarchy root that
/// limits memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CgroupLimit {
    /// The path under the hierarchy root.
    pub cgroup: String,
    pub max_bytes: i64,
    pub current_bytes: i64,
    pub inactive_file_bytes: i64,
}

impl CgroupLimit {
    /// What may still be charged here before the cgroup's OOM killer acts,
    /// crediting its reclaimable inactive page cache.
    fn headroom(&self) -> i64 {
        let used = self
            .current_bytes
            .saturating_sub(self.inactive_file_bytes)
            .max(0);
        self.max_bytes.saturating_sub(used).max(0)
    }
}

/// The limits on `cgroup` and every ancestor under `root`, deepest first. A
/// cgroup without `memory.max` (no memory controller there, or the root) sets
/// none. `Err` when the cgroup cannot be located or a file cannot be read.
pub fn read_cgroup_limits(root: &Path, cgroup: &str) -> Result<Vec<CgroupLimit>, MemoryReadError> {
    // A cgroup outside this cgroup namespace reads as a path with `..`; its
    // limits are not visible from here.
    let relative = cgroup.strip_prefix('/').ok_or(MemoryReadError::Invalid)?;
    let components: Vec<&str> = relative.split('/').filter(|c| !c.is_empty()).collect();
    if components.iter().any(|c| matches!(*c, "." | "..")) {
        return Err(MemoryReadError::Invalid);
    }
    let deepest: PathBuf = components.iter().collect();
    if !root.join(&deepest).is_dir() {
        return Err(MemoryReadError::Invalid);
    }
    let mut limits = Vec::new();
    for depth in (0..=components.len()).rev() {
        let dir = root.join(components[..depth].iter().collect::<PathBuf>());
        let max = match read_small(&dir.join("memory.max")) {
            Ok(raw) => parse_memory_max(&raw)?,
            Err(MemoryReadError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                continue
            }
            Err(error) => return Err(error),
        };
        let Some(max_bytes) = max else {
            continue;
        };
        let current_bytes = parse_bytes(&read_small(&dir.join("memory.current"))?)?;
        let inactive_file_bytes =
            parse_inactive_file(&read_small(&dir.join("memory.stat"))?)?.unwrap_or(0);
        limits.push(CgroupLimit {
            cgroup: format!("/{}", components[..depth].join("/")),
            max_bytes,
            current_bytes,
            inactive_file_bytes,
        });
    }
    Ok(limits)
}

/// Bound a `/proc/meminfo` reading by the cgroup limits that apply (deepest
/// first, as [`read_cgroup_limits`] lists them).
pub fn bound_by_limits(mut host: HostMemorySample, limits: &[CgroupLimit]) -> HostMemorySample {
    let mut capacity_by = None;
    let mut available_by = None;
    for limit in limits {
        if limit.max_bytes < host.memory.capacity_bytes {
            host.memory.capacity_bytes = limit.max_bytes;
            capacity_by = Some(limit.cgroup.clone());
        }
        let headroom = limit.headroom();
        if headroom < host.memory.available_bytes {
            host.memory.available_bytes = headroom;
            available_by = Some(limit.cgroup.clone());
        }
    }
    host.memory.available_bytes = host.memory.available_bytes.min(host.memory.capacity_bytes);
    host.source = match capacity_by.or(available_by) {
        Some(cgroup) => MemorySource::CgroupV2 { cgroup },
        None => MemorySource::Meminfo,
    };
    host
}

/// Bound a `/proc/meminfo` reading by this process's cgroups, given its
/// `/proc/self/cgroup` (`None` when the kernel has no cgroups) and the
/// hierarchy `root`. A memory limit of zero is no usable reading.
pub fn bound_by_cgroup(
    host: HostMemorySample,
    proc_self_cgroup: Option<&str>,
    root: &Path,
) -> Result<HostMemorySample, MemoryReadError> {
    let Some(raw) = proc_self_cgroup else {
        return Ok(host);
    };
    let Ok(membership) = parse_proc_cgroup(raw) else {
        return Ok(HostMemorySample {
            source: MemorySource::CgroupV2Unreadable,
            ..host
        });
    };
    let bounded = if membership.v1_memory {
        HostMemorySample {
            source: MemorySource::CgroupV1Unread,
            ..host
        }
    } else if let Some(cgroup) = membership.unified {
        match read_cgroup_limits(root, &cgroup) {
            Ok(limits) => bound_by_limits(host, &limits),
            Err(_) => HostMemorySample {
                source: MemorySource::CgroupV2Unreadable,
                ..host
            },
        }
    } else {
        host
    };
    if bounded.memory.capacity_bytes == 0 {
        return Err(MemoryReadError::Invalid);
    }
    Ok(bounded)
}

fn read_small(path: &Path) -> Result<String, MemoryReadError> {
    let mut input = String::new();
    std::fs::File::open(path)?
        .take(MAX_FILE + 1)
        .read_to_string(&mut input)?;
    if input.len() as u64 > MAX_FILE {
        return Err(MemoryReadError::Invalid);
    }
    Ok(input)
}

pub fn read_host_memory() -> Result<HostMemorySample, MemoryReadError> {
    // Timestamp before reading: a delayed read cannot make old data look newer.
    let sampled_at_ms = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| MemoryReadError::Invalid)?
            .as_millis(),
    )
    .map_err(|_| MemoryReadError::Invalid)?;
    #[cfg(debug_assertions)]
    if let Some(pinned) = std::env::var_os(TEST_PINNED_HOST_MEMORY_ENV) {
        return pinned_host_memory(
            pinned.to_str().ok_or(MemoryReadError::Invalid)?,
            sampled_at_ms,
        );
    }
    let mut input = String::new();
    std::fs::File::open("/proc/meminfo")?
        .take(65_537)
        .read_to_string(&mut input)?;
    let host = parse_meminfo(&input, sampled_at_ms)?;
    let membership = match read_small(Path::new("/proc/self/cgroup")) {
        Ok(raw) => Some(raw),
        Err(MemoryReadError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => {
            return Ok(HostMemorySample {
                source: MemorySource::CgroupV2Unreadable,
                ..host
            })
        }
    };
    bound_by_cgroup(host, membership.as_deref(), Path::new(CGROUP_ROOT))
}

/// The reading [`TEST_PINNED_HOST_MEMORY_ENV`] states, checked by the same
/// parser as a real `/proc/meminfo` (whole KiB, available within capacity).
#[cfg(debug_assertions)]
fn pinned_host_memory(
    pinned: &str,
    sampled_at_ms: i64,
) -> Result<HostMemorySample, MemoryReadError> {
    let (capacity, available) = pinned.split_once(':').ok_or(MemoryReadError::Invalid)?;
    let kib = |bytes: &str| -> Result<i64, MemoryReadError> {
        let bytes: i64 = bytes.parse().map_err(|_| MemoryReadError::Invalid)?;
        if bytes % 1024 != 0 {
            return Err(MemoryReadError::Invalid);
        }
        Ok(bytes / 1024)
    };
    parse_meminfo(
        &format!(
            "MemTotal: {} kB\nMemAvailable: {} kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n",
            kib(capacity)?,
            kib(available)?
        ),
        sampled_at_ms,
    )
}

#[cfg(test)]
mod tests;
