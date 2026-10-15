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
            state_slot_bytes: None,
            state_bytes: None,
            member: None,
            disk_tables: None,
            gguf: None,
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
    // A larger request runs CapyCTL's hybrid default (owner decision
    // 2026-10-05), below the router's in-flight bound.
    let found = pool(&fp8(settings(96 * GIB, 16 * GIB, Some(weights))), &target).unwrap();
    assert_eq!(found.max_running_requests, Some(8));
    assert_eq!(found.max_mamba_cache_size, Some(40));
    assert_eq!(found.running_limit, Some(8));
    // A declared count above the default is honored.
    let mut many = fp8(settings(96 * GIB, 16 * GIB, Some(weights)));
    many.common.max_concurrent_requests = Some(MAX_REQUESTS_PER_DEPLOYMENT);
    let found = pool(&many, &target).unwrap();
    assert_eq!(found.max_running_requests, None);
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
// passes nothing for it and still sizes the KV pool. (A request that holds
// the 65 slots the first fixes: owner decision 2026-10-14.)
#[test]
fn arguments_that_size_the_state_pool_win() {
    let target = qwen38_27b();
    for extra in [
        ["--max-mamba-cache-size", "64"],
        ["--mamba-full-memory-ratio", "0.5"],
    ] {
        let mut owned = fp8(settings(64 * GIB, 16 * GIB, Some(21_920_000_000)));
        owned.extra_args = extra.map(String::from).to_vec();
        let found = pool(&owned, &target).unwrap();
        assert_eq!(found.max_mamba_cache_size, None, "{extra:?}");
        assert_eq!(found.max_running_requests, None, "{extra:?}");
        assert_eq!(found.max_total_tokens, Some(524288), "{extra:?}");
        // The installation's host-fixed arguments too.
        let host = fp8(settings(64 * GIB, 16 * GIB, Some(21_920_000_000)));
        let found = sglang_pool(&host, &extra.map(String::from), Ok(&target), None).unwrap();
        assert_eq!(found.max_mamba_cache_size, None, "{extra:?}");
    }
    // `--mamba-ssm-dtype bfloat16` halves the temporal state: 48 ×
    // (61440 + 1572864) bytes per slot, so 4 requests fit where 2 did.
    let plain = fp8(settings(46 * GIB, 16 * GIB, Some(21_920_000_000)));
    assert_eq!(pool(&plain, &target).unwrap().max_running_requests, Some(2));
    let mut halved = plain.clone();
    halved.extra_args = ["--mamba-ssm-dtype", "bfloat16"].map(String::from).to_vec();
    assert_eq!(
        pool(&halved, &target).unwrap().max_running_requests,
        Some(4)
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
    assert_eq!(found.static_bytes, Some(11 * GIB));
    // Never above the request, nor on a discrete device.
    let found = pool(&settings(9 * GIB, 4 * GIB, Some(4 * GIB)), &dense()).unwrap();
    assert_eq!(found.static_bytes, Some(9 * GIB));
    let mut discrete = settings(16 * GIB, 4 * GIB, Some(4 * GIB));
    discrete.memory.device_total_bytes = Some(16 * GIB);
    assert_eq!(pool(&discrete, &dense()).unwrap().static_bytes, None);
}

/// gpt-oss-20b's text model as SGLang 0.5.21 sees it: alternating sliding
/// and full attention, which CapyCTL leaves to SGLang's own sizing.
fn sliding() -> Value {
    json!({
        "num_hidden_layers": 24, "num_attention_heads": 64, "num_key_value_heads": 8,
        "head_dim": 64, "hidden_size": 2880, "max_position_embeddings": 131072,
        "torch_dtype": "bfloat16", "sliding_window": 128,
    })
}

// T14 (catalog runs on GB10, 2026-10-07): SGLang 0.5.21 charges what it
// allocates besides the weights against the static pool whether or not
// CapyCTL sized the pools. A model left to SGLang's own sizing (gpt-oss-20b,
// Gemma 4) had a static pool of exactly weights and KV cache, and a KV cache
// of 2 GiB or less refused to start; it now holds the overhead too.
#[test]
fn a_pool_left_to_sglang_still_holds_the_overhead() {
    let weights = 13_760_000_000;
    let overhead = STATIC_OVERHEAD_BYTES as i64;
    for kv in [GIB, 2 * GIB] {
        let found = pool(&derived(kv, weights), &sliding()).unwrap();
        assert!(found.reason.as_deref().unwrap().contains("SGLang sizes"));
        assert_eq!(found.max_total_tokens, None);
        assert_eq!(found.static_bytes, Some(weights + kv + overhead), "{kv}");
    }
    // An unreadable configuration with known weights: no state to count.
    let unread = sglang_pool(&derived(GIB, weights), &[], Err("no config".into()), None);
    assert_eq!(unread.unwrap().static_bytes, Some(weights + GIB + overhead));
    // Boundaries: exactly weights + KV + overhead fits; one byte less is the
    // whole request; a larger explicit request keeps the request less the
    // margin; unknown weights and a discrete device keep the static pool.
    let exact = weights + GIB + overhead;
    let mut explicit = settings(exact, GIB, Some(weights));
    assert_eq!(
        pool(&explicit, &sliding()).unwrap().static_bytes,
        Some(exact)
    );
    explicit.memory.request_bytes = exact - 1;
    assert_eq!(
        pool(&explicit, &sliding()).unwrap().static_bytes,
        Some(exact - 1)
    );
    let large = settings(32 * GIB, GIB, Some(weights));
    assert_eq!(
        pool(&large, &sliding()).unwrap().static_bytes,
        Some(24 * GIB)
    );
    let unknown = settings(32 * GIB, GIB, None);
    assert_eq!(pool(&unknown, &sliding()).unwrap().static_bytes, None);
    let mut discrete = derived(GIB, weights);
    discrete.memory.device_total_bytes = Some(24 * GIB);
    assert_eq!(pool(&discrete, &sliding()).unwrap().static_bytes, None);
}

// T14 (catalog run of Qwen3.6-35B-A3B on GB10, 2026-10-07): with
// `--max-mamba-cache-size 8` and a KV cache of 1536 MiB or 2 GiB, SGLang
// refused to start: the arguments own the state pool, and the static pool
// held neither that state nor SGLang's own allocations. It now holds the 9
// slots SGLang reserves (8 plus a padding slot) and the overhead.
#[test]
fn arguments_that_fix_the_state_pool_are_held_by_the_static_pool() {
    let target = qwen38_27b();
    let weights = 21_920_000_000;
    let overhead = STATIC_OVERHEAD_BYTES as i64;
    let slots = |extra: &[&str]| {
        let mut owned = derived(1536 << 20, weights);
        owned.extra_args = extra.iter().map(|arg| (*arg).to_owned()).collect();
        pool(&owned, &target).unwrap()
    };
    let found = slots(&["--max-mamba-cache-size", "8"]);
    assert_eq!(found.max_mamba_cache_size, None);
    assert_eq!(
        found.static_bytes,
        Some(weights + (1536 << 20) + 9 * STATE as i64 + overhead)
    );
    // The `=` spelling counts alike.
    assert_eq!(
        slots(&["--max-mamba-cache-size=8"]).static_bytes,
        found.static_bytes
    );
    // A ratio leaves the state to SGLang: the overhead only.
    assert_eq!(
        slots(&["--mamba-full-memory-ratio", "0.5"]).static_bytes,
        Some(weights + (1536 << 20) + overhead)
    );
    // With speculative decoding SGLang keeps one intermediate state per draft
    // token for each running request plus one: the declared count, at most
    // the slots.
    let spec = [
        "--max-mamba-cache-size",
        "8",
        "--speculative-algorithm",
        "DFLASH",
        "--speculative-num-draft-tokens",
        "4",
    ];
    let mut declared = derived(1536 << 20, weights);
    declared.extra_args = spec.map(String::from).to_vec();
    declared.common.max_concurrent_requests = Some(2);
    assert_eq!(
        pool(&declared, &target).unwrap().static_bytes,
        Some(weights + (1536 << 20) + (9 + 3 * 4) * STATE as i64 + overhead)
    );
    // Undeclared, the slots bound it (a derived request with a margin large
    // enough to hold it).
    declared.common.max_concurrent_requests = None;
    declared.memory.request_bytes += 8 * GIB;
    declared.memory.margin_bytes += 8 * GIB;
    assert_eq!(
        pool(&declared, &target).unwrap().static_bytes,
        Some(weights + (1536 << 20) + (9 + 9 * 4) * STATE as i64 + overhead)
    );
    // Owner decision 2026-10-14: a declared request that cannot hold that
    // static pool with its margin outside is refused, naming the smallest
    // whole GiB that does; that request keeps the request less the margin.
    let mut explicit = fp8(settings(24 * GIB, 1536 << 20, Some(weights)));
    explicit.extra_args = ["--max-mamba-cache-size", "8"].map(String::from).to_vec();
    let refusal = pool(&explicit, &target).unwrap_err();
    let want = weights + (1536 << 20) + 9 * STATE as i64 + overhead;
    assert!(
        refusal.contains(&format!("{want} bytes in all")),
        "{refusal}"
    );
    let gib = named_request_gib(&refusal);
    assert_eq!(gib, (want + 8 * GIB + GIB - 1) / GIB);
    explicit.memory.request_bytes = gib * GIB;
    assert_eq!(
        pool(&explicit, &target).unwrap().static_bytes,
        Some((gib - 8) * GIB)
    );
    explicit.memory.request_bytes = (gib - 1) * GIB;
    assert!(pool(&explicit, &target).is_err());
}

/// The memory request a refusal names, in MiB.
fn named_request_mib(refusal: &str) -> i64 {
    let tail = refusal
        .split("raise engine_config.memory.request to \"")
        .nth(1)
        .unwrap_or_else(|| panic!("no named request: {refusal}"));
    tail.split("MiB\"").next().unwrap().parse().unwrap()
}

/// The memory request a refusal names, in whole GiB.
fn named_request_gib(refusal: &str) -> i64 {
    let tail = refusal
        .split("raise engine_config.memory.request to \"")
        .nth(1)
        .unwrap_or_else(|| panic!("no named request: {refusal}"));
    tail.split("GiB\"").next().unwrap().parse().unwrap()
}

// Found live 2026-10-15 (Qwen3.8-27B with an EAGLE MTP draft and
// `--max-mamba-cache-size 60` at a 65 GiB request on host A): status bounds
// a reason to 512 bytes, and the refusal, which named the request to raise to
// last, reached the operator cut at "raise engin". It now leads with that
// request, 75 GiB (71163366912 + 8589934592 = 79753301504 bytes, rounded up
// to whole GiB), and keeps every figure after it.
#[test]
fn the_live_eagle_refusal_leads_with_the_request_to_raise() {
    let mut live = settings(69_793_218_560, 31_138_512_896, Some(21_921_697_280));
    let args = [
        "--max-mamba-cache-size",
        "60",
        "--speculative-algorithm",
        "EAGLE",
    ]
    .map(String::from);
    live.extra_args = args.to_vec();
    let refusal = fixed_state_fits(&live, &args, 9_796_337_664, 5_085_593_600).unwrap_err();
    assert_eq!(
        refusal,
        "raise engine_config.memory.request to \"75GiB\" (80530636800 bytes), keeping \
         memory.kv_cache at 31138512896 bytes: SGLang's static pool holds 21921697280 bytes of \
         weights, the MTP draft's 5085593600-byte embedding and LM head, the 31138512896-byte \
         KV cache, 9796337664 bytes of recurrent state for the 60 slots \
         `--max-mamba-cache-size` fixes (with its speculative decoding scratch), and \
         3221225472 bytes of SGLang's own allocations, 71163366912 bytes in all; with the \
         8589934592-byte margin outside it that is more than the 69793218560-byte memory \
         request"
    );
    assert_eq!(named_request_gib(&refusal), 75);
    assert_eq!(counted_in(&refusal), 71_163_366_912);
    // The request it names is the smallest whole GiB that holds them.
    live.memory.request_bytes = 75 * GIB;
    assert!(fixed_state_fits(&live, &args, 9_796_337_664, 5_085_593_600).is_ok());
    live.memory.request_bytes = 74 * GIB;
    assert!(fixed_state_fits(&live, &args, 9_796_337_664, 5_085_593_600).is_err());
}

/// The static pool a refusal of a declared request says it needs, in bytes.
fn counted_in(refusal: &str) -> i64 {
    let head = refusal
        .split(" bytes in all")
        .next()
        .unwrap_or_else(|| panic!("no static pool: {refusal}"));
    head.rsplit(' ').next().unwrap().parse().unwrap()
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
        // Found live 2026-10-15: the request to raise to leads the refusal.
        assert!(
            refusal.starts_with(&format!(
                "raise engine_config.memory.request to \"{}MiB\"",
                named_request_mib(&refusal)
            )),
            "{refusal}"
        );
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
    // Half the margin holds 16 requests; the hybrid default runs 8.
    assert_eq!(found.max_mamba_cache_size, Some(5 * 8));
    assert_eq!(found.max_running_requests, Some(8));
    assert_eq!(found.running_limit, Some(8));
}

// ADR 0014, note on amendment A14 (found live 2026-10-04): on a discrete GPU
// whose pools CapyCTL fixes, the rendered fraction carries an allowance for
// what SGLang's baseline does not count. Unified memory, a pool left to
// SGLang, and a state pool sized by a ratio get none.
#[test]
fn a_discrete_launch_with_fixed_pools_carries_the_baseline_allowance() {
    let target = frognano_4b();
    let allowance = Some(DISCRETE_BASELINE_ALLOWANCE_BYTES);
    let discrete = derived_discrete(LAPTOP_KV, FROGNANO_WEIGHTS);
    assert_eq!(
        pool(&discrete, &target).unwrap().static_allowance,
        allowance
    );
    let mut plain = settings(12 * GIB, 2 * GIB, Some(8 * GIB));
    plain.memory.device_total_bytes = Some(16 * GIB);
    assert_eq!(pool(&plain, &dense()).unwrap().static_allowance, allowance);
    let unified = derived(4 * GIB, FROGNANO_WEIGHTS);
    assert_eq!(pool(&unified, &target).unwrap().static_allowance, None);
    let sliding = json!({
        "num_hidden_layers": 4, "num_attention_heads": 8, "hidden_size": 1024,
        "max_position_embeddings": 4096, "torch_dtype": "bfloat16", "sliding_window": 1024,
    });
    let mut declared = plain.clone();
    declared.max_total_tokens = Some(4096);
    assert_eq!(pool(&declared, &sliding).unwrap().static_allowance, None);
    let mut ratio = discrete.clone();
    ratio.extra_args = vec!["--mamba-full-memory-ratio".into(), "0.5".into()];
    assert_eq!(pool(&ratio, &target).unwrap().static_allowance, None);
    let mut slots = discrete;
    slots.extra_args = vec!["--max-mamba-cache-size".into(), "20".into()];
    assert_eq!(pool(&slots, &target).unwrap().static_allowance, allowance);
}

// ADR 0014 amendment A16: the host measures one state slot from the
// checkpoint's configuration, as the launch sizes it with its arguments.
#[test]
fn the_state_slot_is_read_from_the_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.json"), qwen38_27b().to_string()).unwrap();
    assert_eq!(sglang_state_slot_bytes(dir.path(), &[]), Some(STATE as i64));
    // A bfloat16 temporal state: 48 x (61440 + 1572864).
    let args = ["--mamba-ssm-dtype".to_owned(), "bfloat16".to_owned()];
    assert_eq!(
        sglang_state_slot_bytes(dir.path(), &args),
        Some(48 * (61_440 + 1_572_864))
    );
    std::fs::write(dir.path().join("config.json"), dense().to_string()).unwrap();
    assert_eq!(sglang_state_slot_bytes(dir.path(), &[]), None);
    assert_eq!(sglang_state_slot_bytes(&dir.path().join("none"), &[]), None);
}

