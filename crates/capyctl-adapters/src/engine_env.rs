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
//!   one job per core ran two hosts out of memory (found live). capyctl sets
//!   `clamp(floor((MemAvailable at launch − unified Ready charge) / 8 GiB), 1,
//!   CPU count)`. The unified Ready charge is what the deployment's Ready
//!   phase charges on unified-memory domains ([`unified_ready_bytes`]): the
//!   engines build their kernels once the model is resident (SGLang's and
//!   vLLM's FlashInfer JIT after the weights load, TensorFold's extensions on
//!   its first request), and on unified memory the model and the compilers
//!   draw from the one pool. It is zero on a discrete GPU, whose model sits in
//!   the GPU's own memory, so there `MemAvailable` alone decides, as before.
//! - `FLASHINFER_NVCC_THREADS`: threads inside each FlashInfer `nvcc`; each one
//!   multiplies a job's memory, so capyctl pins FlashInfer's own default of 1.
//!
//! The resolved engine env (ADR 0028 §2.1: the runtime profile's `env` and the
//! deployment's `engine_config.env`) may set `MAX_JOBS` or
//! `FLASHINFER_NVCC_THREADS` (positive integers, checked when it is resolved);
//! its value wins over the computed one. vLLM's `NVCC_THREADS` applies only
//! when vLLM itself is built, so it is not set.

use std::collections::BTreeMap;

/// SPEC §13.3 (amended 2026-09-25): the fixed system tool directories after
/// the engine's own bin and the profile's `<cuda_home>/bin`. Defined beside
/// the toolchain check, which looks tools up on the same PATH.
pub use capyctl_config::toolchain::SYSTEM_PATH;

/// Memory one JIT compile job is budgeted. Each fused-MoE `nvcc`/`cicc` job
/// held 7 to 9 GB on GB10 (found live 2026-09-25). Found live 2026-10-09
/// (SGLang 0.5.21, a model whose Ready charge is about 95 GiB on a GB10):
/// sized from `MemAvailable` before the model loaded, 14 jobs ran in the
/// roughly 20 GiB the loaded model left, the host reached 116.4 GiB and the
/// first start was killed for memory twice; 2 jobs, which is 20 GiB / 8 GiB,
/// built in a first start of 1745 s.
pub const BUILD_JOB_BYTES: u64 = 8 << 30;

/// `clamp(floor((available − committed) / 8 GiB), 1, cpus)`, where
/// `committed` is the memory the engine itself takes from the same pool
/// before its builds run ([`unified_ready_bytes`]); an unknown `available`
/// gives 1.
pub fn build_job_cap(available_bytes: Option<u64>, committed_bytes: u64, cpus: usize) -> usize {
    let by_memory = available_bytes.map_or(1, |bytes| {
        bytes.saturating_sub(committed_bytes) / BUILD_JOB_BYTES
    });
    usize::try_from(by_memory)
        .unwrap_or(usize::MAX)
        .clamp(1, cpus.max(1))
}

