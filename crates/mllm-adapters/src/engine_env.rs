//! The engine toolchain environment: where the JIT compilers are found and how
//! many compile jobs they may run (owner decisions 2026-09-25, SPEC §13.3
//! amendment).
//!
//! - `cuda_home`: an optional, host-approved runtime-profile field. When set,
//!   `<cuda_home>/bin` joins the engine PATH after the engine's own bin and
//!   `CUDA_HOME` is set. vLLM treats FlashInfer as unavailable unless `nvcc` is
//!   on PATH (found live on GB10: graph capture with an FP8 KV cache then
//!   failed). Without it the PATH stays the minimal fixed one.
//! - `MAX_JOBS`: FlashInfer's ninja JIT, torch `cpp_extension` and `tvm_ffi`
//!   honour it. Each fused-MoE `nvcc`/`cicc` job held 7 to 9 GB on GB10, and
//!   one job per core ran two hosts out of memory (found live). mllm sets
//!   `clamp(floor(MemAvailable at launch / 8 GiB), 1, CPU count)`.
//! - `FLASHINFER_NVCC_THREADS`: threads inside each FlashInfer `nvcc`; each one
//!   multiplies a job's memory, so mllm pins FlashInfer's own default of 1.
//!
//! A profile's `env` may set `MAX_JOBS` or `FLASHINFER_NVCC_THREADS` (positive
//! integers, checked when the profile is resolved); its value wins. vLLM's
//! `NVCC_THREADS` applies only when vLLM itself is built, so it is not set.

use std::collections::BTreeMap;

/// Memory one JIT compile job is budgeted.
pub const BUILD_JOB_BYTES: u64 = 8 << 30;

/// The profile `env` names that override the computed build limits.
pub const BUILD_ENV_OVERRIDES: &[&str] = &["MAX_JOBS", "FLASHINFER_NVCC_THREADS"];

/// `clamp(floor(available / 8 GiB), 1, cpus)`; an unknown `available` gives 1.
pub fn build_job_cap(available_bytes: Option<u64>, cpus: usize) -> usize {
    let by_memory = available_bytes.map_or(1, |bytes| bytes / BUILD_JOB_BYTES);
    usize::try_from(by_memory)
        .unwrap_or(usize::MAX)
        .clamp(1, cpus.max(1))
}

/// `MemAvailable` from `/proc/meminfo`, in bytes, if readable.
pub fn mem_available_bytes() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    parse_mem_available(&text)
}

fn parse_mem_available(meminfo: &str) -> Option<u64> {
    meminfo.lines().find_map(|line| {
        let rest = line.strip_prefix("MemAvailable:")?;
        let kib: u64 = rest.trim().strip_suffix("kB")?.trim().parse().ok()?;
        kib.checked_mul(1024)
    })
}

/// The CPUs this process may use.
pub fn cpu_count() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

/// The engine PATH: the engine's own bin, then `<cuda_home>/bin` when the
/// profile names one, then the fixed system directories.
pub fn tool_path(engine_bin: Option<&str>, cuda_home: Option<&str>, system: &str) -> String {
    let cuda_bin = cuda_home.map(|home| format!("{}/bin", home.trim_end_matches('/')));
    engine_bin
        .into_iter()
        .map(str::to_owned)
        .chain(cuda_bin)
        .chain(std::iter::once(system.to_owned()))
        .collect::<Vec<_>>()
        .join(":")
}

