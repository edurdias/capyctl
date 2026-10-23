# Excerpt of SGLang v0.5.22 python/sglang/srt/mem_cache/storage/lmcache/lmcache_unified_radix_cache.py (Apache-2.0), kept for CapyCTL's LMCache launch-time check tests.
"""Unified radix tree backed by the LMCache multiprocess service.

LMCache owns external storage; retrieved KV is published after reaching GPU.
"""

from __future__ import annotations

import atexit
import logging
from array import array
from typing import TYPE_CHECKING, Any, Optional

import torch
from lmcache.integration.sglang.lmcache_mp_metadata import (
    LMCacheExternalFlow,
    LMCachePendingStore,
)
from lmcache.integration.sglang.unified_lmcache_mp_connector import (
    UnifiedLMCacheMPConnector,
)

from sglang.srt.mem_cache.allocator.unified_hybrid_swa import (
    UnifiedSWAAllocatorBase,
)
from sglang.srt.mem_cache.base_prefix_cache import (
    CacheRequestHandle,
    EvictParams,
    InitLoadBackParams,
    InsertParams,
    MatchPrefixParams,
    MatchResult,
)
from sglang.srt.mem_cache.radix_cache import RadixKey
from sglang.srt.mem_cache.unified_cache.components import ComponentType
from sglang.srt.mem_cache.unified_radix_cache import NodeId, UnifiedRadixCache

if TYPE_CHECKING:
    from sglang.srt.configs.model_config import ModelConfig
    from sglang.srt.managers.schedule_batch import Req
    from sglang.srt.mem_cache.cache_init_params import CacheInitParams

        flow.load_req = None
        flow.prefix_published = True

    def _submit_store(self, req: Req, token_ids: list[int]) -> None:
        key = RadixKey(
            token_ids,
            req.extra_key,
            is_bigram=self.tree_core.is_eagle,
            cache_salt=req.cache_salt,
        ).page_aligned(self.page_size)
        aligned_len = (
            len(key)

            self.dec_lock_ref(matched.last_device_node, lock_params)
            logger.debug(
                "LMCache store skipped for %s: no Mamba checkpoint at token %d",
                req.rid,
                len(key),
            )
            return
        try:
            operation = self.lmcache_connector.submit_store(
                req.rid,
                key.raw_token_ids()[: len(key)],
                self.lmcache_connector.device_indices_by_group(
                    store_indices,
                    mamba_value=mamba_value,
                ),
                device_indices_start=store_start,
                cache_salt=self.lmcache_connector.build_cache_salt(
                    req.cache_salt, req.extra_key
                ),
            )
        except Exception:

