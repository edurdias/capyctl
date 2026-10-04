use super::*;
use capyctl_domain::launch::CommonEngineSettings;
use serde_json::json;

const GIB: i64 = 1 << 30;

/// Qwen3.8-27B NVFP4's language model: 48 gated-delta-net layers and 16
/// attention layers with 4 KV heads of 256. SGLang's state per request is
/// 48 × (10240 × 3 × 2 + 48 × 128 × 128 × 4) = 153944064 bytes ("146.81 MB"
/// in its own log, found live 2026-10-02).
fn qwen38_27b() -> Value {
    let mut types = Vec::new();
    for _ in 0..16 {
        types.extend(["linear_attention", "linear_attention", "linear_attention"]);
        types.push("full_attention");
    }
    json!({"model_type": "qwen3_5", "text_config": {
        "model_type": "qwen3_5_text", "num_hidden_layers": 64, "num_attention_heads": 24,
        "num_key_value_heads": 4, "head_dim": 256, "hidden_size": 5120,
        "max_position_embeddings": 262144, "dtype": "bfloat16", "layer_types": types,
        "full_attention_interval": 4, "linear_conv_kernel_dim": 4, "linear_key_head_dim": 128,
        "linear_num_key_heads": 16, "linear_num_value_heads": 48, "linear_value_head_dim": 128,
        "mamba_ssm_dtype": "float32",
    }})
}

const STATE: u64 = 153_944_064;

/// The DFlash2 draft model: 5 sliding-window layers SGLang counts in full,
/// 8 KV heads of 128.
fn dflash_drafter() -> Value {
    json!({
        "architectures": ["DFlash2DraftModel"], "model_type": "qwen3",
        "num_hidden_layers": 5, "num_attention_heads": 32, "num_key_value_heads": 8,
        "head_dim": 128, "hidden_size": 5120, "max_position_embeddings": 262144,
        "dtype": "bfloat16", "sliding_window": 2048, "use_sliding_window": true,
        "layer_types": ["sliding_attention", "sliding_attention", "sliding_attention",
                        "sliding_attention", "sliding_attention"],
    })
}

/// A dense model at 512 KiB of bfloat16 KV per token.
fn dense() -> Value {
    json!({
        "num_hidden_layers": 32, "num_attention_heads": 32, "hidden_size": 4096,
        "max_position_embeddings": 32768, "torch_dtype": "bfloat16",
    })
}

fn settings(request: i64, kv: i64, weights: Option<i64>) -> SglangLaunchSettings {
    SglangLaunchSettings {
        common: CommonEngineSettings::default(),
        memory: MemoryRequest {
            request_bytes: request,
            kv_cache_bytes: kv,
            margin_bytes: 8 * GIB,
            weights_bytes: weights,
            startup_bytes: None,
            device_total_bytes: None,
            overhead_bytes: None,
            startup_graphs_bytes: None,
        },
        max_total_tokens: None,
        max_mamba_cache_size: None,
        chunked_prefill_size: None,
        tokenizer_workers: 1,
        tool_call_parser: None,
        reasoning_parser: None,
        memory_saver: true,
        cpu_weight_backup: false,
        weight_restore: "disk_reload".into(),
        extra_args: Vec::new(),
        provenance: Default::default(),
    }
}

fn fp8(mut settings: SglangLaunchSettings) -> SglangLaunchSettings {
    settings.common.kv_cache_dtype = Some("fp8_e4m3".into());
    settings
}

fn dflash(mut settings: SglangLaunchSettings) -> SglangLaunchSettings {
    settings.extra_args = [
        "--speculative-algorithm",
        "DFLASH",
        "--speculative-draft-model-path",
        "/drafters/dflash2",
        "--speculative-num-draft-tokens",
        "8",
    ]
    .map(String::from)
    .to_vec();
    settings
}

fn pool(settings: &SglangLaunchSettings, config: &Value) -> Result<SglangPool, String> {
    sglang_pool(settings, &[], Ok(config), None)
}

