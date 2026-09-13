//! Normalized, immutable engine launch choices. These are requests, not grants or evidence.

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "engine", rename_all = "lowercase")]
pub enum ProfileLaunchSettings {
    Vllm(VllmLaunchSettings),
    Sglang(SglangLaunchSettings),
    Fake(FakeLaunchSettings),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VllmLaunchSettings {
    pub tensor_parallel_size: u32,
    pub pipeline_parallel_size: u32,
    pub enable_sleep_mode: bool,
    pub kv_cache_dtype: String,
    pub block_size_tokens: u32,
    pub cpu_offload_bytes: i64,
    pub requested_budget: VllmRequestedBudget,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VllmRequestedBudget {
    pub kv_cache_bytes: i64,
    pub swap_space_bytes: i64,
    pub gpu_utilization_pct: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SglangLaunchSettings {
    pub recipe: String,
    pub tensor_parallel_size: u32,
    pub data_parallel_size: u32,
    pub tokenizer_workers: u32,
    pub model_dtype: String,
    pub context_tokens: u32,
    pub max_running_requests: u32,
    pub max_total_tokens: u32,
    pub prefill_cuda_graphs: bool,
    pub decode_cuda_graphs: bool,
    pub memory_saver: bool,
    pub cpu_weight_backup: bool,
    pub speculative_decoding: bool,
    pub lora: bool,
    pub trust_remote_code: bool,
    pub disaggregation: bool,
    pub external_cache: bool,
    pub cpu_kv_offload: bool,
    pub native_grpc: bool,
    pub weight_restore: String,
    pub requested_budget: SglangRequestedBudget,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SglangRequestedBudget {
    pub kv_cache_bytes: i64,
    pub static_memory_fraction_bps: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FakeLaunchSettings;
