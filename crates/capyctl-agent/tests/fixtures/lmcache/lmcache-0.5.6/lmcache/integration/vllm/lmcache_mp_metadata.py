# Excerpt of LMCache v0.5.6 lmcache/integration/vllm/lmcache_mp_metadata.py (Apache-2.0), kept for CapyCTL's LMCache launch-time check tests.
    # Number of scheduled tokens in this request. We keep tracking this to
    # avoid saving tokens whose KV has not been computed yet.
    num_scheduled_tokens: int = 0

    # Number of tokens stored will be initialized when lookup the external
    # hit tokens and will be updated when processing new requests and cached
    # requests.
    num_stored_tokens: int = 0

    # Staging load operation -- save vllm and lmcache hit tokens during lookup
    num_vllm_hit_tokens: int = 0
    num_lmcache_hit_tokens: int = 0

    # Main state
    state: LMCacheMPRequestState = LMCacheMPRequestState.PREFETCHING

    cache_salt: str = ""
    request_configs: dict[str, Any] | None = None
    max_offload_tokens: int | None = None
    lookup_started_at: float | None = None

    mm_adjusted_prompt_ids: list[int] = field(default_factory=list)

    def __init__(self, request: "Request"):
        self.request_id = request.request_id
        self.cache_salt: str = request.cache_salt or ""
        self.request_configs = extract_request_configs_from_request(request)
        self.max_offload_tokens = (self.request_configs or {}).get(
            "lmcache.max_offload_tokens"
        )
        self.lookup_started_at = None