// ADR 0014 amendment A16: what a derived request reserves for the state.
#[test]
fn a_derived_request_reserves_the_state_that_fits() {
    let all = |_: u64| true;
    // Undeclared, the hybrid default (owner decision 2026-10-05): 8 requests.
    assert_eq!(
        derived_state_reserve(STATE, &[], None, all),
        Some(41 * STATE)
    );
    assert_eq!(
        derived_state_reserve(STATE, &[], Some(32), all),
        Some(161 * STATE)
    );
    assert_eq!(
        derived_state_reserve(STATE, &[], Some(8), all),
        Some(41 * STATE)
    );
    let under = |bytes: u64| bytes <= 31 * STATE;
    assert_eq!(
        derived_state_reserve(STATE, &[], None, under),
        Some(31 * STATE)
    );
    assert_eq!(derived_state_reserve(STATE, &[], None, |_| false), None);
    // Draft-token states are reserved with the slots.
    let args: Vec<String> = [
        "--speculative-algorithm",
        "DFLASH",
        "--speculative-num-draft-tokens",
        "8",
    ]
    .map(str::to_owned)
    .into();
    assert_eq!(
        derived_state_reserve(STATE, &args, Some(2), all),
        Some((11 + 3 * 8) * STATE)
    );
    // Under ReplaySSM spec-verify the slot alone cannot size the ring: the
    // intermediate states, which exceed it, stay reserved.
    let mut replay = args.clone();
    replay.push("--enable-linear-replayssm-spec".to_owned());
    assert_eq!(
        derived_state_reserve(STATE, &replay, Some(2), all),
        Some((11 + 3 * 8) * STATE)
    );
    // Without a draft-token count, or when the arguments size the state
    // pool, the launch sizes it as before.
    assert_eq!(derived_state_reserve(STATE, &args[..2], Some(2), all), None);
    let args = ["--max-mamba-cache-size".to_owned(), "64".to_owned()];
    assert_eq!(derived_state_reserve(STATE, &args, None, all), None);
}

