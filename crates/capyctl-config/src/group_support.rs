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
            // ADR 0028 note of 2026-10-15 (MN4, SGLang 0.5.21, TP2 across two
            // GB10s): the group's first wake reloaded the head's weights for
            // 222 s, the canary ran, and 3 s later the wake was
            // restore_uncertain. A group restarts instead of parking in 0.1.3.
            deep_park: false,
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
