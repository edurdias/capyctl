//! Publication of the host's NVIDIA device inventory at standalone boot.
//!
//! SPEC §3: the versioned inventory digest (`mllm-nvidia-inventory-v1`,
//! computed by `runtime/sglang_device.py`) is a host fact, published at boot
//! exactly like the fingerprints, and each device's physical UUID is what the
//! guarded launcher sets the engine child's `CUDA_VISIBLE_DEVICES` from. Both
//! are published only when the bounded, closed-error collector
//! observes an inventory: a machine with no NVIDIA devices publishes nothing,
//! and an SGLang deployment then fails placement honestly at the native gate
//! instead of the host claiming devices it cannot see.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use mllm_agent::gpu_memory::GpuSample;
use serde_json::Value;

/// How long the collector is given before the publication is abandoned. The
/// collector itself is import-free and reads only bounded kernel files, so a
/// collection that outlives this bound is a wedged host, and the fail-closed
/// shape — publishing nothing — is the answer, not an unbounded wait.
const BOUND: Duration = Duration::from_secs(30);

/// The collector module, run with the interpreter's `-m` from the checkout
/// that owns the `runtime` package.
const MODULE: &str = "runtime.sglang_device";

/// What a boot publishes about the host's NVIDIA devices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventoryPublication {
    /// Host identity observed in the same inventory as its digest.
    pub host_id: String,
    /// The collector's `mllm-nvidia-inventory-v1` digest, as the host policy's
    /// `device_inventory_digest` carries it.
    pub digest: String,
    /// Each device's physical UUID, keyed by the driver index `nvidia-smi`
    /// reports for it — the `N` of the host policy's device `gpuN` (design §7:
    /// every observed GPU is published with its own UUID).
    ///
    /// Design §1: a UUID is published only when the inventory collector and
    /// `nvidia-smi` observe the same UUID at the same PCI address for every
    /// device; any disagreement publishes no UUIDs (fail closed) while the
    /// digest, which is the inventory's own fact, is still published. Without
    /// an `nvidia-smi` sample only a single-device inventory names its device
    /// (as `gpu0`, the one entry such a host publishes); more than one device
    /// cannot be keyed by index, so none is.
    pub physical_gpu_uuids: BTreeMap<u32, String>,
}

/// The live publication: `python3 -m runtime.sglang_device` inside
/// `runtime_root`. Every failure is closed and silent — no output, a non-zero
/// exit, a malformed document, or the bound expiring all publish nothing.
///
/// `sample` is the boot's one `nvidia-smi` sample, which the device UUIDs are
/// corroborated against and keyed by (see
/// [`InventoryPublication::physical_gpu_uuids`]).
pub fn collect(runtime_root: &Path, sample: Option<&GpuSample>) -> Option<InventoryPublication> {
    collect_with(runtime_root, sample, &run_collector)
}

/// The seam a test stubs instead of running Python.
pub fn collect_with(
    runtime_root: &Path,
    sample: Option<&GpuSample>,
    run: &dyn Fn(&Path) -> std::io::Result<String>,
) -> Option<InventoryPublication> {
    publication(&run(runtime_root).ok()?, sample)
}

/// Runs the collector and returns its stdout, bounded.
fn run_collector(runtime_root: &Path) -> std::io::Result<String> {
    // SPEC §9.1 / T21: no bytecode is written into mllm's runtime tree, whose
    // integrity check refuses any it finds.
    let mut child = Command::new("python3")
        .arg("-B")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .arg("-m")
        .arg(MODULE)
        .current_dir(runtime_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + BOUND;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "the device inventory bound was reached",
                ));
            }
        }
    };
    let mut printed = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        // The child has exited, so this reads what it left in the pipe.
        std::io::Read::read_to_string(&mut stdout, &mut printed)?;
    }
    if !status.success() {
        return Err(std::io::Error::other(
            "the device collector refused to observe this host",
        ));
    }
    Ok(printed)
}

/// The closed parse of the collector's JSON document. The digest must be the
/// exact 64 lowercase hex characters the host policy validates, and every
/// device UUID the exact shape `runtime/sglang_device.py` validates; anything
/// else is a document the host does not publish from.
pub fn publication(raw: &str, sample: Option<&GpuSample>) -> Option<InventoryPublication> {
    let document: Value = serde_json::from_str(raw.trim()).ok()?;
    let host_id = document["host_id"].as_str()?;
    if host_id.is_empty()
        || host_id.len() > 253
        || !host_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
    {
        return None;
    }
    let digest = document["digest"].as_str()?;
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return None;
    }
    let devices = document["devices"].as_array()?;
    // The collector refuses an empty inventory (a non-zero exit with no
    // output), so an empty list here is a malformed document, not a host fact.
    if devices.is_empty() {
        return None;
    }
    // The collector never publishes a device without its validated UUID, so a
    // document that claims otherwise is malformed and publishes nothing.
    let observed = devices
        .iter()
        .map(|device| {
            let uuid = device["physical_gpu_uuid"]
                .as_str()
                .filter(|uuid| is_physical_uuid(uuid))?;
            Some((uuid, device["pci_address"].as_str()))
        })
        .collect::<Option<Vec<_>>>()?;
    let physical_gpu_uuids = match sample {
        Some(sample) => corroborated(&observed, sample).unwrap_or_default(),
        // No index to key by: only the single device a `gpu0` host names.
        None => match observed.as_slice() {
            [(uuid, _)] => BTreeMap::from([(0, (*uuid).to_owned())]),
            _ => BTreeMap::new(),
        },
    };
    Some(InventoryPublication {
        host_id: host_id.to_owned(),
        digest: digest.to_owned(),
        physical_gpu_uuids,
    })
}

/// Design §1: the inventory's devices keyed by `nvidia-smi`'s index, or `None`
/// unless both observe exactly the same devices — the same UUID at the same PCI
/// address, one for one.
fn corroborated(
    observed: &[(&str, Option<&str>)],
    sample: &GpuSample,
) -> Option<BTreeMap<u32, String>> {
    if observed.len() != sample.devices.len() {
        return None;
    }
    let mut keyed = BTreeMap::new();
    for (uuid, pci_address) in observed {
        let address = pci_bus_id((*pci_address)?)?;
        let device = sample
            .devices
            .iter()
            .find(|device| pci_bus_id(&device.pci_bus_id) == Some(address.clone()))?;
        if device.uuid != *uuid || keyed.insert(device.index, (*uuid).to_owned()).is_some() {
            return None;
        }
    }
    Some(keyed)
}

/// A PCI address in one comparable form. The collector writes the kernel's
/// `dddd:bb:dd.f`; `nvidia-smi` writes an eight-digit, upper-case domain
/// (`0000000F:01:00.0`). Both name the domain as hex, so it is compared as a
/// number and the rest case-insensitively.
fn pci_bus_id(address: &str) -> Option<(u32, String)> {
    let (domain, rest) = address.split_once(':')?;
    if domain.is_empty() || rest.is_empty() {
        return None;
    }
    Some((
        u32::from_str_radix(domain, 16).ok()?,
        rest.to_ascii_lowercase(),
    ))
}

/// The physical UUID shape `runtime/sglang_device.py` validates (`GPU-` +
/// 8-4-4-4 lowercase hex). Both sides refuse exactly the same inputs.
pub fn is_physical_uuid(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("GPU-") else {
        return false;
    };
    rest.len() == 36
        && rest.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => matches!(byte, b'0'..=b'9' | b'a'..=b'f'),
        })
}
