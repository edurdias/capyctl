# Excerpt of LMCache v0.5.7 lmcache/integration/vllm/lmcache_mp_metadata.py (Apache-2.0), kept for CapyCTL's LMCache launch-time check tests.
# Source: https://github.com/LMCache/LMCache/blob/v0.5.7/lmcache/integration/vllm/lmcache_mp_metadata.py

class LMCacheMPRequestTracker:

    def __init__(self, request: "Request"):
        self.request_id = request.request_id
        self.cache_salt: str = request.cache_salt or ""
        self.request_configs = extract_request_configs_from_request(request)
        self.max_offload_tokens = (self.request_configs or {}).get(
            "lmcache.max_offload_tokens"
        )
        self.lookup_started_at = None
        self.all_token_ids = request.all_token_ids
        self.num_prompt_tokens = request.num_prompt_tokens
        self.allocated_block_ids = {}
        self.num_engine_computed_tokens = 0
        self.num_stored_tokens = 0
        self.num_vllm_hit_tokens = 0
        self.num_lmcache_hit_tokens = 0
        self.apc_covered_tokens = 0
        self.need_full_lookup = False
        self.pending_free_ranges = []
        self.state = LMCacheMPRequestState.PREFETCHING
        self.packed_tokens = bytearray()
        self.mm_adjusted_prompt_ids = []
        mm_hashes, mm_positions = extract_mm_features(request)
        if mm_hashes and mm_positions:
            prompt_ids = torch.tensor(request.prompt_token_ids)
            apply_mm_hashes_to_token_ids(prompt_ids, mm_hashes, mm_positions)
            self.mm_adjusted_prompt_ids = prompt_ids.tolist()

