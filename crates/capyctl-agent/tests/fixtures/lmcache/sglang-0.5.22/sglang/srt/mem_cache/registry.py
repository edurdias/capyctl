# Excerpt of SGLang v0.5.22 python/sglang/srt/mem_cache/registry.py (Apache-2.0), kept for CapyCTL's LMCache launch-time check tests.
def default_radix_cache_factory(ctx: TreeCacheBuildContext) -> BasePrefixCache:
    """Built-in Radix Cache selection chain."""
    params = ctx.params

    is_pure_swa = ctx.is_hybrid_swa and ctx.full_tokens_per_layer == 0
    # A disabled cache publishes nothing, so no storage backend applies.
    # Host-pool retraction goes to UnifiedRadixCache, which validates it (and
    # rejects pure-SWA).
    if ctx.disable_radix_cache:
        if (
            is_pure_swa
            and get_disagg().disaggregation_decode_retraction_backup != "host_pool"
        ):
            from sglang.srt.mem_cache.pure_swa_radix_cache import PureSWARadixCache

            return PureSWARadixCache(params=params)
        return create_unified_radix_cache(ctx)

    if get_memory().enable_lmcache:
        from sglang.srt.mem_cache.storage.lmcache.lmcache_unified_radix_cache import (
            LMCacheUnifiedRadixCache,
        )
        from sglang.srt.mem_cache.unified_cache.components import ComponentType

        tree_components = []
        if not is_pure_swa:
            tree_components.append(ComponentType.FULL)
        if ctx.is_hybrid_swa:
            tree_components.append(ComponentType.SWA)
        if ctx.is_hybrid_ssm:
            tree_components.append(ComponentType.MAMBA)
        params.tree_components = tuple(tree_components)
        return LMCacheUnifiedRadixCache(
            params,
            model_config=ctx.model_config,
            tp_size=ctx.tp_size,
            tp_rank=ctx.tp_rank,
            lmcache_config_file=get_memory().lmcache_config_file,
            forward_stream=ctx.tp_worker.model_runner.forward_stream,
        )

    if get_memory().enable_unified_cache_external_linker:
        return create_unified_radix_cache(ctx)

    if is_pure_swa:
        from sglang.srt.mem_cache.pure_swa_radix_cache import PureSWARadixCache

        return PureSWARadixCache(params=params)

    if get_memory().enable_flexkv:
        # Importing the package side-effect registers the explicit
        # ``--radix-cache-backend=flexkv`` factory; we then call the
        # factory directly so --enable-flexkv stands on its own.
        import os

        from sglang.srt.mem_cache.storage.flexkv import _flexkv_factory

        # Honor a CLI --flexkv-config-file by forwarding it via the env
        # var that FlexKV's config loader actually reads.
        if get_memory().flexkv_config_file and not os.environ.get("FLEXKV_CONFIG_PATH"):
            os.environ["FLEXKV_CONFIG_PATH"] = get_memory().flexkv_config_file
        return _flexkv_factory(ctx)

    return create_unified_radix_cache(ctx)