// T14 (owner decision 2026-10-03): a dense model's KV pool is the KV cache
// in tokens, in the KV dtype; nothing else is sized.
#[test]
fn a_dense_models_kv_pool_is_the_kv_cache_in_tokens() {
    let dense = dense();
    let found = pool(&settings(16 * GIB, 4 * GIB, None), &dense).unwrap();
    assert_eq!(found.max_total_tokens, Some(8192));
    assert_eq!(found.max_running_requests, None);
    assert_eq!(found.max_mamba_cache_size, None);
    let found = pool(&fp8(settings(16 * GIB, 4 * GIB, None)), &dense).unwrap();
    assert_eq!(found.max_total_tokens, Some(16384));
    // A declared max_total_tokens is the deployment's own pool.
    let mut declared = settings(16 * GIB, 4 * GIB, None);
    declared.max_total_tokens = Some(1000);
    assert_eq!(pool(&declared, &dense).unwrap().max_total_tokens, None);
}

// T14: a gated-delta-net hybrid keeps KV for its attention layers only (16 ×
// 2 × 4 × 256 = 32 KiB of fp8 per token), and its recurrent state is sized
// for the most running requests (up to CapyCTL's in-flight bound) that fit
// the static pool beside the weights and the KV cache.
#[test]
fn a_hybrid_models_state_is_sized_beside_its_kv_cache() {
    let target = qwen38_27b();
    let weights = 21_920_000_000;
    let found = pool(&fp8(settings(48 * GIB, 16 * GIB, Some(weights))), &target).unwrap();
    // 16 GiB / 32 KiB.
    assert_eq!(found.max_total_tokens, Some(524288));
    // Room: 40 GiB static - weights - 16 GiB = 3849803776 bytes, 25 slots;
    // 4 requests keep 21.
    assert_eq!(found.max_running_requests, Some(4));
    assert_eq!(found.max_mamba_cache_size, Some(20));
    assert_eq!(found.running_limit, Some(4));
    // The static pool holds weights, KV, state and SGLang's own allocations.
    let overhead = STATIC_OVERHEAD_BYTES as i64;
    assert_eq!(
        found.static_bytes,
        Some(weights + 16 * GIB + 21 * STATE as i64 + overhead)
    );
    // A larger request runs CapyCTL's whole in-flight bound.
    let found = pool(&fp8(settings(96 * GIB, 16 * GIB, Some(weights))), &target).unwrap();
    assert_eq!(
        found.max_running_requests,
        Some(MAX_REQUESTS_PER_DEPLOYMENT)
    );
    assert_eq!(found.max_mamba_cache_size, Some(160));
    assert_eq!(found.running_limit, None);
    // A declared count is used as declared, and not repeated.
    let mut declared = fp8(settings(48 * GIB, 16 * GIB, Some(weights)));
    declared.common.max_concurrent_requests = Some(2);
    let found = pool(&declared, &target).unwrap();
    assert_eq!(found.max_running_requests, None);
    assert_eq!(found.max_mamba_cache_size, Some(10));
}

// T14 (found live 2026-10-03): with DFlash2 (8 draft tokens) SGLang also
// keeps (running + 1) × 8 intermediate states; the draft model's KV layers
// (5 × 2 × 8 × 128 fp8 bytes) share the KV pool.
#[test]
fn a_draft_model_adds_its_kv_and_intermediate_states() {
    let target = qwen38_27b();
    let drafter = dflash_drafter();
    let weights = 25_770_000_000;
    let mut declared = dflash(fp8(settings(72 * GIB, 16 * GIB, Some(weights))));
    declared.common.max_concurrent_requests = Some(8);
    let found = sglang_pool(&declared, &[], Ok(&target), Some(Ok(&drafter))).unwrap();
    // 16 GiB / (32768 + 10240) bytes, rounded down.
    assert_eq!(found.max_total_tokens, Some(399457));
    assert_eq!(found.max_mamba_cache_size, Some(40));
    assert_eq!(found.max_running_requests, None);
    // The recipe's 48 GiB request leaves no room for that state: refused,
    // naming the request that would hold it.
    declared.memory.request_bytes = 48 * GIB;
    let refusal = sglang_pool(&declared, &[], Ok(&target), Some(Ok(&drafter))).unwrap_err();
    let needed = (8 * 5 + 1 + 9 * 8) * STATE;
    assert!(refusal.contains(&format!("{needed} bytes")), "{refusal}");
    assert!(
        refusal.contains("engine_config.memory.request")
            && refusal.contains("max_concurrent_requests"),
        "{refusal}"
    );
    // Undeclared, and not even one running request fits: refused too.
    declared.common.max_concurrent_requests = None;
    let refusal = sglang_pool(&declared, &[], Ok(&target), Some(Ok(&drafter))).unwrap_err();
    assert!(refusal.contains("1 running request "), "{refusal}");
    assert!(!refusal.contains("max_concurrent_requests"), "{refusal}");
}

