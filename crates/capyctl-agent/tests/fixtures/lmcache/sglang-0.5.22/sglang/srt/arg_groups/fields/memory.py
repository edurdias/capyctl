# Excerpt of SGLang v0.5.22 python/sglang/srt/arg_groups/fields/memory.py (Apache-2.0), kept for CapyCTL's LMCache launch-time check tests.
class Memory(msgspec.Struct):
    """Namespace ``memory``."""
    ...

    # -------------------------------------------------------------------------
    # LMCache
    # -------------------------------------------------------------------------
    enable_lmcache: A[
        bool, "Using LMCache as an alternative hierarchical cache solution"
    ] = False
    lmcache_config_file: A[
        Optional[str],
        "Path to the LMCache YAML configuration file",
    ] = None

    # -------------------------------------------------------------------------
    # FlexKV