/// The Qwen3.8-27B NVFP4 deployment the ReplaySSM report came from: EAGLE
/// with 4 draft tokens, 22 running requests, `--max-mamba-cache-size 110`,
/// a 79 GiB request and a 29 GiB KV cache; `extra` is appended to its
/// arguments.
fn eagle_27b(extra: &[&str]) -> SglangLaunchSettings {
    let mut deployment = settings(79 * GIB, 29 * GIB, Some(21_920_000_000));
    deployment.common.max_concurrent_requests = Some(22);
    deployment.extra_args = [
        "--speculative-algorithm",
        "EAGLE",
        "--speculative-num-steps",
        "3",
        "--speculative-eagle-topk",
        "1",
        "--speculative-num-draft-tokens",
        "4",
        "--max-mamba-cache-size",
        "110",
    ]
    .iter()
    .chain(extra)
    .map(|arg| (*arg).to_owned())
    .collect();
    deployment
}

/// SGLang 0.5.20 and 0.5.21's ReplaySSM spec-verify scratch for one
/// request of Qwen3.8-27B with 4 draft tokens: per linear-attention layer,
/// the 16-record ring (`replayssm_ring_bytes_per_req`: bfloat16 compact d
/// and k with their low parts, a float32 gate) and the deduplicated conv
/// window (10240 channels × (4 + 4 - 2) bfloat16 columns).
const REPLAY: u64 = 48 * (16 * (48 * 128 * 4 + 16 * 128 * 4 + 48 * 4) + 10240 * 6 * 2);

