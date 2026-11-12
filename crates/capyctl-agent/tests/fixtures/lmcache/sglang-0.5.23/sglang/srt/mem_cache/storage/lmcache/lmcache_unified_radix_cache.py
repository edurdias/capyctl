# Excerpts fetched 2026-11-11 from https://raw.githubusercontent.com/sgl-project/sglang/v0.5.23/python/sglang/srt/mem_cache/storage/lmcache/lmcache_unified_radix_cache.py
# Apache-2.0; verbatim methods in synthetic class wrappers without upstream bases.
# T45 source-shape checks only, not qualification.
class LMCacheUnifiedRadixCache:
    def reset(self) -> None:
        connector = self.lmcache_connector
        if connector is not None:
            for flow in list(self._external_flows.values()):
                if flow.load is not None:
                    try:
                        flow.load.future.result(timeout=connector.operation_timeout)
                    except Exception:
                        pass
                    connector.complete_load(flow.load, synchronize=False)
                    flow.load_completed = True
                    if flow.prefix_published:
                        # Free only private slots; the tree owns the published suffix.
                        self._release_unused_loaded_slots(flow)
                    else:
                        self.token_to_kv_pool_allocator.free(
                            flow.load.device_indices[flow.released_skip_tokens :]
                        )
                    if flow.mamba_value is not None:
                        self.req_to_token_pool.mamba_allocator.free(flow.mamba_value)
                        flow.mamba_value = None
                    self._release_flow_anchor(flow)
            for pending in list(self._pending_stores):
                try:
                    pending.operation.future.result(timeout=connector.operation_timeout)
                except Exception:
                    pass
                connector.complete_store(pending.operation, synchronize=False)
                self.dec_lock_ref(pending.node_id, pending.lock_params)
            self._external_flows.clear()
            self._pending_stores.clear()
            self._pending_store_counts.clear()
            self._session_finish_requested.clear()
            self.prefetch_loaded_tokens_by_reqid.clear()
            connector.end_all_sessions()
        super().reset()