/// What the deployment's Ready phase charges on unified-memory domains, in
/// bytes: the memory its engine holds, out of the pool `MemAvailable`
/// measures, by the time its JIT builds run (ADR 0007 footprints; SPEC §7.2).
/// A discrete GPU's `device` domain and a `distinct` system domain add
/// nothing, so a discrete host's build limit is unchanged.
pub fn unified_ready_bytes(effective: &capyctl_config::effective::EffectiveDeployment) -> u64 {
    effective
        .resources
        .ready
        .allocations
        .iter()
        .filter(|allocation| {
            effective
                .host
                .domains
                .get(&allocation.domain)
                .is_some_and(|domain| {
                    domain.memory == capyctl_config::effective::DomainMemory::Unified
                })
        })
        .fold(0_u64, |total, allocation| {
            total.saturating_add(u64::try_from(allocation.bytes).unwrap_or(0))
        })
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
/// build limit and where it came from. `committed_bytes` is the deployment's
/// [`unified_ready_bytes`].
pub fn toolchain_environment(
    cuda_home: Option<&str>,
    overrides: &BTreeMap<String, String>,
    available_bytes: Option<u64>,
    committed_bytes: u64,
    cpus: usize,
) -> (BTreeMap<String, String>, String) {
    let mut env = BTreeMap::new();
    if let Some(home) = cuda_home {
        env.insert("CUDA_HOME".to_owned(), home.to_owned());
    }
    let cap = build_job_cap(available_bytes, committed_bytes, cpus);
    // ADR 0028 §2.1: the resolved engine env (the runtime profile's `env`
    // and the deployment's `engine_config.env`) wins over the computed cap.
    let (jobs, source) = match overrides.get("MAX_JOBS") {
        Some(value) => (value.clone(), "engine env".to_owned()),
        None => (
            cap.to_string(),
            match available_bytes {
                Some(bytes) if committed_bytes > 0 => format!(
                    "MemAvailable {} GiB less the {} GiB Ready charge on unified memory \
                     / 8 GiB per job, {cpus} CPUs",
                    bytes >> 30,
                    committed_bytes >> 30
                ),
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

/// ADR 0028 §2.1: the whole launch environment of an engine, in one place: the
/// resolved engine env first, then CapyCTL's own values, which win. `engine_exe`
/// is the engine executable; its directory leads the PATH. The second value is
/// the log line naming the chosen build limit.
pub fn launch_environment_noted(
    resolved: &BTreeMap<String, String>,
    engine_exe: Option<&str>,
    cuda_home: Option<&str>,
    available_bytes: Option<u64>,
    committed_bytes: u64,
    cpus: usize,
) -> (BTreeMap<String, String>, String) {
    let mut env = resolved.clone();
    let engine_bin = engine_exe
        .and_then(|exe| std::path::Path::new(exe).parent())
        .and_then(|dir| dir.to_str())
        .filter(|dir| !dir.is_empty());
    env.insert(
        "PATH".to_owned(),
        tool_path(engine_bin, cuda_home, SYSTEM_PATH),
    );
    let (toolchain, line) =
        toolchain_environment(cuda_home, resolved, available_bytes, committed_bytes, cpus);
    env.extend(toolchain);
    (env, line)
}

/// [`launch_environment_noted`] without the log line, for a launch whose
/// engine commits nothing on unified memory.
pub fn launch_environment(
    resolved: &BTreeMap<String, String>,
    engine_exe: Option<&str>,
    cuda_home: Option<&str>,
    available_bytes: u64,
    cpus: usize,
) -> BTreeMap<String, String> {
    launch_environment_noted(
        resolved,
        engine_exe,
        cuda_home,
        Some(available_bytes),
        0,
        cpus,
    )
    .0
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    // T37: the launch environment carries the resolved engine env; CapyCTL's own values win.
    #[test]
    fn launch_environment_includes_resolved_engine_env() {
        let resolved = BTreeMap::from([("MBX_FUSED_DRAFT".to_owned(), "1".to_owned())]);
        let env = launch_environment(&resolved, Some("/opt/venv/bin/vllm"), None, 8 << 30, 4);
        assert_eq!(env["MBX_FUSED_DRAFT"], "1");
        assert!(env["PATH"].starts_with("/opt/venv/bin"));
        // CapyCTL's computed limits and PATH are not replaced by a resolved name
        // of the same spelling beyond the documented MAX_JOBS override.
        let overridden = BTreeMap::from([("MAX_JOBS".to_owned(), "2".to_owned())]);
        let env = launch_environment(&overridden, None, None, 64 << 30, 8);
        assert_eq!(env["MAX_JOBS"], "2");
        assert_eq!(env["FLASHINFER_NVCC_THREADS"], "1");
        // ADR 0028 §2.1: CapyCTL's values are applied last. Owned names cannot
        // reach here through resolution, but the function holds regardless.
        let colliding = BTreeMap::from([
            ("PATH".to_owned(), "/evil".to_owned()),
            ("CUDA_HOME".to_owned(), "/evil".to_owned()),
        ]);
        let env = launch_environment(
            &colliding,
            Some("/opt/venv/bin/vllm"),
            Some("/cuda"),
            8 << 30,
            4,
        );
        assert!(env["PATH"].starts_with("/opt/venv/bin"));
        assert_eq!(env["CUDA_HOME"], "/cuda");
    }

    // T21 (SPEC §13.3 amendment): the job cap follows free memory, bounded by 1
    // and the CPU count.
    #[test]
    fn the_job_cap_is_free_memory_over_eight_gib_clamped() {
        assert_eq!(build_job_cap(Some(118 * GIB), 0, 20), 14);
        assert_eq!(build_job_cap(Some(118 * GIB), 0, 8), 8);
        assert_eq!(build_job_cap(Some(7 * GIB), 0, 20), 1);
        assert_eq!(build_job_cap(Some(0), 0, 20), 1);
        assert_eq!(build_job_cap(None, 0, 20), 1);
        assert_eq!(build_job_cap(Some(64 * GIB), 0, 0), 1);
    }

    // T21, SPEC §7.2 (found live 2026-10-09 on a GB10): the memory the engine
    // holds on unified memory once loaded is not the compilers'. 117 GiB
    // available at launch less a 95 GiB Ready charge leaves room for the 2
    // jobs that built where 14 were killed for memory; a charge at or above
    // what is available keeps the floor of 1.
    #[test]
    fn the_job_cap_leaves_out_the_unified_ready_charge() {
        assert_eq!(build_job_cap(Some(117 * GIB), 0, 20), 14);
        assert_eq!(build_job_cap(Some(117 * GIB), 95 * GIB, 20), 2);
        assert_eq!(build_job_cap(Some(117 * GIB), 110 * GIB, 20), 1);
        assert_eq!(build_job_cap(Some(60 * GIB), 95 * GIB, 20), 1);
        assert_eq!(build_job_cap(Some(117 * GIB), u64::MAX, 20), 1);
        assert_eq!(build_job_cap(None, 95 * GIB, 20), 1);
    }

    fn golden_sglang() -> capyctl_config::effective::EffectiveDeployment {
        let source: serde_json::Value = serde_json::from_str(include_str!(
            "../../capyctl-config/tests/fixtures/effective-sglang-golden.json"
        ))
        .unwrap();
        capyctl_config::effective::resolve_effective(
            &source["input"]["deployment"],
            &source["input"]["host"],
        )
        .unwrap()
    }

    // T26 (ADR 0019): only a unified domain's Ready charge is left out; a
    // discrete GPU's device domain and a distinct system domain add nothing,
    // so a discrete host's cap is unchanged.
    #[test]
    fn only_the_ready_charge_on_unified_memory_counts() {
        use capyctl_config::effective::DomainMemory;
        let unified = golden_sglang();
        // The golden deployment's Ready phase charges 8 GiB on `unified`.
        assert_eq!(unified_ready_bytes(&unified), 8 * GIB);
        for memory in [DomainMemory::Distinct, DomainMemory::Device] {
            let mut other = unified.clone();
            other.host.domains.get_mut("unified").unwrap().memory = memory;
            assert_eq!(unified_ready_bytes(&other), 0, "{memory:?}");
        }
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

    // T21: the computed limits are set and logged; the engine env's value wins.
    #[test]
    fn toolchain_variables_are_computed_and_overridable() {
        let (env, line) = toolchain_environment(
            Some("/usr/local/cuda"),
            &BTreeMap::new(),
            Some(40 * GIB),
            0,
            20,
        );
        assert_eq!(env["MAX_JOBS"], "5");
        assert_eq!(env["FLASHINFER_NVCC_THREADS"], "1");
        assert_eq!(env["CUDA_HOME"], "/usr/local/cuda");
        assert!(line.contains("MAX_JOBS=5"), "{line}");
        assert!(!line.contains("Ready charge"), "{line}");
        // SPEC §7.2: on unified memory the Ready charge is left out and named.
        let (env, line) =
            toolchain_environment(None, &BTreeMap::new(), Some(117 * GIB), 95 * GIB, 20);
        assert_eq!(env["MAX_JOBS"], "2");
        assert!(
            line.contains("MemAvailable 117 GiB less the 95 GiB Ready charge"),
            "{line}"
        );
        // ADR 0028 §2.1: a set MAX_JOBS wins, whatever the charge.
        let overrides: BTreeMap<String, String> = [("MAX_JOBS".into(), "6".into())].into();
        let (env, line) = toolchain_environment(None, &overrides, Some(117 * GIB), 95 * GIB, 20);
        assert_eq!(env["MAX_JOBS"], "6");
        assert!(!env.contains_key("CUDA_HOME"));
        assert!(line.contains("MAX_JOBS=6 (engine env)"), "{line}");
    }
}