// T14 (#40's rule): arguments that size the state pool own it; CapyCTL
// passes nothing for it and still sizes the KV pool.
#[test]
fn arguments_that_size_the_state_pool_win() {
    let target = qwen38_27b();
    for extra in [
        ["--max-mamba-cache-size", "64"],
        ["--mamba-full-memory-ratio", "0.5"],
    ] {
        let mut owned = fp8(settings(48 * GIB, 16 * GIB, Some(21_920_000_000)));
        owned.extra_args = extra.map(String::from).to_vec();
        let found = pool(&owned, &target).unwrap();
        assert_eq!(found.max_mamba_cache_size, None, "{extra:?}");
        assert_eq!(found.max_running_requests, None, "{extra:?}");
        assert_eq!(found.max_total_tokens, Some(524288), "{extra:?}");
        // The installation's host-fixed arguments too.
        let host = fp8(settings(48 * GIB, 16 * GIB, Some(21_920_000_000)));
        let found = sglang_pool(&host, &extra.map(String::from), Ok(&target), None).unwrap();
        assert_eq!(found.max_mamba_cache_size, None, "{extra:?}");
    }
    // `--mamba-ssm-dtype bfloat16` halves the temporal state: 48 ×
    // (61440 + 1572864) bytes per slot, so 9 requests fit where 4 did.
    let mut halved = fp8(settings(48 * GIB, 16 * GIB, Some(21_920_000_000)));
    halved.extra_args = ["--mamba-ssm-dtype", "bfloat16"].map(String::from).to_vec();
    assert_eq!(
        pool(&halved, &target).unwrap().max_running_requests,
        Some(9)
    );
}

// T14: what CapyCTL cannot model is left to SGLang, with the reason.
#[test]
fn unmodelled_shapes_are_left_to_sglang() {
    let sliding = json!({
        "num_hidden_layers": 4, "num_attention_heads": 8, "hidden_size": 1024,
        "max_position_embeddings": 4096, "torch_dtype": "bfloat16", "sliding_window": 1024,
    });
    let found = pool(&settings(16 * GIB, 4 * GIB, None), &sliding).unwrap();
    assert_eq!(
        found,
        SglangPool {
            reason: found.reason.clone(),
            ..SglangPool::default()
        }
    );
    assert!(found.reason.unwrap().contains("SGLang sizes"));
    let unread = sglang_pool(
        &settings(16 * GIB, 4 * GIB, None),
        &[],
        Err("no config".into()),
        None,
    )
    .unwrap();
    assert_eq!(unread.max_total_tokens, None);
    // Without the weights the room is unknown: KV tokens and a declared
    // count's state only.
    let target = qwen38_27b();
    let found = pool(&fp8(settings(48 * GIB, 16 * GIB, None)), &target).unwrap();
    assert_eq!(found.max_total_tokens, Some(524288));
    assert_eq!(found.max_mamba_cache_size, None);
    let mut declared = fp8(settings(48 * GIB, 16 * GIB, None));
    declared.common.max_concurrent_requests = Some(8);
    assert_eq!(
        pool(&declared, &target).unwrap().max_mamba_cache_size,
        Some(40)
    );
    // Speculative decoding with no draft-token count: the state is SGLang's.
    let mut unknown = fp8(settings(96 * GIB, 16 * GIB, Some(21_920_000_000)));
    unknown.extra_args = ["--speculative-algorithm", "NEXTN"]
        .map(String::from)
        .to_vec();
    assert_eq!(pool(&unknown, &target).unwrap().max_mamba_cache_size, None);
}