// T14 (reported 2026-10-14, SGLang 0.5.21 `kv_cache_configurator.py`
// `_handle_max_mamba_cache` and `memory_pool.py` `MambaPool.__init__`):
// `--enable-linear-replayssm-spec` drops the per-draft-token intermediate
// states; SGLang keeps the ReplaySSM ring and the conv windows for each
// running request plus one instead. CapyCTL counted 92 intermediate states
// (13.2 GiB) and capped the static pool at the whole request.
#[test]
fn replayssm_spec_verify_keeps_a_ring_instead_of_intermediate_states() {
    let target = qwen38_27b();
    let weights = 21_920_000_000;
    let overhead = STATIC_OVERHEAD_BYTES as i64;
    // Without the flag: 111 slots and (22 + 1) × 4 intermediate states, which
    // with the margin overflow the request (owner decision 2026-10-14:
    // refused, naming the request that holds them).
    let intermediates = weights + 29 * GIB + (111 + 23 * 4) * STATE as i64 + overhead;
    let refusal = pool(&eagle_27b(&[]), &target).unwrap_err();
    assert_eq!(counted_in(&refusal), intermediates);
    assert_eq!(
        named_request_gib(&refusal),
        (intermediates + 8 * GIB + GIB - 1) / GIB
    );
    // With it, the slots and 23 ReplaySSM scratch sets fit well inside: the
    // static pool is the request less its 8 GiB margin.
    let replay = pool(&eagle_27b(&["--enable-linear-replayssm-spec"]), &target).unwrap();
    assert_eq!(replay.static_bytes, Some(71 * GIB));
    assert_eq!(replay.max_total_tokens, Some(475136));
    // At a 72 GiB request the refusal shows what is counted.
    let at_72 = |profile: &[String], extra: &[&str]| {
        let mut deployment = eagle_27b(extra);
        deployment.memory.request_bytes = 72 * GIB;
        counted_in(&sglang_pool(&deployment, profile, Ok(&target), None).unwrap_err())
    };
    assert_eq!(REPLAY, 31_211_520);
    let counted = weights + 29 * GIB + 111 * STATE as i64 + 23 * REPLAY as i64 + overhead;
    assert_eq!(at_72(&[], &["--enable-linear-replayssm-spec"]), counted);
    // SGLang's parser takes an unambiguous abbreviation too, and the flag
    // counts in the host-fixed arguments as in the deployment's.
    assert_eq!(at_72(&[], &["--enable-linear-replayssm-sp"]), counted);
    let host = ["--enable-linear-replayssm-spec".to_owned()];
    assert_eq!(at_72(&host, &[]), counted);
    // A longer ring (`--linear-replayssm-cache-len`) grows with its records.
    let ring_16 = 48 * 16 * (48 * 128 * 4 + 16 * 128 * 4 + 48 * 4);
    assert_eq!(
        at_72(
            &[],
            &[
                "--enable-linear-replayssm-spec",
                "--linear-replayssm-cache-len",
                "32",
            ]
        ),
        counted + 23 * ring_16
    );
    // The decode ring (`--enable-linear-replayssm`, another option) keeps
    // the intermediate states, and SGLang's boolean flag takes no value: a
    // `=true` spelling is refused by its parser, so it counts nothing.
    for extra in [
        "--enable-linear-replayssm",
        "--enable-linear-replayssm-spec=true",
    ] {
        assert_eq!(at_72(&[], &[extra]), intermediates, "{extra}");
    }
}

