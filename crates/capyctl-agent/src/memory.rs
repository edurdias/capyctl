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

pub fn read_host_memory() -> Result<HostMemorySample, MemoryReadError> {
    // Timestamp before reading: a delayed read cannot make old data look newer.
    let sampled_at_ms = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| MemoryReadError::Invalid)?
            .as_millis(),
    )
    .map_err(|_| MemoryReadError::Invalid)?;
    let mut input = String::new();
    std::fs::File::open("/proc/meminfo")?
        .take(65_537)
        .read_to_string(&mut input)?;
    parse_meminfo(&input, sampled_at_ms)
}