// T14: SGLang's static pool, shared by the renderer and the fit.
#[test]
fn the_static_pool_is_the_request_less_the_margin() {
    let unified = settings(48 * GIB, 16 * GIB, None);
    assert_eq!(static_pool_bytes(&unified.memory), (8 * GIB, 40 * GIB));
    let mut discrete = settings(11 * GIB + 4 * GIB, 4 * GIB, None);
    discrete.memory.device_total_bytes = Some(16 * GIB);
    assert_eq!(static_pool_bytes(&discrete.memory), (GIB, 14 * GIB));
    // A request smaller than KV plus margin gives the static pool the KV cache.
    let small = settings(10 * GIB, 4 * GIB, None);
    assert_eq!(static_pool_bytes(&small.memory), (8 * GIB, 4 * GIB));
}

/// A request CapyCTL derived: weights + KV + the 8 GiB margin.
fn derived(kv: i64, weights: i64) -> SglangLaunchSettings {
    let mut derived = fp8(settings(weights + kv + 8 * GIB, kv, Some(weights)));
    derived.provenance.insert(
        "memory.request".into(),
        capyctl_domain::launch::SettingSource::Derived,
    );
    derived
}

// T14 (owner decision 2026-10-03): a derived request holds nothing for the
// state, so it fits what it can with up to half of the margin, limits the
// running requests to that, and refuses only when one request does not fit.
#[test]
fn a_derived_request_fits_what_it_can() {
    let target = qwen38_27b();
    let drafter = dflash_drafter();
    let found = pool(&derived(4 * GIB, 21_920_000_000), &target).unwrap();
    // 4 GiB holds 27 slots: 5 requests keep 26.
    assert_eq!(found.max_running_requests, Some(5));
    assert_eq!(found.max_mamba_cache_size, Some(25));
    assert_eq!(found.running_limit, Some(5));
    assert_eq!(
        found.static_bytes,
        Some(21_920_000_000 + 4 * GIB + 26 * STATE as i64 + STATIC_OVERHEAD_BYTES as i64)
    );
    // A declared count is lowered to what fits rather than refused.
    let mut declared = dflash(derived(16 * GIB, 25_770_000_000));
    declared.common.max_concurrent_requests = Some(8);
    let found = sglang_pool(&declared, &[], Ok(&target), Some(Ok(&drafter))).unwrap();
    assert_eq!(found.max_running_requests, Some(1));
    assert_eq!(found.max_mamba_cache_size, Some(5));
    assert_eq!(found.running_limit, Some(1));
    assert_eq!(
        found.static_bytes,
        Some(25_770_000_000 + 16 * GIB + 22 * STATE as i64 + STATIC_OVERHEAD_BYTES as i64)
    );
    // One request that does not fit half the margin is refused.
    declared.extra_args[5] = "32".into();
    let refusal = sglang_pool(&declared, &[], Ok(&target), Some(Ok(&drafter))).unwrap_err();
    assert!(refusal.contains("1 running request "), "{refusal}");
    // An explicit request of the same size is strict.
    let mut explicit = dflash(derived(16 * GIB, 25_770_000_000));
    explicit.provenance.clear();
    explicit.common.max_concurrent_requests = Some(8);
    let refusal = sglang_pool(&explicit, &[], Ok(&target), Some(Ok(&drafter))).unwrap_err();
    assert!(refusal.contains("8 running requests"), "{refusal}");
}

// T14 (found live 2026-10-03): a dense model's static pool also holds
// SGLang's own allocations beside the weights and the KV cache, from the
// margin, so the KV pool is not cut short.
#[test]
fn a_dense_static_pool_holds_the_overhead() {
    let found = pool(&settings(16 * GIB, 4 * GIB, Some(4 * GIB)), &dense()).unwrap();
    assert_eq!(found.static_bytes, Some(10 * GIB));
    // Never above the request, nor on a discrete device.
    let found = pool(&settings(9 * GIB, 4 * GIB, Some(4 * GIB)), &dense()).unwrap();
    assert_eq!(found.static_bytes, Some(9 * GIB));
    let mut discrete = settings(16 * GIB, 4 * GIB, Some(4 * GIB));
    discrete.memory.device_total_bytes = Some(16 * GIB);
    assert_eq!(pool(&discrete, &dense()).unwrap().static_bytes, None);
}