/// The toolchain variables one launch sets, and a log line naming the chosen
/// build limit and where it came from.
pub fn toolchain_environment(
    cuda_home: Option<&str>,
    overrides: &BTreeMap<String, String>,
    available_bytes: Option<u64>,
    cpus: usize,
) -> (BTreeMap<String, String>, String) {
    let mut env = BTreeMap::new();
    if let Some(home) = cuda_home {
        env.insert("CUDA_HOME".to_owned(), home.to_owned());
    }
    let cap = build_job_cap(available_bytes, cpus);
    let (jobs, source) = match overrides.get("MAX_JOBS") {
        Some(value) => (value.clone(), "profile env".to_owned()),
        None => (
            cap.to_string(),
            match available_bytes {
                Some(bytes) => format!(
                    "MemAvailable {} GiB / 8 GiB per job, {cpus} CPUs",
                    bytes >> 30
                ),
                None => "MemAvailable unreadable".to_owned(),
            },
        ),
    };
    env.insert("MAX_JOBS".to_owned(), jobs.clone());
    let threads = overrides
        .get("FLASHINFER_NVCC_THREADS")
        .cloned()
        .unwrap_or_else(|| "1".to_owned());
    env.insert("FLASHINFER_NVCC_THREADS".to_owned(), threads.clone());
    let line = format!(
        "engine build limits: MAX_JOBS={jobs} ({source}), FLASHINFER_NVCC_THREADS={threads}{}",
        cuda_home.map_or(String::new(), |home| format!(", CUDA_HOME={home}"))
    );
    (env, line)
}

/// The profile `env` entries that override the build limits.
pub fn build_overrides(profile_env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    profile_env
        .iter()
        .filter(|(name, _)| BUILD_ENV_OVERRIDES.contains(&name.as_str()))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    // T21 (SPEC §13.3 amendment): the job cap follows free memory, bounded by 1
    // and the CPU count.
    #[test]
    fn the_job_cap_is_free_memory_over_eight_gib_clamped() {
        assert_eq!(build_job_cap(Some(118 * GIB), 20), 14);
        assert_eq!(build_job_cap(Some(118 * GIB), 8), 8);
        assert_eq!(build_job_cap(Some(7 * GIB), 20), 1);
        assert_eq!(build_job_cap(Some(0), 20), 1);
        assert_eq!(build_job_cap(None, 20), 1);
        assert_eq!(build_job_cap(Some(64 * GIB), 0), 1);
    }

    #[test]
    fn mem_available_is_read_from_meminfo() {
        let text = "MemTotal:       130663170 kB\nMemFree:  1 kB\nMemAvailable:   123850952 kB\n";
        assert_eq!(parse_mem_available(text), Some(123_850_952 * 1024));
        assert_eq!(parse_mem_available("MemTotal: 1 kB\n"), None);
    }

    // T21: CUDA's bin joins PATH only when the profile names a CUDA home.
    #[test]
    fn the_cuda_bin_joins_path_only_when_named() {
        let system = "/usr/local/bin:/usr/bin:/bin";
        assert_eq!(
            tool_path(Some("/opt/venv/bin"), None, system),
            "/opt/venv/bin:/usr/local/bin:/usr/bin:/bin"
        );
        assert_eq!(
            tool_path(Some("/opt/venv/bin"), Some("/usr/local/cuda-13.0/"), system),
            "/opt/venv/bin:/usr/local/cuda-13.0/bin:/usr/local/bin:/usr/bin:/bin"
        );
        assert_eq!(tool_path(None, None, "/usr/bin:/bin"), "/usr/bin:/bin");
    }

    // T21: the computed limits are set and logged; a profile override wins.
    #[test]
    fn toolchain_variables_are_computed_and_overridable() {
        let (env, line) = toolchain_environment(
            Some("/usr/local/cuda"),
            &BTreeMap::new(),
            Some(40 * GIB),
            20,
        );
        assert_eq!(env["MAX_JOBS"], "5");
        assert_eq!(env["FLASHINFER_NVCC_THREADS"], "1");
        assert_eq!(env["CUDA_HOME"], "/usr/local/cuda");
        assert!(line.contains("MAX_JOBS=5"), "{line}");
        let overrides: BTreeMap<String, String> = [("MAX_JOBS".into(), "2".into())].into();
        let (env, line) = toolchain_environment(None, &overrides, Some(40 * GIB), 20);
        assert_eq!(env["MAX_JOBS"], "2");
        assert!(!env.contains_key("CUDA_HOME"));
        assert!(line.contains("profile env"), "{line}");
        let profile: BTreeMap<String, String> = [
            ("RUST_LOG".into(), "info".into()),
            ("FLASHINFER_NVCC_THREADS".into(), "2".into()),
        ]
        .into();
        assert_eq!(build_overrides(&profile).len(), 1);
    }
}
