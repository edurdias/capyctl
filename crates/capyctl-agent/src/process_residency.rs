//! ADR 0007: the memory each GPU process on this host holds, sampled beside
//! the host's availability.
//!
//! Found live 2026-09-23 (matrix M33, host-a): a vLLM wake beside a ready
//! SGLang 14B was refused `insufficient resources` although 46 + 24 GiB fit the
//! 97 GiB limit. The host's published availability already excluded the 46 GiB
//! SGLang held, and admission charged that reservation again, so every engine
//! resident on a host was counted twice. ADR 0007 allows credit only for a
//! verified resident lower bound bound to the current runtime identity, so the
//! host reports, per process, what the driver attributes to it (GPU memory,
//! which on a unified-memory host is carved from the same pool) plus its
//! anonymous resident pages, keyed by the process identity. The lifecycle
//! authority matches that identity against the runtime it recorded; this
//! module grants nothing.
//!
//! Sampling runs `nvidia-smi` off the caller's thread, at most every
//! [`REFRESH`], bounded by [`BOUND`]. A host without it reports no processes,
//! which credits nothing (the behaviour before this existed).

use std::collections::BTreeMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use capyctl_domain::resources::{DeviceBytes, ProcessResident};

/// How often a new sample is started when one is asked for.
pub const REFRESH: Duration = Duration::from_secs(2);
/// A sample older than this is not reported.
pub const MAX_AGE: Duration = Duration::from_secs(5);
/// The bound on one `nvidia-smi` run.
pub const BOUND: Duration = Duration::from_secs(3);
/// At most this many processes are reported per sample.
pub const MAX_PROCESSES: usize = 256;

/// ADR 0035 §8: one reading of GPU memory per (process, GPU): `(pid, device,
/// bytes)`, the device by its host-local id (`gpuN`).
pub type GpuReading = (u32, String, i64);

/// Reads GPU memory per (process, GPU).
pub type GpuCollector = dyn Fn() -> Option<Vec<GpuReading>> + Send + Sync;

#[derive(Default)]
struct State {
    last: Option<(Instant, Vec<ProcessResident>)>,
    running: bool,
}

/// Background sampler of per-process resident memory.
pub struct ResidencySampler {
    state: Mutex<State>,
    collect: Arc<GpuCollector>,
}

impl ResidencySampler {
    /// The production sampler: `nvidia-smi --query-compute-apps`, each row's
    /// GPU UUID mapped to its host device id through the host's GPU
    /// inventory (ADR 0035 §8). The inventory is read again once when a row
    /// names a UUID it does not hold; a UUID still unknown refuses the
    /// sample.
    pub fn nvidia() -> Arc<Self> {
        let inventory: Mutex<BTreeMap<String, String>> = Mutex::default();
        Self::with_collector(Arc::new(move || {
            let text = run_nvidia_smi()?;
            let mut known = inventory.lock().ok()?;
            if let Some(rows) = parse_compute_apps(&text, &known) {
                return Some(rows);
            }
            *known = inventory_devices(&crate::gpu_memory::sample()?);
            parse_compute_apps(&text, &known)
        }))
    }

