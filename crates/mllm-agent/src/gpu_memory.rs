//! SPEC §7.2 / ADR 0019: the memory of each NVIDIA GPU on this host.
//!
//! A discrete GPU's VRAM is a physical domain distinct from host RAM; an
//! integrated device (GB10) reports `[N/A]` memory because it has none of its
//! own, which is how a unified host is recognised — never from a product name.
//! Sampling runs `nvidia-smi` from a fixed path with a cleared environment,
//! bounded in time and output, exactly like `process_residency`. Every parse
//! failure invalidates the whole sample: an unknown device closes admission
//! on its domain (SPEC §7.2), it is never filled in.

use std::collections::BTreeSet;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The bound on one `nvidia-smi` run.
pub const BOUND: Duration = Duration::from_secs(3);
/// The most output one run may produce; more invalidates the sample.
pub const MAX_OUTPUT: usize = 64 * 1024;
const MIB: i64 = 1 << 20;
/// Driver rounding between `used + free` and `total`.
const SLACK: i64 = 64 * MIB;
const QUERY: &str = "--query-gpu=index,uuid,pci.bus_id,name,memory.total,memory.used,memory.free";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuMemory {
    pub total_bytes: i64,
    pub used_bytes: i64,
    pub free_bytes: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuDevice {
    pub index: u32,
    pub uuid: String,
    pub pci_bus_id: String,
    pub name: String,
    /// `None` for an integrated device sharing host memory.
    pub memory: Option<GpuMemory>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuSample {
    pub devices: Vec<GpuDevice>,
    pub sampled_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostShape {
    NoGpu,
    Unified,
    Discrete(Vec<GpuDevice>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GpuShapeError {
    #[error("unsupported_gpu_topology: integrated and discrete GPUs on one host")]
    MixedTopology,
}

impl GpuShapeError {
    /// The stable error code (design §11).
    pub fn code(self) -> &'static str {
        match self {
            GpuShapeError::MixedTopology => "unsupported_gpu_topology",
        }
    }
}

pub type GpuSampler = dyn Fn() -> Option<GpuSample> + Send + Sync;

/// A MiB field in bytes; `Ok(None)` when the device has no memory of its own.
fn mib(field: &str) -> Result<Option<i64>, ()> {
    match field {
        "[N/A]" | "[Not Supported]" => Ok(None),
        text => {
            let value: i64 = text.parse().map_err(|_| ())?;
            if value < 0 {
                return Err(());
            }
            value.checked_mul(MIB).map(Some).ok_or(())
        }
    }
}

/// Parse `nvidia-smi --query-gpu=... --format=csv,noheader,nounits`.
/// `None` when anything is malformed: the sample is closed, never repaired.
pub fn parse_query_gpu(text: &str, sampled_at_ms: i64) -> Option<GpuSample> {
    if text.len() > MAX_OUTPUT || sampled_at_ms < 0 {
        return None;
    }
    let mut devices = Vec::new();
    let (mut indexes, mut uuids) = (BTreeSet::new(), BTreeSet::new());
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let fields: Vec<&str> = line.split(',').map(str::trim).collect();
        let [index, uuid, bus, name, total, used, free] = fields.as_slice() else {
            return None;
        };
        let index: u32 = index.parse().ok()?;
        if !uuid.starts_with("GPU-") || uuid.len() > 64 || bus.is_empty() || name.is_empty() {
            return None;
        }
        if !indexes.insert(index) || !uuids.insert(uuid.to_string()) {
            return None;
        }
        let memory = match (mib(total).ok()?, mib(used).ok()?, mib(free).ok()?) {
            (None, None, None) => None,
            (Some(total_bytes), Some(used_bytes), Some(free_bytes)) => {
                if total_bytes == 0
                    || used_bytes.checked_add(free_bytes)? > total_bytes.checked_add(SLACK)?
                {
                    return None;
                }
                Some(GpuMemory {
                    total_bytes,
                    used_bytes,
                    free_bytes,
                })
            }
            _ => return None,
        };
        devices.push(GpuDevice {
            index,
            uuid: uuid.to_string(),
            pci_bus_id: bus.to_string(),
            name: name.to_string(),
            memory,
        });
    }
    devices.sort_by_key(|d| d.index);
    Some(GpuSample {
        devices,
        sampled_at_ms,
    })
}

/// The host shape a sample describes (design §1). Mixed integrated and
/// discrete devices are refused rather than guessed.
pub fn shape(sample: Option<&GpuSample>) -> Result<HostShape, GpuShapeError> {
    let Some(sample) = sample.filter(|s| !s.devices.is_empty()) else {
        return Ok(HostShape::NoGpu);
    };
    let integrated = sample.devices.iter().filter(|d| d.memory.is_none()).count();
    match integrated {
        0 => Ok(HostShape::Discrete(sample.devices.clone())),
        n if n == sample.devices.len() => Ok(HostShape::Unified),
        _ => Err(GpuShapeError::MixedTopology),
    }
}

/// The live sample; `None` on any failure (no binary, timeout, bad output).
pub fn sample() -> Option<GpuSample> {
    let program = ["/usr/bin/nvidia-smi", "/bin/nvidia-smi"]
        .into_iter()
        .find(|p| std::path::Path::new(p).is_file())?;
    let text = run_bounded(program, &[QUERY, "--format=csv,noheader,nounits"], BOUND)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis();
    parse_query_gpu(&text, i64::try_from(now).ok()?)
}

/// Run `program` with a cleared environment, stdin closed and stderr
/// discarded; killed at `bound`. `None` on a failed spawn, a timeout, a
/// non-zero exit or unreadable output. Output beyond [`MAX_OUTPUT`] is kept
/// one byte over the limit so the parser refuses it.
fn run_bounded(program: &str, args: &[&str], bound: Duration) -> Option<String> {
    let mut child = Command::new(program)
        .args(args)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + bound;
    let status = loop {
        match child.try_wait().ok()? {
            Some(status) => break status,
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let mut text = String::new();
    child
        .stdout
        .take()?
        .take(MAX_OUTPUT as u64 + 1)
        .read_to_string(&mut text)
        .ok()?;
    if !status.success() {
        return None;
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: i64 = 1 << 20;

    // T26: a discrete card reports its own memory in MiB.
    #[test]
    fn a_discrete_row_is_parsed_in_bytes() {
        let text = "0, GPU-11111111-2222-3333-4444-555555555555, 00000000:01:00.0, NVIDIA Discrete GPU, 16376, 1500, 14876\n";
        let sample = parse_query_gpu(text, 1_000).expect("valid row");
        let device = &sample.devices[0];
        assert_eq!(device.index, 0);
        assert_eq!(device.pci_bus_id, "00000000:01:00.0");
        let memory = device.memory.as_ref().expect("discrete");
        assert_eq!(memory.total_bytes, 16376 * MIB);
        assert_eq!(memory.used_bytes, 1500 * MIB);
        assert_eq!(memory.free_bytes, 14876 * MIB);
        assert_eq!(
            shape(Some(&sample)).unwrap(),
            HostShape::Discrete(sample.devices.clone())
        );
    }

    // T26: an integrated device (GB10) has no memory of its own.
    #[test]
    fn an_integrated_row_is_unified() {
        let text = "0, GPU-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee, 0000000F:01:00.0, NVIDIA GB10, [N/A], [N/A], [N/A]\n";
        let sample = parse_query_gpu(text, 1).expect("valid row");
        assert!(sample.devices[0].memory.is_none());
        assert_eq!(shape(Some(&sample)).unwrap(), HostShape::Unified);
        let unsupported = text.replace("[N/A]", "[Not Supported]");
        assert!(parse_query_gpu(&unsupported, 1).unwrap().devices[0]
            .memory
            .is_none());
    }

    #[test]
    fn no_sample_or_no_device_is_no_gpu() {
        assert_eq!(shape(None).unwrap(), HostShape::NoGpu);
        assert_eq!(
            shape(Some(&parse_query_gpu("", 1).unwrap())).unwrap(),
            HostShape::NoGpu
        );
    }

    // T26: mixed integrated and discrete devices are refused, never guessed.
    #[test]
    fn mixed_topology_is_refused() {
        let text = "0, GPU-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee, 0000000F:01:00.0, NVIDIA GB10, [N/A], [N/A], [N/A]\n\
                    1, GPU-11111111-2222-3333-4444-555555555555, 00000000:01:00.0, RTX, 16376, 0, 16376\n";
        let sample = parse_query_gpu(text, 1).unwrap();
        assert_eq!(shape(Some(&sample)), Err(GpuShapeError::MixedTopology));
    }

    // T26: a collector that overruns its bound is killed and yields nothing.
    #[test]
    fn a_timed_out_run_yields_nothing() {
        let started = Instant::now();
        assert!(run_bounded("/bin/sleep", &["5"], Duration::from_millis(200)).is_none());
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    // T26: a failing collector yields nothing; a clean one yields its output.
    #[test]
    fn a_bounded_run_reports_only_success() {
        assert!(run_bounded("/bin/false", &[], BOUND).is_none());
        assert!(run_bounded("/nonexistent/nvidia-smi", &[], BOUND).is_none());
        assert_eq!(
            run_bounded("/bin/echo", &["ok"], BOUND).as_deref(),
            Some("ok\n")
        );
    }

    #[test]
    fn the_error_code_is_stable() {
        assert_eq!(
            GpuShapeError::MixedTopology.code(),
            "unsupported_gpu_topology"
        );
    }

    // Optional: on a host with a GPU the live sample must parse and shape.
    #[test]
    fn a_live_sample_when_present_has_a_shape() {
        if let Some(sample) = sample() {
            assert!(shape(Some(&sample)).is_ok());
        }
    }

    // T29: anything malformed invalidates the whole sample.
    #[test]
    fn malformed_samples_are_refused() {
        let good =
            "0, GPU-11111111-2222-3333-4444-555555555555, 00000000:01:00.0, RTX, 16376, 0, 16376\n";
        for bad in [
            "0, GPU-x, 00000000:01:00.0, RTX, 16376, 0\n".to_string(), // 6 fields
            good.replace("16376, 0, 16376", "16376, 9000, 9000"),      // used+free > total
            good.replace("0, GPU", "x, GPU"),                          // index
            good.replace("16376, 0", "-1, 0"),                         // negative
            format!("{good}{good}"),                                   // duplicate
            "a".repeat(70_000),                                        // oversized
        ] {
            assert!(parse_query_gpu(&bad, 1).is_none(), "refused: {bad:.60}");
        }
    }
}