// T14: when CapyCTL sizes the state pool itself, the flag drops the
// intermediate states there too. The 79 GiB request cannot hold 22 running
// requests with 92 intermediate states; with the ring it does.
#[test]
fn replayssm_spec_verify_lets_the_sized_state_pool_hold_its_requests() {
    let target = qwen38_27b();
    let weights = 21_920_000_000;
    let without_slots = |extra: &[&str]| {
        let mut deployment = eagle_27b(extra);
        deployment.extra_args.truncate(8);
        deployment
            .extra_args
            .extend(extra.iter().map(|arg| (*arg).to_owned()));
        pool(&deployment, &target)
    };
    let refusal = without_slots(&[]).unwrap_err();
    assert!(
        refusal.contains(&format!("{} bytes", (111 + 23 * 4) * STATE)),
        "{refusal}"
    );
    let found = without_slots(&["--enable-linear-replayssm-spec"]).unwrap();
    assert_eq!(found.max_mamba_cache_size, Some(110));
    assert_eq!(found.max_running_requests, None);
    assert_eq!(found.running_limit, None);
    assert_eq!(found.static_bytes, Some(71 * GIB));
    // At a 75 GiB request (a 67 GiB floor) they still fit, and the static
    // pool shows what is counted.
    let mut tighter = eagle_27b(&["--enable-linear-replayssm-spec"]);
    tighter.extra_args.remove(8);
    tighter.extra_args.remove(8);
    tighter.memory.request_bytes = 75 * GIB;
    assert_eq!(
        pool(&tighter, &target).unwrap().static_bytes,
        Some(
            weights
                + 29 * GIB
                + 111 * STATE as i64
                + 23 * REPLAY as i64
                + STATIC_OVERHEAD_BYTES as i64
        )
    );
}