    /// A sampler reading GPU memory per (process, GPU) through `collect`.
    pub fn with_collector(collect: Arc<GpuCollector>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::default(),
            collect,
        })
    }

    /// The latest sample no older than [`MAX_AGE`], keeping only processes
    /// that are still the same processes (pid and start ticks). Never blocks:
    /// when the sample is older than [`REFRESH`] a new one is started in the
    /// background and a later call reports it.
    pub fn current(self: &Arc<Self>) -> Vec<ProcessResident> {
        let Ok(mut state) = self.state.lock() else {
            return Vec::new();
        };
        let stale = state
            .last
            .as_ref()
            .is_none_or(|(at, _)| at.elapsed() >= REFRESH);
        if stale && !state.running {
            state.running = true;
            let this = self.clone();
            let spawned = std::thread::Builder::new()
                .name("capyctl-residency".into())
                .spawn(move || {
                    let sample = this.sample_now();
                    if let Ok(mut state) = this.state.lock() {
                        state.last = Some((Instant::now(), sample));
                        state.running = false;
                    }
                });
            if spawned.is_err() {
                state.running = false;
            }
        }
        match &state.last {
            Some((at, sample)) if at.elapsed() < MAX_AGE => sample
                .iter()
                .filter(|p| start_ticks(p.pid) == Some(p.start_ticks))
                .cloned()
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Take one sample now, on this thread, and keep it as the latest for
    /// [`Self::current`]. For a caller that asks rarely (an admission attempt
    /// every few tens of seconds), for which a cached sample is always past
    /// [`MAX_AGE`] (found live on a 16 GB discrete GPU).
    pub fn sample_fresh(&self) -> Vec<ProcessResident> {
        let sample = self.sample_now();
        if let Ok(mut state) = self.state.lock() {
            state.last = Some((Instant::now(), sample.clone()));
        }
        sample
    }

    /// Take one sample now, on this thread. ADR 0035 §8: one entry per
    /// process, with its GPU memory per GPU (`per_device`) and their sum
    /// (`device_bytes`); its host pages are read once, never per GPU.
    pub fn sample_now(&self) -> Vec<ProcessResident> {
        let Some(gpu) = (self.collect)() else {
            return Vec::new();
        };
        let Some(boot_id) = boot_id() else {
            return Vec::new();
        };
        let mut processes: Vec<(u32, Vec<DeviceBytes>)> = Vec::new();
        for (pid, device, bytes) in gpu {
            let Ok(bytes) = u64::try_from(bytes) else {
                return Vec::new();
            };
            let at = match processes.iter().position(|(p, _)| *p == pid) {
                Some(at) => at,
                None => {
                    processes.push((pid, Vec::new()));
                    processes.len() - 1
                }
            };
            let per_device = &mut processes[at].1;
            match per_device.iter_mut().find(|d| d.device == device) {
                Some(entry) => entry.bytes = entry.bytes.saturating_add(bytes),
                None => per_device.push(DeviceBytes { device, bytes }),
            }
        }
        processes
            .into_iter()
            .take(MAX_PROCESSES)
            .filter_map(|(pid, per_device)| {
                let start_ticks = start_ticks(pid)?;
                let anonymous = anonymous_resident_bytes(pid)?;
                let gpu_bytes = per_device.iter().try_fold(0_i64, |sum, d| {
                    sum.checked_add(i64::try_from(d.bytes).ok()?)
                })?;
                // ADR 0019: the two figures stay separate so a discrete host
                // credits each to its own domain; a unified host credits the
                // sum (ADR 0007).
                Some(ProcessResident {
                    pid,
                    boot_id: boot_id.clone(),
                    start_ticks,
                    bytes: gpu_bytes.checked_add(anonymous)?,
                    device_bytes: gpu_bytes,
                    host_bytes: anonymous,
                    per_device,
                })
            })
            .collect()
    }
}

/// ADR 0035 §8: the host device id (`gpuN`, N the driver's index) of each GPU
/// UUID in an inventory sample.
pub fn inventory_devices(sample: &crate::gpu_memory::GpuSample) -> BTreeMap<String, String> {
    sample
        .devices
        .iter()
        .map(|device| (device.uuid.clone(), format!("gpu{}", device.index)))
        .collect()
}

/// Parse `nvidia-smi --query-compute-apps=pid,gpu_uuid,used_memory
/// --format=csv,noheader,nounits` output (MiB). ADR 0035 §8: the driver
/// returns one row per process and GPU; each GPU UUID is mapped to its host
/// device id through `inventory` (UUID to `gpuN`), and repeated rows of one
/// process on one GPU are one reading. Rows whose memory the driver does not
/// report (`[N/A]`, `[Not Supported]`) are skipped; a UUID the inventory does
/// not hold, or anything else malformed, refuses the whole sample (`None`).
pub fn parse_compute_apps(
    text: &str,
    inventory: &BTreeMap<String, String>,
) -> Option<Vec<GpuReading>> {
    if text.len() > 64 * 1024 {
        return None;
    }
    let mut out: Vec<GpuReading> = Vec::new();
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let mut fields = line.split(',').map(str::trim);
        let (Some(pid), Some(uuid), Some(used), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return None;
        };
        let pid: u32 = pid.parse().ok()?;
        if pid == 0 {
            return None;
        }
        let device = inventory.get(uuid)?;
        if used.starts_with('[') {
            continue;
        }
        let mib: i64 = used.parse().ok()?;
        if mib < 0 {
            return None;
        }
        let bytes = mib.checked_mul(1 << 20)?;
        match out.iter_mut().find(|(p, d, _)| *p == pid && d == device) {
            Some(reading) => reading.2 = reading.2.checked_add(bytes)?,
            None => out.push((pid, device.clone(), bytes)),
        }
        if out.len() > MAX_PROCESSES * 8 {
            return None;
        }
    }
    Some(out)
}

