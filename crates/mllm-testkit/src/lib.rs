//! Test scaffolding for mllm: the Fake engine, the fake launcher, the
//! lifecycle simulation, the shared store fixture and the scripted process
//! tools.
//!
//! None of this is a product. The Fake engine reports Ready without an engine
//! behind it, which is exactly what makes it useful in a test and exactly what
//! must never reach an operator's machine, so it lives in a crate every other
//! crate reaches through `[dev-dependencies]` alone.
//! `scripts/check-release-clean.sh` is what keeps that true.
//!
//! **Passing anything here is not qualification.** A native engine recipe is
//! qualified on the host it runs on; a green Fake test says only that mllm's own
//! decisions were the ones expected of it (SPEC §18).
//!
//! Tests reach the Fake through two seams the product already has:
//! [`fake_provider`] supplies it as the host's one engine installation, and
//! [`fake_bindings`] supplies it as the adapter a frozen binding resolves to.
//! Neither adds a lane to the coordinator that production does not take.

pub mod fake_engine;
pub mod fake_launcher;
pub mod fixture;
mod lifecycle;
mod provider;
mod scripted_tool;

pub use fake_engine::{FakeEngine, BUFFER_RESIDUE, FULL_RESIDENT_BYTES, LEVEL1_RETAINED_BYTES};
pub use fake_launcher::FakeLauncher;
pub use lifecycle::FakeFault;
pub use provider::{
    fake_bindings, fake_installation, fake_provider, fake_tools_factory, spawn_fake_coordinator,
};
pub use scripted_tool::ScriptedTool;

use mllm_domain::launch::{ProfileLaunchSettings, VllmLaunchSettings, VllmRequestedBudget};

/// The launch settings a test deployment carries.
///
/// Test deployments describe a vLLM installation, because that is the family the
/// product has: the Fake is injected as the adapter, not declared in a document.
/// The injected builder reads none of these fields, so they only have to be a
/// block the configuration layer accepts.
pub fn vllm_launch_settings() -> ProfileLaunchSettings {
    ProfileLaunchSettings::Vllm(VllmLaunchSettings {
        tensor_parallel_size: 1,
        pipeline_parallel_size: 1,
        enable_sleep_mode: true,
        kv_cache_dtype: "auto".into(),
        block_size_tokens: 16,
        cpu_offload_bytes: 0,
        requested_budget: VllmRequestedBudget {
            kv_cache_bytes: 4 << 30,
            swap_space_bytes: 0,
            gpu_utilization_pct: 75,
        },
    })
}

/// The same block as a host policy carries it: JSON, validated by the
/// configuration layer rather than by a second parser here.
pub fn vllm_launch_settings_json() -> serde_json::Value {
    serde_json::json!({
        "engine": "vllm",
        "tensor_parallel_size": 1,
        "pipeline_parallel_size": 1,
        "enable_sleep_mode": true,
        "kv_cache_dtype": "auto",
        "block_size_tokens": 16,
        "cpu_offload_bytes": "0B",
        "requested_budget": {
            "kv_cache_bytes": "4GiB",
            "swap_space_bytes": "0B",
            "gpu_utilization_pct": 75
        }
    })
}