/// The memory request a refusal names, in MiB.
fn named_request_mib(refusal: &str) -> i64 {
    let tail = refusal
        .split("raise engine_config.memory.request to \"")
        .nth(1)
        .unwrap_or_else(|| panic!("no named request: {refusal}"));
    tail.split("MiB\"").next().unwrap().parse().unwrap()
}

/// `settings` with an explicit request of `mib` MiB.
fn at_mib(settings: &SglangLaunchSettings, mib: i64) -> SglangLaunchSettings {
    let mut explicit = settings.clone();
    explicit.provenance.clear();
    explicit.memory.request_bytes = mib << 20;
    explicit
}

// Found live 2026-10-04 (FrogNano NVFP4 on SGLang 0.5.21): a refusal named
// 27.88 GB at a 24 GiB request and 30.03 GB at 26 GiB, while the state needed
// weights + KV + state + the 8 GiB margin (31 GiB started). The request a
// refusal names is the smallest whole MiB the same check accepts: one MiB
// less is refused again.
#[test]
fn a_refusal_names_the_smallest_request_that_fits() {
    let target = qwen38_27b();
    let drafter = dflash_drafter();
    let check = |settings: &SglangLaunchSettings| {
        sglang_pool(settings, &[], Ok(&target), Some(Ok(&drafter)))
    };
    let weights = 25_770_000_000;
    let mut cases = Vec::new();
    // Weights and KV beyond the static pool: the room was floored at zero.
    cases.push(dflash(fp8(settings(30 * GIB, 16 * GIB, Some(weights)))));
    // Some room, not enough for the declared requests.
    let mut declared = dflash(fp8(settings(48 * GIB, 16 * GIB, Some(weights))));
    declared.common.max_concurrent_requests = Some(8);
    cases.push(declared);
    // Without speculative decoding, one request at a small request.
    cases.push(fp8(settings(24 * GIB, 4 * GIB, Some(21_920_000_000))));
    // A discrete device: the margin is a tenth of the weights share.
    let mut discrete = fp8(settings(20 * GIB, 2 * GIB, Some(17 * GIB)));
    discrete.memory.device_total_bytes = Some(48 * GIB);
    cases.push(discrete);
    // A derived request whose one request does not fit half the margin.
    let mut derived = dflash(derived(16 * GIB, weights));
    derived.extra_args[5] = "32".into();
    cases.push(derived);
    for case in cases {
        let refusal = check(&case).unwrap_err();
        let named = named_request_mib(&refusal);
        assert!(named << 20 > case.memory.request_bytes, "{refusal}");
        assert!(
            check(&at_mib(&case, named)).is_ok(),
            "{named} MiB: {refusal}"
        );
        assert!(
            check(&at_mib(&case, named - 1)).is_err(),
            "{named} MiB: {refusal}"
        );
    }
}

/// FrogNano-4B-2609's language model: 24 gated-delta-net layers and 8
/// attention layers with 4 KV heads of 256 (32 KiB of bfloat16 KV per
/// token). SGLang's state per request is 24 × (8192 × 3 × 2 + 32 × 128 ×
/// 128 × 4) = 51511296 bytes.
fn frognano_4b() -> Value {
    let mut types = Vec::new();
    for _ in 0..8 {
        types.extend(["linear_attention", "linear_attention", "linear_attention"]);
        types.push("full_attention");
    }
    json!({"model_type": "qwen3_5", "text_config": {
        "model_type": "qwen3_5_text", "num_hidden_layers": 32, "num_attention_heads": 16,
        "num_key_value_heads": 4, "head_dim": 256, "hidden_size": 2560,
        "max_position_embeddings": 262144, "dtype": "bfloat16", "layer_types": types,
        "full_attention_interval": 4, "linear_conv_kernel_dim": 4, "linear_key_head_dim": 128,
        "linear_num_key_heads": 16, "linear_num_value_heads": 32, "linear_value_head_dim": 128,
        "mamba_ssm_dtype": "float32",
    }})
}