fn run_nvidia_smi() -> Option<String> {
    // A test process whose GPUs are pinned (`TEST_PINNED_GPU_ENV`, debug
    // builds only) observes no process on this machine's GPUs, as on a
    // machine without `nvidia-smi`.
    if crate::gpu_memory::gpu_pinned() {
        return None;
    }
    // SPEC §7.2: the same lookup as the memory sample: fixed paths, then PATH.
    let program = crate::gpu_memory::nvidia_smi()?;
    let mut child = Command::new(program)
        .args([
            "--query-compute-apps=pid,gpu_uuid,used_memory",
            "--format=csv,noheader,nounits",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + BOUND;
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
    if let Some(stdout) = child.stdout.take() {
        stdout.take(64 * 1024 + 1).read_to_string(&mut text).ok()?;
    }
    status.success().then_some(text)
}

/// ADR 0033 §6, A1 (lifecycle design §7): the resident set (`VmRSS`:
/// anonymous, shared and file-backed pages) of a launch's LMCache server
/// group: each of its recorded processes still alive (same boot, same start
/// ticks), summed. The basis A1's overhead allowance was measured on (found
/// live 2026-10-22, GB10: a SGLang server's 4903 MiB `VmRSS` held 329 MiB of
/// `RssFile` that `RssAnon` plus `RssShmem` missed). `None` when the
/// server's `lmcache` leader is not alive or a live member cannot be read:
/// unknown, never zero.
pub fn group_resident_bytes(group: &[capyctl_domain::completion::ProcessIdentity]) -> Option<u64> {
    let boot = boot_id()?;
    let alive = |identity: &capyctl_domain::completion::ProcessIdentity| {
        identity.boot_id == boot && start_ticks(identity.pid) == Some(identity.start_ticks)
    };
    if !group.iter().any(|identity| {
        identity.role == capyctl_domain::completion::LMCACHE_ROLE && alive(identity)
    }) {
        return None;
    }
    group
        .iter()
        .filter(|identity| alive(identity))
        .try_fold(0_u64, |sum, identity| {
            let status = std::fs::read_to_string(format!("/proc/{}/status", identity.pid)).ok()?;
            sum.checked_add(u64::try_from(parse_vm_resident_bytes(&status)?).ok()?)
        })
}

/// ADR 0033 §6: the GPU memory `sample` attributes to the processes of
/// `group` (matched by pid, boot and start ticks), summed. `None` when the
/// sample names none of them: unknown, never zero.
pub fn group_device_bytes(
    sample: &[ProcessResident],
    group: &[capyctl_domain::completion::ProcessIdentity],
) -> Option<u64> {
    let matched: Vec<i64> = sample
        .iter()
        .filter(|resident| {
            group.iter().any(|identity| {
                identity.pid == resident.pid
                    && identity.boot_id == resident.boot_id
                    && identity.start_ticks == resident.start_ticks
            })
        })
        .map(|resident| resident.device_bytes)
        .collect();
    if matched.is_empty() {
        return None;
    }
    matched.into_iter().try_fold(0_u64, |sum, bytes| {
        sum.checked_add(u64::try_from(bytes).ok()?)
    })
}

fn boot_id() -> Option<String> {
    let raw = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    let boot = raw.trim();
    (boot.len() == 36).then(|| boot.to_owned())
}

/// Field 22 of /proc/<pid>/stat, counted after the comm field's final ')'.
fn start_ticks(pid: u32) -> Option<u64> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = raw.rfind(')')?;
    raw.get(close + 2..)?
        .split_whitespace()
        .nth(19)
        .and_then(|field| field.parse().ok())
}

/// `RssAnon` plus `RssShmem` of /proc/<pid>/status, in bytes: resident pages
/// no page-cache reclaim can return to the host's availability. Pinned host
/// memory (a `host_backed` weights copy) is shared memory, not anonymous:
/// found live on a 16 GB discrete GPU, vLLM's parked copy of Qwen3-4B was
/// 11.2 GB of `RssShmem` beside 1.9 GB of `RssAnon`, and stayed after the wake.
fn anonymous_resident_bytes(pid: u32) -> Option<i64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    parse_resident_host_bytes(&status)
}

/// The `RssAnon` and `RssShmem` lines of a /proc/<pid>/status text, summed.
fn parse_resident_host_bytes(status: &str) -> Option<i64> {
    status_kib(status, "RssAnon:")?
        .checked_add(status_kib(status, "RssShmem:")?)?
        .checked_mul(1024)
}

