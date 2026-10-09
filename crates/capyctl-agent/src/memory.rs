use std::collections::BTreeMap;
use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};

use capyctl_domain::resources::MemoryObservation;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostMemorySample {
    pub memory: MemoryObservation,
    pub swap_used_bytes: i64,
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
    parse_meminfo(&input, sampled_at_ms)
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
mod tests {
    use super::*;

    #[test]
    fn a_pinned_reading_states_capacity_and_availability() {
        let sample = pinned_host_memory(&format!("{}:{}", 32_i64 << 30, 8_i64 << 30), 7).unwrap();
        assert_eq!(sample.memory.capacity_bytes, 32 << 30);
        assert_eq!(sample.memory.available_bytes, 8 << 30);
        assert_eq!(sample.memory.sampled_at_ms, 7);
        assert_eq!(sample.swap_used_bytes, 0);
    }

    #[test]
    fn a_malformed_pinned_reading_is_invalid() {
        for pinned in [
            "",
            "1024",
            "1024:",
            "x:1024",
            "1024:2048",
            "1000:1000",
            "-1024:0",
        ] {
            assert!(
                pinned_host_memory(pinned, 7).is_err(),
                "accepted {pinned:?}"
            );
        }
    }
}
