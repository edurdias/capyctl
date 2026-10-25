//! ADR 0028 §2, §10: what each engine's multi-node mode can run.
use crate::{topology::GroupShape, ConfigError, ConfigErrorCode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupSupport {
    pub max_tensor_parallel: Option<u32>,
    pub pipeline: bool,
    pub exact_hosts: Option<u32>,
    pub deep_park: bool,
    pub worker_listens: bool,
}

pub fn group_support(engine: &str) -> Option<GroupSupport> {
    match engine {
        "vllm" => Some(GroupSupport {
            max_tensor_parallel: None,
            pipeline: true,
            exact_hosts: None,
            deep_park: true,
            worker_listens: false,
        }),
        "sglang" => Some(GroupSupport {
            max_tensor_parallel: None,
            pipeline: true,
            exact_hosts: None,
            // ADR 0028 note of 2026-10-16: the group wake that failed live at
            // MN4 committed member reports observed before its readiness
            // probe and canary; a wake now commits reports observed after its
            // canary, and an SGLang group parks again.
            deep_park: true,
            worker_listens: true,
        }),
        // TensorFold 0.6.5: `--tp {1,2}`, one GPU per machine, no sleep.
        "tensorfold" => Some(GroupSupport {
            max_tensor_parallel: Some(2),
            pipeline: false,
            exact_hosts: Some(2),
            deep_park: false,
            worker_listens: false,
        }),
        _ => None,
    }
}

pub fn check_engine_shape(
    engine: &str,
    shape: &GroupShape,
    residency: &str,
) -> Result<(), ConfigError> {
    let unsupported = |detail: &str| {
        ConfigError::new(
            ConfigErrorCode::UnsupportedCombination,
            "topology",
            format!("group_shape_unsupported:{engine}: {detail}"),
        )
    };
    let Some(s) = group_support(engine) else {
        return Err(unsupported("no multi-node mode"));
    };
    if s.max_tensor_parallel
        .is_some_and(|m| shape.topology.tensor_parallel > m)
    {
        return Err(unsupported("tensor_parallel above the engine's limit"));
    }
    if !s.pipeline && shape.topology.pipeline_parallel > 1 {
        return Err(unsupported("no pipeline parallel"));
    }
    if s.exact_hosts.is_some_and(|n| shape.hosts.len() as u32 != n) {
        return Err(unsupported("host count"));
    }
    // Both parking tiers wake by reloading or restoring the engine, so an
    // engine whose group cannot park refuses `deep` and `host_backed` alike.
    if residency != "restart_only" && !s.deep_park {
        return Err(ConfigError::new(
            ConfigErrorCode::UnsupportedCombination,
            "residency",
            format!(
                "capability_missing:deep_park: a {engine} group cannot park \
                 (residency `{residency}`); declare `residency: restart_only`"
            ),
        ));
    }
    Ok(())
}

/// ADR 0035 §10: what each engine runs on several GPUs of one host. `None`
/// for an engine with no local multi-GPU mode (TensorFold until its own
/// change says otherwise, custom engines), which stays on one GPU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalSupport {
    /// The largest tensor-parallel size; `None` is any size up to the
    /// host's GPU count.
    pub max_tensor_parallel: Option<u32>,
    /// Pipeline parallel on one host (ADR 0035 owner decision 9: not in 0.1.9).
    pub pipeline: bool,
    /// Deep and host-backed parking at N > 1, enabled per engine only once
    /// its live row passes (ADR 0035 §9: MG4 for vLLM, MG5 for SGLang).
    pub deep_park: bool,
    /// llama.cpp's `--split-mode` values CapyCTL renders (ADR 0035 §4,
    /// owner decision 5: `layer` only); empty for the tensor-parallel engines.
    pub split_modes: &'static [&'static str],
}

/// One arm per engine: `deep_park` flips per engine as its live row passes
/// (ADR 0035 §9: MG4 for vLLM, MG5 for SGLang), and nothing else gates
/// parking at N > 1 (resolution, the sleep and saver launch flags and the
/// per-GPU park evidence all follow it).
pub fn local_support(engine: &str) -> Option<LocalSupport> {
    match engine {
        "vllm" => Some(LocalSupport {
            max_tensor_parallel: None,
            pipeline: false,
            deep_park: false,
            split_modes: &[],
        }),
        "sglang" => Some(LocalSupport {
            max_tensor_parallel: None,
            pipeline: false,
            deep_park: false,
            split_modes: &[],
        }),
        "llamacpp" => Some(LocalSupport {
            max_tensor_parallel: None,
            pipeline: false,
            deep_park: false,
            split_modes: &["layer"],
        }),
        _ => None,
    }
}