// T14: SGLang declares a float32 temporal state under the flag unless
// `--mamba-ssm-dtype` names one (`handle_linear_attn_backend`), whatever
// the checkpoint says.
#[test]
fn replayssm_spec_verify_keeps_a_float32_temporal_state() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = qwen38_27b();
    config["text_config"]["mamba_ssm_dtype"] = json!("bfloat16");
    std::fs::write(dir.path().join("config.json"), config.to_string()).unwrap();
    let halved = 48 * (61_440 + 1_572_864);
    assert_eq!(sglang_state_slot_bytes(dir.path(), &[]), Some(halved));
    let flag = "--enable-linear-replayssm-spec".to_owned();
    assert_eq!(
        sglang_state_slot_bytes(dir.path(), std::slice::from_ref(&flag)),
        Some(STATE as i64)
    );
    let named = [flag, "--mamba-ssm-dtype".to_owned(), "bfloat16".to_owned()];
    assert_eq!(sglang_state_slot_bytes(dir.path(), &named), Some(halved));
}

/// RadixArk/Qwen3.8-27B-NVFP4-BF16-LMHead at
/// 009632fef96dd349150baa780c984e62e70e91fe as CapyCTL reads it: the language
/// model of [`qwen38_27b`] with its MTP head (`mtp_num_hidden_layers: 1`), a
/// 248320-token vocabulary and an untied BF16 embedding and LM head (2542796800
/// bytes each in its safetensors headers).
fn qwen38_27b_mtp() -> Value {
    let mut config = qwen38_27b();
    config["architectures"] = json!(["Qwen3_5ForConditionalGeneration"]);
    config["tie_word_embeddings"] = json!(false);
    let text = &mut config["text_config"];
    text["mtp_num_hidden_layers"] = json!(1);
    text["vocab_size"] = json!(248320);
    text["tie_word_embeddings"] = json!(false);
    config
}

