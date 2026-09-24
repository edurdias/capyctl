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

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mllm_domain::resources::ProcessResident;

/// How often a new sample is started when one is asked for.
pub const REFRESH: Duration = Duration::from_secs(2);
/// A sample older than this is not reported.
pub const MAX_AGE: Duration = Duration::from_secs(5);
/// The bound on one `nvidia-smi` run.
pub const BOUND: Duration = Duration::from_secs(3);
/// At most this many processes are reported per sample.
pub const MAX_PROCESSES: usize = 256;

/// Reads GPU memory per pid: `(pid, bytes)`.
pub type GpuCollector = dyn Fn() -> Option<Vec<(u32, i64)>> + Send + Sync;

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
    /// The production sampler: `nvidia-smi --query-compute-apps`.
    pub fn nvidia() -> Arc<Self> {
        Self::with_collector(Arc::new(|| {
            run_nvidia_smi().and_then(|text| parse_compute_apps(&text))
        }))
    }

    /// A sampler reading GPU memory per pid through `collect`.
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
                .name("mllm-residency".into())
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

    /// Take one sample now, on this thread.
    pub fn sample_now(&self) -> Vec<ProcessResident> {
        let Some(gpu) = (self.collect)() else {
            return Vec::new();
        };
        let Some(boot_id) = boot_id() else {
            return Vec::new();
        };
        gpu.into_iter()
            .take(MAX_PROCESSES)
            .filter_map(|(pid, gpu_bytes)| {
                let start_ticks = start_ticks(pid)?;
                let anonymous = anonymous_resident_bytes(pid)?;
                Some(ProcessResident {
                    pid,
                    boot_id: boot_id.clone(),
                    start_ticks,
                    bytes: gpu_bytes.checked_add(anonymous)?,
                })
            })
            .collect()
    }
}

/// Parse `nvidia-smi --query-compute-apps=pid,used_memory
/// --format=csv,noheader,nounits` output (MiB). Rows whose memory the driver
/// does not report (`[N/A]`, `[Not Supported]`) are skipped; anything else
/// malformed refuses the whole sample (`None`).
pub fn parse_compute_apps(text: &str) -> Option<Vec<(u32, i64)>> {
    if text.len() > 64 * 1024 {
        return None;
    }
    let mut out = Vec::new();
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let (pid, used) = line.split_once(',')?;
        let pid: u32 = pid.trim().parse().ok()?;
        let used = used.trim();
        if used.starts_with('[') {
            continue;
        }
        let mib: i64 = used.parse().ok()?;
        if pid == 0 || mib < 0 {
            return None;
        }
        out.push((pid, mib.checked_mul(1 << 20)?));
        if out.len() > MAX_PROCESSES {
            return None;
        }
    }
    Some(out)
}

fn run_nvidia_smi() -> Option<String> {
    let program = ["/usr/bin/nvidia-smi", "/bin/nvidia-smi"]
        .into_iter()
        .find(|path| std::path::Path::new(path).is_file())?;
    let mut child = Command::new(program)
        .args([
            "--query-compute-apps=pid,used_memory",
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
        stdout
            .take(64 * 1024 + 1)
            .read_to_string(&mut text)
            .ok()?;
    }
    status.success().then_some(text)
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

/// `RssAnon` of /proc/<pid>/status, in bytes: resident pages no page-cache
/// reclaim can return to the host's availability.
fn anonymous_resident_bytes(pid: u32) -> Option<i64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let line = status.lines().find(|line| line.starts_with("RssAnon:"))?;
    let mut words = line["RssAnon:".len()..].split_whitespace();
    let kib: i64 = words.next()?.parse().ok()?;
    (words.next() == Some("kB") && kib >= 0).then(|| kib.checked_mul(1024))?
}

#[cfg(test)]
mod tests {
    use super::*;

    // T26: the driver's per-process rows, in MiB; unreported rows are skipped
    // and anything malformed refuses the whole sample.
    #[test]
    fn compute_app_rows_parse_strictly() {
        assert_eq!(
            parse_compute_apps("1234, 4290\n 99 , [N/A]\n5678, 0\n").unwrap(),
            vec![(1234, 4290 << 20), (5678, 0)]
        );
        assert_eq!(parse_compute_apps("").unwrap(), vec![]);
        for bad in ["x, 1", "1, x", "1", "0, 5", "1, -5"] {
            assert!(parse_compute_apps(bad).is_none(), "{bad}");
        }
    }

    // T26: a sample is bound to the process identity: this process's own pid
    // reports its start ticks and anonymous pages beside the GPU bytes read,
    // and a pid that does not exist is dropped.
    #[test]
    fn a_sample_binds_each_process_to_its_identity() {
        let me = std::process::id();
        let sampler = ResidencySampler::with_collector(Arc::new(move || {
            Some(vec![(me, 1 << 20), (u32::MAX - 1, 1 << 20)])
        }));
        let sample = sampler.sample_now();
        assert_eq!(sample.len(), 1);
        assert_eq!(sample[0].pid, me);
        assert_eq!(Some(sample[0].start_ticks), start_ticks(me));
        assert!(sample[0].bytes > 1 << 20, "GPU bytes plus anonymous pages");
    }

    // T26: `current` never blocks; the first call starts a sample and a later
    // one reports it.
    #[test]
    fn current_reports_a_background_sample() {
        let me = std::process::id();
        let sampler = ResidencySampler::with_collector(Arc::new(move || Some(vec![(me, 0)])));
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
