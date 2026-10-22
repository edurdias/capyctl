# Excerpt of LMCache v0.5.6 lmcache/integration/sglang/unified_lmcache_mp_connector.py (Apache-2.0), kept for CapyCTL's LMCache launch-time check tests.
class UnifiedLMCacheMPConnector:
    """Asynchronous device-IPC connector to a standalone LMCache server."""

    def __init__(
        self,
        *,
        config_file: Optional[str],

    @staticmethod
    def build_cache_salt(cache_salt: Optional[str], extra_key: Optional[str]) -> str:
        """Combine SGLang's salt and extra key into one LMCache namespace."""
        if not extra_key:
            return cache_salt or ""
        payload = f"{cache_salt or ''}\0{extra_key}".encode()
        return hashlib.sha256(payload).hexdigest()

    def _create_key(
        self,
        operation: LMCacheLookupOperation,
        *,
        start: int,
        end: int,
        worker_id: Optional[int],
    ) -> Any:
        # First Party
        from lmcache.v1.multiprocess.custom_types import IPCCacheServerKey

        return IPCCacheServerKey(
            model_name=self.model_name,
            world_size=self.kv_world_size,
            worker_id=worker_id,
            # Sharded KV has one reader per object. TP-replicated MLA maps all
            # TP ranks in a PP stage to one object, so lookup must reserve one
            # read lock for every rank that retrieves that object.
            num_kv_readers=self.num_kv_readers,
            token_ids=tuple(operation.token_ids),
            start=start,
            end=end,
            request_id=operation.request_id,
            cache_salt=operation.cache_salt,
        )

