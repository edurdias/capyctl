//! Test scaffolding for capyctl: the Fake engine, the fake multi-host engine
//! group, the fake launcher, the lifecycle simulation, the shared store
//! fixture and the scripted process tools.
//!
//! None of this is a product. The Fake engine reports Ready without an engine
//! behind it, which is exactly what makes it useful in a test and exactly what
//! must never reach an operator's machine, so it lives in a crate every other
//! crate reaches through `[dev-dependencies]` alone.
//! `scripts/check-release-clean.sh` is what keeps that true.
//!
//! **Passing anything here is not qualification.** A native engine recipe is
//! qualified on the host it runs on; a green Fake test says only that capyctl's own
//! decisions were the ones expected of it (SPEC §18).
//!
//! Tests reach the Fake through two seams the product already has:
//! [`fake_provider`] supplies it as the host's one engine installation, and
//! [`fake_bindings`] supplies it as the adapter a frozen binding resolves to.
//! Neither adds a lane to the coordinator that production does not take.

pub mod fake_engine;
pub mod fake_group;
pub mod fake_launcher;
pub mod fixture;
mod lifecycle;
pub mod ports;
mod provider;
mod scripted_tool;

pub use fake_engine::{FakeEngine, BUFFER_RESIDUE, FULL_RESIDENT_BYTES, LEVEL1_RETAINED_BYTES};
pub use fake_group::{FakeGroup, JournalState, RankEnd};
pub use fake_launcher::FakeLauncher;
pub use lifecycle::FakeFault;
pub use provider::{
    fake_bindings, fake_bindings_with_members, fake_installation, fake_provider,
    fake_tools_factory, live_identity, spawn_fake_coordinator, NO_NETWORK_ORIGIN,
};
pub use scripted_tool::ScriptedTool;

use capyctl_domain::launch::{
    CommonEngineSettings, LaunchSettings, MemoryRequest, SettingSource, SglangLaunchSettings,
    VllmLaunchSettings,
};

/// The launch settings a test deployment carries.
///
/// Test deployments describe a vLLM installation, because that is the family the
/// product has: the Fake is injected as the adapter, not declared in a document.
/// The injected builder reads none of these fields, so they only have to be a
/// value the configuration layer could produce.
pub fn vllm_launch_settings() -> LaunchSettings {
    LaunchSettings::Vllm(VllmLaunchSettings {
        common: CommonEngineSettings::default(),
        memory: MemoryRequest {
            request_bytes: 8 << 30,
            kv_cache_bytes: 4 << 30,
            margin_bytes: 8 << 30,
            weights_bytes: None,
            startup_bytes: None,
            device_total_bytes: None,
            overhead_bytes: None,
            startup_graphs_bytes: None,
            state_slot_bytes: None,
            state_bytes: None,
            member: None,
            disk_tables: None,
        },
        block_size_tokens: None,
        max_num_batched_tokens: None,
        safetensors_load_strategy: None,
        tool_call_parser: None,
        reasoning_parser: None,
        enable_sleep_mode: false,
        extra_args: Vec::new(),
        provenance: [
            ("enable_sleep_mode".to_owned(), SettingSource::Derived),
            ("memory.request".to_owned(), SettingSource::Derived),
        ]
        .into_iter()
        .collect(),
    })
}

/// SGLang settings equivalent to the pinned Qwen3-4B recipe the protected entry
/// still validates (ADR 0014 WE1 interim): a deep-parking deployment that states
/// only its KV cache. Not qualification evidence.
pub fn sglang_launch_settings() -> SglangLaunchSettings {
    SglangLaunchSettings {
        common: CommonEngineSettings {
            cuda_graphs: Some(false),
            ..CommonEngineSettings::default()
        },
        // Request = weights (4 GiB here) + KV + margin (ADR 0014 §5), so the
        // static share SGLang sizes from the grant is 8 GiB.
        memory: MemoryRequest {
            request_bytes: 16 << 30,
            kv_cache_bytes: 4 << 30,
            margin_bytes: 8 << 30,
            weights_bytes: None,
            startup_bytes: None,
            device_total_bytes: None,
            overhead_bytes: None,
            startup_graphs_bytes: None,
            state_slot_bytes: None,
            state_bytes: None,
            member: None,
            disk_tables: None,
        },
        max_total_tokens: None,
        max_mamba_cache_size: None,
        static_allowance_bytes: None,
        chunked_prefill_size: None,
        tokenizer_workers: 1,
        tool_call_parser: None,
        reasoning_parser: None,
        memory_saver: true,
        cpu_weight_backup: false,
        weight_restore: "disk_reload".into(),
        extra_args: Vec::new(),
        provenance: [
            ("cuda_graphs", SettingSource::CapyctlDefault),
            ("sglang.tokenizer_workers", SettingSource::CapyctlDefault),
            ("memory_saver", SettingSource::Derived),
            ("cpu_weight_backup", SettingSource::Derived),
            ("weight_restore", SettingSource::Derived),
            ("memory.request", SettingSource::Derived),
        ]
        .into_iter()
        .map(|(field, source)| (field.to_owned(), source))
        .collect(),
    }
}

/// ADR 0023: TensorFold settings with a fixed context.
pub fn tensorfold_launch_settings() -> LaunchSettings {
    LaunchSettings::Tensorfold(capyctl_domain::launch::TensorfoldLaunchSettings {
        common: CommonEngineSettings {
            context_length: Some(8192),
            ..CommonEngineSettings::default()
        },
        memory: MemoryRequest {
            request_bytes: 30 << 30,
            kv_cache_bytes: 4 << 30,
            margin_bytes: 0,
            weights_bytes: None,
            startup_bytes: None,
            device_total_bytes: None,
            overhead_bytes: None,
            startup_graphs_bytes: None,
            state_slot_bytes: None,
            state_bytes: None,
            member: None,
            disk_tables: None,
        },
        max_tokens: None,
        thinking: None,
        extra_args: Vec::new(),
        provenance: Default::default(),
    })
}

/// The `engine_config` block a test deployment carries (ADR 0014 §2): JSON,
/// validated by the configuration layer rather than by a second parser here.
pub fn vllm_engine_config_json() -> serde_json::Value {
    serde_json::json!({"memory": {"kv_cache": "4GiB"}})
}