/// One BF16 table of that checkpoint: 248320 × 5120 × 2 bytes.
const TABLE: i64 = 2_542_796_800;

/// The deployment of the live run on host A (2026-10-14, SGLang 0.5.21): the
/// checkpoint's MTP head drafting with EAGLE, `kv_cache_dtype: fp8_e4m3`, the
/// measured weights, and `extra` appended to its arguments.
fn mtp_eagle(extra: &[&str]) -> SglangLaunchSettings {
    let mut deployment = fp8(settings(79 * GIB, 29 * GIB, Some(23_749_332_688)));
    deployment.common.max_concurrent_requests = Some(22);
    deployment.extra_args = [
        "--speculative-algorithm",
        "EAGLE",
        "--speculative-num-steps",
        "3",
        "--speculative-eagle-topk",
        "1",
        "--speculative-num-draft-tokens",
        "4",
        "--enable-linear-replayssm-spec",
        "--max-mamba-cache-size",
        "110",
        "--enable-mixed-chunk",
    ]
    .iter()
    .chain(extra)
    .map(|arg| (*arg).to_owned())
    .collect();
    deployment
}

/// The KV tokens SGLang 0.5.21's profile (`_profile_available_bytes`) gives
/// the target of [`mtp_eagle`] from a static pool of `static_bytes`: what the
/// weights, the draft's tables, the state with its ReplaySSM scratch and
/// `overhead` leave, at 16 fp8 attention layers of 4 × 256 per token.
fn profiled_tokens(static_bytes: i64, overhead: i64) -> i64 {
    let state = 111 * STATE as i64 + 23 * REPLAY as i64;
    (static_bytes - 23_749_332_688 - 2 * TABLE - state - overhead) / 32768
}

// T14 (found live 2026-10-14 on host A, SGLang 0.5.21, branch
// fix/sglang-replayssm-state at d4b86843): EAGLE from the checkpoint's MTP
// head. CapyCTL passed 950272 KV tokens (29 GiB at 32 KiB) and a 71 GiB
// static pool; SGLang held 770984 and 776670, and the process took
// 78.7-81.8 GiB at Ready. The draft worker loads its own embedding and LM
// head before the target's profile, which charges them against the static
// pool, and then keeps a KV pool of one layer for the same tokens outside it.
#[test]
fn an_mtp_draft_keeps_its_kv_layer_and_tables_in_the_static_pool() {
    let target = qwen38_27b_mtp();
    let weights = 23_749_332_688;
    let overhead = STATIC_OVERHEAD_BYTES as i64;
    let state = 111 * STATE as i64 + 23 * REPLAY as i64;
    // What d4b86843 rendered, and what SGLang's profile left the target of
    // it: 804866 tokens with the 3 GiB overhead. The live 770984 and 776670
    // are 0.86-1.03 GiB below: the draft runner's own allocations, which no
    // figure here models.
    let before = 71 * GIB;
    assert_eq!(profiled_tokens(before, overhead), 804866);
    assert!((770984..=776670).all(|live| live < profiled_tokens(before, overhead)));
    assert!(profiled_tokens(before, overhead + (21 << 26)) < 770984);
    // Now the draft's 2048-byte fp8 layer shares the KV cache, so the pool is
    // 29 GiB / (32768 + 2048) bytes, and the static pool holds the tables:
    // 75.44 GiB, which with the 8 GiB margin the 79 GiB request cannot hold
    // (owner decision 2026-10-14). The refusal names 84 GiB.
    let rendered = weights + 29 * GIB + state + overhead + 2 * TABLE;
    let refusal = pool(&mtp_eagle(&[]), &target).unwrap_err();
    assert_eq!(counted_in(&refusal), rendered);
    assert_eq!(named_request_gib(&refusal), 84);
    assert!(
        refusal.contains(&format!(
            "the MTP draft's {}-byte embedding and LM head",
            2 * TABLE
        )),
        "{refusal}"
    );
    // Without `--enable-linear-replayssm-spec` its 92 intermediate states
    // need 96 GiB.
    let mut intermediates = mtp_eagle(&[]);
    intermediates.extra_args.remove(8);
    let refusal = pool(&intermediates, &target).unwrap_err();
    assert_eq!(named_request_gib(&refusal), 96);
    let at_84 = |extra: &[&str]| {
        let mut deployment = mtp_eagle(extra);
        deployment.memory.request_bytes = 84 * GIB;
        pool(&deployment, &target).unwrap()
    };
    let found = at_84(&[]);
    assert_eq!(found.max_total_tokens, Some(894373));
    assert_eq!(found.static_bytes, Some(76 * GIB));
    let mut short = mtp_eagle(&[]);
    short.memory.request_bytes = 83 * GIB;
    assert!(pool(&short, &target).is_err());
    // SGLang's profile reaches the tokens passed from the static pool counted
    // even with that extra GiB, and the draft's layer (allocated after it)
    // fits what is left.
    assert!(profiled_tokens(rendered, overhead + GIB) >= 894373);
    let target_kv = 894373 * 32768;
    let draft_kv = 894373 * 2048;
    assert!(target_kv + draft_kv <= 29 * GIB);
    // `NEXTN` (SGLang's alias), `EAGLE3`, and a draft KV dtype of its own.
    for algorithm in ["NEXTN", "eagle3"] {
        let mut named = mtp_eagle(&[]);
        named.extra_args[1] = algorithm.to_owned();
        named.memory.request_bytes = 84 * GIB;
        assert_eq!(pool(&named, &target).unwrap(), found, "{algorithm}");
    }
    let bf16_draft = at_84(&["--speculative-draft-kv-cache-dtype", "bfloat16"]);
    assert_eq!(bf16_draft.max_total_tokens, Some(844686));
    // When CapyCTL sizes the state itself it counts them alike: the 79 GiB
    // request is refused, naming them, and 82 GiB holds them.
    let mut sized = mtp_eagle(&[]);
    sized.extra_args.drain(9..11);
    let refusal = pool(&sized, &target).unwrap_err();
    assert!(
        refusal.contains(&format!("{} bytes of weights", weights + 2 * TABLE))
            && refusal.contains("embedding and LM head included"),
        "{refusal}"
    );
    sized.memory.request_bytes = 82 * GIB;
    let sized = pool(&sized, &target).unwrap();
    assert_eq!(sized.max_total_tokens, Some(894373));
    assert_eq!(sized.max_mamba_cache_size, Some(110));
    assert_eq!(sized.static_bytes, Some(rendered));
}