const FROGNANO_STATE: u64 = 51_511_296;
const FROGNANO_WEIGHTS: i64 = 9_319_820_920;
/// The default KV cache on a 16 GB laptop GPU: a quarter of its managed limit.
const LAPTOP_KV: i64 = 3_949_440_534;

/// A request CapyCTL derived on a discrete GPU: the weights x 1.10 plus the
/// KV cache (discrete GPU design §3), beside SGLang's family margin.
fn derived_discrete(kv: i64, weights: i64) -> SglangLaunchSettings {
    let mut derived = settings(weights / 100 * 110 + kv, kv, Some(weights));
    derived.memory.device_total_bytes = Some(17_171_480_576);
    derived.provenance.insert(
        "memory.request".into(),
        capyctl_domain::launch::SettingSource::Derived,
    );
    derived
}

// Found live 2026-10-04 (FrogNano-4B BF16, SGLang 0.5.21, a 16 GB laptop
// GPU): a derived request on a discrete device leaves no room beside the
// weights and the KV cache, and every start was refused for one running
// request. CapyCTL chose that KV cache, so the state takes up to half of it
// and the KV pool holds the rest.
#[test]
fn a_derived_discrete_request_keeps_the_state_in_its_kv_cache() {
    let target = frognano_4b();
    let found = pool(&derived_discrete(LAPTOP_KV, FROGNANO_WEIGHTS), &target).unwrap();
    // Half the KV cache holds 7 requests: 36 slots of state.
    let state = 36 * FROGNANO_STATE;
    assert!(state <= LAPTOP_KV as u64 / 2 && 41 * FROGNANO_STATE > LAPTOP_KV as u64 / 2);
    assert_eq!(found.max_running_requests, Some(7));
    assert_eq!(found.max_mamba_cache_size, Some(35));
    assert_eq!(found.running_limit, Some(7));
    assert_eq!(
        found.max_total_tokens,
        Some(((LAPTOP_KV as u64 - state) / 32768) as u32)
    );
    // A discrete device keeps its static pool.
    assert_eq!(found.static_bytes, None);
    // A declared count that fits is kept.
    let mut declared = derived_discrete(LAPTOP_KV, FROGNANO_WEIGHTS);
    declared.common.max_concurrent_requests = Some(4);
    let found = pool(&declared, &target).unwrap();
    assert_eq!(found.max_running_requests, None);
    assert_eq!(found.max_mamba_cache_size, Some(20));
    assert_eq!(found.running_limit, None);
    assert_eq!(
        found.max_total_tokens,
        Some(((LAPTOP_KV as u64 - 21 * FROGNANO_STATE) / 32768) as u32)
    );
    // An explicit request of the same size is strict, and refused.
    let mut explicit = derived_discrete(LAPTOP_KV, FROGNANO_WEIGHTS);
    explicit.provenance.clear();
    let refusal = pool(&explicit, &target).unwrap_err();
    assert!(refusal.contains("1 running request "), "{refusal}");
    // A KV cache whose half holds no request's state is refused.
    let refusal = pool(&derived_discrete(256 << 20, FROGNANO_WEIGHTS), &target).unwrap_err();
    assert!(refusal.contains("1 running request "), "{refusal}");
}

// T14: unified memory is unchanged: the state borrows half the margin and
// the KV pool is the whole KV cache.
#[test]
fn a_derived_unified_request_keeps_its_kv_pool() {
    let target = frognano_4b();
    let mut unified = derived(4 * GIB, FROGNANO_WEIGHTS);
    unified.common.kv_cache_dtype = None;
    let found = pool(&unified, &target).unwrap();
    assert_eq!(found.max_total_tokens, Some((4 * GIB / 32768) as u32));
    assert_eq!(found.max_mamba_cache_size, Some(5 * 16));
    assert_eq!(found.running_limit, Some(16));
}