/// The `VmRSS` line of a /proc/<pid>/status text, in bytes.
fn parse_vm_resident_bytes(status: &str) -> Option<i64> {
    status_kib(status, "VmRSS:")?.checked_mul(1024)
}

/// The kB figure of the `name` line of a /proc/<pid>/status text.
fn status_kib(status: &str, name: &str) -> Option<i64> {
    let line = status.lines().find(|line| line.starts_with(name))?;
    let mut words = line[name.len()..].split_whitespace();
    let kib: i64 = words.next()?.parse().ok()?;
    (words.next() == Some("kB") && kib >= 0).then_some(kib)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inventory() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("GPU-aaaa".to_string(), "gpu0".to_string()),
            ("GPU-bbbb".to_string(), "gpu1".to_string()),
        ])
    }

    // T26 T46 (ADR 0035 §8): the driver's per-process rows, in MiB, each on
    // its GPU by UUID, mapped to the host device id; unreported rows are
    // skipped and anything malformed refuses the whole sample.
    #[test]
    fn compute_app_rows_parse_strictly() {
        assert_eq!(
            parse_compute_apps(
                "1234, GPU-aaaa, 4290\n 99 , GPU-aaaa, [N/A]\n5678, GPU-bbbb, 0\n",
                &inventory()
            )
            .unwrap(),
            vec![(1234, "gpu0".into(), 4290 << 20), (5678, "gpu1".into(), 0)]
        );
        assert_eq!(parse_compute_apps("", &inventory()).unwrap(), vec![]);
        for bad in [
            "x, GPU-aaaa, 1",
            "1, GPU-aaaa, x",
            "1",
            "1, 5",
            "0, GPU-aaaa, 5",
            "1, GPU-aaaa, -5",
            "1, GPU-aaaa, 5, 6",
        ] {
            assert!(parse_compute_apps(bad, &inventory()).is_none(), "{bad}");
        }
    }

    // T46 (ADR 0035 §8): one process on two GPUs is two readings, one per
    // GPU; repeated rows of one process on one GPU are one reading; a UUID the
    // host's inventory does not hold refuses the whole sample, as a
    // malformed row does.
    #[test]
    fn a_process_on_two_gpus_reads_per_gpu_and_unknown_uuids_refuse() {
        let rows = parse_compute_apps(
            "10, GPU-aaaa, 20000\n10, GPU-bbbb, 512\n11, GPU-bbbb, 100\n11, GPU-bbbb, 28\n",
            &inventory(),
        )
        .unwrap();
        assert_eq!(
            rows,
            vec![
                (10, "gpu0".into(), 20000 << 20),
                (10, "gpu1".into(), 512 << 20),
                (11, "gpu1".into(), 128 << 20),
            ]
        );
        assert!(parse_compute_apps("10, GPU-aaaa, 1\n11, GPU-cccc, 1\n", &inventory()).is_none());
        assert!(parse_compute_apps("11, GPU-cccc, [N/A]\n", &inventory()).is_none());
        // The inventory maps each UUID to `gpu<index>`.
        let sample = crate::gpu_memory::GpuSample {
            devices: vec![crate::gpu_memory::GpuDevice {
                index: 3,
                uuid: "GPU-dddd".into(),
                pci_bus_id: "00000000:01:00.0".into(),
                name: "card".into(),
                memory: None,
            }],
            sampled_at_ms: 1,
        };
        assert_eq!(
            inventory_devices(&sample),
            BTreeMap::from([("GPU-dddd".to_string(), "gpu3".to_string())])
        );
    }

    // T26: the host figure is anonymous plus shared resident memory (a
    // pinned host_backed copy is shared memory); a status without either line
    // gives none.
    #[test]
    fn host_bytes_count_anonymous_and_shared_pages() {
        let status = "Name:\tVLLM::EngineCore\nRssAnon:\t 1828364 kB\nRssFile:\t  611900 kB\nRssShmem:\t10971276 kB\n";
        assert_eq!(
            parse_resident_host_bytes(status),
            Some((1_828_364 + 10_971_276) * 1024)
        );
        assert_eq!(parse_resident_host_bytes("RssAnon:\t 1 kB\n"), None);
        assert_eq!(
            parse_resident_host_bytes("RssAnon:\t x kB\nRssShmem:\t 1 kB\n"),
            None
        );
    }

    // T45 T26 (ADR 0033 §6, A1; found live 2026-10-22, GB10): an LMCache
    // server's resident set is `VmRSS`, its file-backed pages included, the
    // basis A1's overhead allowance was measured on; a status without it
    // gives none.
    #[test]
    fn a_server_groups_resident_set_counts_file_backed_pages() {
        let status = "Name:\tpython3\nVmRSS:\t 5020672 kB\nRssAnon:\t  446464 kB\nRssFile:\t  336896 kB\nRssShmem:\t 4237312 kB\n";
        assert_eq!(parse_vm_resident_bytes(status), Some(5_020_672 * 1024));
        assert!(parse_vm_resident_bytes(status) > parse_resident_host_bytes(status));
        assert_eq!(parse_vm_resident_bytes("RssAnon:\t 1 kB\n"), None);
        // A live group reads its leader's `VmRSS`; a dead leader reads none.
        let me = std::process::id();
        let identity = capyctl_domain::completion::ProcessIdentity {
            pid: me,
            boot_id: boot_id().unwrap(),
            start_ticks: start_ticks(me).unwrap(),
            role: capyctl_domain::completion::LMCACHE_ROLE.into(),
        };
        let own = std::fs::read_to_string("/proc/self/status").unwrap();
        let read = group_resident_bytes(std::slice::from_ref(&identity)).unwrap();
        let vm = parse_vm_resident_bytes(&own).unwrap() as u64;
        assert!(read.abs_diff(vm) < 64 << 20, "{read} vs {vm}");
        assert!(read as i64 > parse_resident_host_bytes(&own).unwrap());
        let gone = capyctl_domain::completion::ProcessIdentity {
            start_ticks: identity.start_ticks + 1,
            ..identity
        };
        assert_eq!(group_resident_bytes(&[gone]), None);
    }

    // T26: a sample is bound to the process identity: this process's own pid
    // reports its start ticks and anonymous pages beside the GPU bytes read,
    // and a pid that does not exist is dropped.
    #[test]
    fn a_sample_binds_each_process_to_its_identity() {
        let me = std::process::id();
        let sampler = ResidencySampler::with_collector(Arc::new(move || {
            Some(vec![
                (me, "gpu0".into(), 1 << 20),
                (u32::MAX - 1, "gpu0".into(), 1 << 20),
            ])
        }));
        let sample = sampler.sample_now();
        assert_eq!(sample.len(), 1);
        assert_eq!(sample[0].pid, me);
        assert_eq!(Some(sample[0].start_ticks), start_ticks(me));
        assert!(sample[0].bytes > 1 << 20, "GPU bytes plus anonymous pages");
        // ADR 0019: the GPU bytes and the anonymous pages are also reported
        // apart, for a discrete host's device and system domains.
        assert_eq!(sample[0].device_bytes, 1 << 20);
        assert!(sample[0].host_bytes > 0);
        assert_eq!(
            sample[0].bytes,
            sample[0].device_bytes + sample[0].host_bytes
        );
    }

    // T46 (ADR 0035 §8): a process holding memory on two GPUs is one entry
    // with a reading per GPU and their sum as its device bytes; its host
    // pages are read once.
    #[test]
    fn a_process_on_two_gpus_is_one_entry_with_per_gpu_readings() {
        let me = std::process::id();
        let sampler = ResidencySampler::with_collector(Arc::new(move || {
            Some(vec![
                (me, "gpu0".into(), 3 << 20),
                (me, "gpu1".into(), 1 << 20),
            ])
        }));
        let sample = sampler.sample_now();
        assert_eq!(sample.len(), 1);
        assert_eq!(
            sample[0].per_device,
            vec![
                DeviceBytes {
                    device: "gpu0".into(),
                    bytes: 3 << 20
                },
                DeviceBytes {
                    device: "gpu1".into(),
                    bytes: 1 << 20
                },
            ]
        );
        assert_eq!(sample[0].device_bytes, 4 << 20);
        assert_eq!(
            sample[0].bytes,
            sample[0].device_bytes + sample[0].host_bytes
        );
        assert_eq!(sample[0].bytes_on("gpu1"), Some(1 << 20));
        assert_eq!(sample[0].bytes_on("gpu2"), Some(0));
    }

    // T26: `current` never blocks; the first call starts a sample and a later
    // one reports it.
    #[test]
    fn current_reports_a_background_sample() {
        let me = std::process::id();
        let sampler =
            ResidencySampler::with_collector(Arc::new(move || Some(vec![(me, "gpu0".into(), 0)])));
        let first = sampler.current();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = first;
        while seen.is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
            seen = sampler.current();
        }
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].pid, me);
        // A collector that fails reports nothing.
        let failing = ResidencySampler::with_collector(Arc::new(|| None));
        assert!(failing.sample_now().is_empty());
    }
}