// T14: only a draft from the checkpoint's own head counts its tables and KV
// layer; every other launch of that checkpoint sizes as before.
#[test]
fn other_launches_of_an_mtp_checkpoint_size_as_before() {
    let target = qwen38_27b_mtp();
    let weights = 23_749_332_688;
    let overhead = STATIC_OVERHEAD_BYTES as i64;
    // No speculative decoding: the 950272 tokens, and the static pool is the
    // request less its margin, which holds the state.
    let mut plain = fp8(settings(79 * GIB, 29 * GIB, Some(weights)));
    plain.common.max_concurrent_requests = Some(22);
    plain.extra_args = ["--max-mamba-cache-size", "110"].map(String::from).to_vec();
    let found = pool(&plain, &target).unwrap();
    assert_eq!(found.max_total_tokens, Some(950272));
    assert!(weights + 29 * GIB + 111 * STATE as i64 + overhead < 71 * GIB);
    assert_eq!(found.static_bytes, Some(71 * GIB));
    assert_eq!(found, pool(&plain, &qwen38_27b()).unwrap());
    // A separate draft model, named in the arguments or declared, is sized
    // from its own configuration.
    let named = pool(
        &mtp_eagle(&["--speculative-draft-model-path", "/drafters/eagle"]),
        &target,
    )
    .unwrap();
    assert_eq!(named.max_total_tokens, Some(950272));
    let declared = sglang_pool(
        &mtp_eagle(&[]),
        &[],
        Ok(&target),
        Some(Ok(&dflash_drafter())),
    )
    .unwrap();
    assert_eq!(declared.max_total_tokens, Some(724016));
    // A checkpoint without an MTP head has no draft of its own to count.
    let headless = pool(&mtp_eagle(&[]), &qwen38_27b()).unwrap();
    assert_eq!(headless.max_total_tokens, Some(950272));
    // Tied embeddings are one table.
    let mut tied = target.clone();
    tied["text_config"]["tie_word_embeddings"] = json!(true);
    tied["tie_word_embeddings"] = json!(true);
    let counted = |config: &Value| counted_in(&pool(&mtp_eagle(&[]), config).unwrap_err());
    assert_eq!(counted(&target) - counted(&tied), TABLE);
}
