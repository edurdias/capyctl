# Excerpt of LMCache v0.5.7 lmcache/integration/sglang/unified_lmcache_mp_connector.py (Apache-2.0), kept for CapyCTL's LMCache launch-time check tests.
# Source: https://github.com/LMCache/LMCache/blob/v0.5.7/lmcache/integration/sglang/unified_lmcache_mp_connector.py

class UnifiedLMCacheMPConnector:

    """Asynchronous device-IPC connector to a standalone LMCache server."""

    def __init__(
        self,
        *,
        config_file: Optional[str],
        model_config: Any,
        tp_size: int,
        tp_rank: int,
        tp_group: Optional[dist.ProcessGroup],
        pp_size: int = 1,
        pp_rank: int = 0,
        pp_group: Optional[dist.ProcessGroup] = None,
        page_size: int,
        token_to_kv_pool_allocator: Any,
        req_to_token_pool: Any,
        tree_components: tuple[Any, ...],
        mamba_component: Any,
        sliding_window_size: Optional[int],
    ) -> None:
        try:
            # First Party
            from lmcache.v1.multiprocess.client_config import load_mp_client_config
            from lmcache.v1.multiprocess.transport.factory import (
                RequestClientFactory,
            )
        except ImportError as exc:
            raise ImportError(
                "LMCacheUnifiedRadixCache requires the `lmcache` package and "
                "a running LMCache multiprocess server."
            ) from exc

        # First Party
        from lmcache.integration.sglang.unified_kv_adapter import (
            SGLangUnifiedKVAdapter,
        )

        self._sglang_kv_adapter = SGLangUnifiedKVAdapter(
            token_to_kv_pool_allocator=token_to_kv_pool_allocator,
            req_to_token_pool=req_to_token_pool,
            tree_components=tree_components,
            mamba_component=mamba_component,
            page_size=page_size,
            sliding_window_size=sliding_window_size,
        )
        kv_groups = self._sglang_kv_adapter.resolve_registered_groups()
        mla_enabled = self._sglang_kv_adapter.is_mla_enabled(model_config)

        if not kv_groups or any(not group.kv_tensors for group in kv_groups):
            raise ValueError("LMCache KV group registration cannot be empty")
        kv_tensors = [tensor for group in kv_groups for tensor in group.kv_tensors]
        if any(t.device != kv_tensors[0].device for t in kv_tensors):
            raise ValueError("All LMCache-registered KV tensors must share one device")
        if any(t.dim() < 2 for t in kv_tensors):
            raise NotImplementedError(
                "LMCache MP requires tensors with a leading block/slot axis"
            )
        if any(not tensor.is_contiguous() for tensor in kv_tensors):
            raise NotImplementedError(
                "LMCache MP currently requires contiguous SGLang NHD/MLA tensors"
            )

        resolved_groups: list[SGLangKVComponentGroup] = []
        for group in kv_groups:
            tokens_per_block = group.tokens_per_block or page_size
            slots_per_block = group.slots_per_block or page_size
            if tokens_per_block <= 0 or slots_per_block <= 0:
                raise ValueError(
                    f"LMCache group {group.name!r} has invalid block geometry: "
                    f"{tokens_per_block=}, {slots_per_block=}"
                )
            if tokens_per_block % slots_per_block:
                raise ValueError(
                    f"LMCache group {group.name!r} tokens_per_block "
                    f"{tokens_per_block} must be a multiple of slots_per_block "
                    f"{slots_per_block}"
                )
            tensor_rows_per_block = group.tensor_rows_per_block or (
                slots_per_block,
            ) * len(group.kv_tensors)
            if len(tensor_rows_per_block) != len(group.kv_tensors):
                raise ValueError(
                    f"LMCache group {group.name!r} has "
                    f"{len(group.kv_tensors)} tensors but "
                    f"{len(tensor_rows_per_block)} tensor row geometries"
                )
            for tensor, rows_per_block in zip(
                group.kv_tensors, tensor_rows_per_block, strict=True
            ):
                if rows_per_block <= 0 or tensor.shape[0] % rows_per_block:
                    raise ValueError(
                        f"LMCache group {group.name!r} tensor rows "
                        f"{tensor.shape[0]} are not divisible by "
                        f"tensor_rows_per_block={rows_per_block}"
                    )
            resolved_groups.append(
                SGLangKVComponentGroup(
                    name=group.name,
                    kv_tensors=group.kv_tensors,
                    sliding_window_size=group.sliding_window_size,
                    tokens_per_block=tokens_per_block,
                    slots_per_block=slots_per_block,
                    tensor_rows_per_block=tuple(tensor_rows_per_block),
                    recurrent_state=group.recurrent_state,
                )
            )

        self.mla_enabled = bool(mla_enabled)
        self.mla_only = self._is_mla_only(self.mla_enabled, resolved_groups)

        # Preserve token-native attention as [NB, BS, NH, HS] and token-native
        # MLA as [NB, BS, HS]. Page-native sidecars and recurrent state use an
        # explicit single-plane opaque view [NB, 1, 1, elements].
        wire_groups: list[SGLangKVComponentGroup] = []
        for group in resolved_groups:
            wire_tensors = tuple(
                self._to_wire_block_tensor(
                    tensor,
                    rows_per_block,
                    preserve_head_geometry=(
                        not self.mla_enabled
                        and not group.recurrent_state
                        and tensor.dim() == 3
                    ),
                    preserve_mla_geometry=(
                        self.mla_enabled
                        and not group.recurrent_state
                        and tensor.dim() == 3
                    ),
                )
                for tensor, rows_per_block in zip(
                    group.kv_tensors,
                    group.tensor_rows_per_block,
                    strict=True,
                )
            )
            wire_block_counts = {tensor.shape[0] for tensor in wire_tensors}
            if len(wire_block_counts) != 1:
                raise ValueError(
                    f"LMCache group {group.name!r} tensors expose different "
                    f"block counts: {sorted(wire_block_counts)}"
                )
            wire_groups.append(
                SGLangKVComponentGroup(
                    name=group.name,
                    kv_tensors=wire_tensors,
                    sliding_window_size=group.sliding_window_size,
                    tokens_per_block=group.tokens_per_block,
                    slots_per_block=group.slots_per_block,
                    tensor_rows_per_block=(1,) * len(group.kv_tensors),
                    recurrent_state=group.recurrent_state,
                )
            )
        kv_tensors = [tensor for group in wire_groups for tensor in group.kv_tensors]

        config = load_mp_client_config(config_file_path=config_file)
        if not config.mp_host:
            raise ValueError(
                "LMCache MP config must define mp_host; pass "
                "--lmcache-config-file or LMCACHE_CONFIG_FILE"
            )
        host = str(config.mp_host)
        if "://" not in host:
            host = f"tcp://{host}"
        self.server_url = f"{host.rstrip(':')}:{config.mp_port}"
        self._mq_timeout = float(
            config.get_extra_config_value(
                "lmcache.mp.mq_timeout", _DEFAULT_MQ_TIMEOUT_SECONDS
            )
        )
        self._heartbeat_interval = float(
            config.get_extra_config_value(
                "lmcache.mp.heartbeat_interval",
                _DEFAULT_HEARTBEAT_INTERVAL_SECONDS,
            )
        )
        self._mp_transfer_mode = config.get_extra_config_value(
            "lmcache.mp.mp_transfer_mode", None
        )
        if any(
            group.recurrent_state or group.sliding_window_size >= 0
            for group in wire_groups
        ):
            logger.warning(
                "LMCache sparse SWA/recurrent groups require the LMCache MP "
                "server to be started with --separate-object-groups."
            )

        self.model_name = model_config.model_path
        # Match vLLM's MLA-only parallel strategy. Replicated MLA collapses
        # the LMCache object identity across TP while retaining one distinct
        # piece per PP stage. The general path keeps one piece per TP x PP rank.
        (
            self.tp_size,
            self.tp_rank,
            self.pp_size,
            self.pp_rank,
            self.sglang_world_size,
            self.sglang_worker_id,
        ) = self._resolve_parallel_geometry(tp_size, tp_rank, pp_size, pp_rank)
        self.kv_world_size, self.kv_worker_id = self._resolve_kv_geometry(
            tp_size,
            tp_rank,
            pp_size,
            pp_rank,
            mla_only=self.mla_only,
        )
        self.num_kv_readers = self.tp_size if self.mla_only else 1
        self._is_kv_writer = not self.mla_only or self.tp_rank == 0
        self.tp_group = tp_group
        self.pp_group = pp_group
        if self.tp_size > 1:
            if self.tp_group is None:
                raise ValueError("LMCache TP>1 requires a CPU TP process group")
            group_size = dist.get_world_size(group=self.tp_group)
            if group_size != self.tp_size:
                raise ValueError(
                    "LMCache MP TP synchronization group has the wrong size: "
                    f"got {group_size=}, expected {self.tp_size}"
                )
        if self.pp_size > 1:
            if self.pp_group is None:
                raise ValueError("LMCache PP>1 requires a CPU PP process group")
            group_size = dist.get_world_size(group=self.pp_group)
            if group_size != self.pp_size:
                raise ValueError(
                    "LMCache MP PP synchronization group has the wrong size: "
                    f"got {group_size=}, expected {self.pp_size}"
                )
        self._lookup_leader = self.tp_rank == 0 and self.pp_rank == 0
        self.page_size = int(page_size)
        self.device = kv_tensors[0].device
        self.instance_id = uuid.uuid4().int & ((1 << 63) - 1)
        self._kv_caches = {f"kv_{i}": tensor for i, tensor in enumerate(kv_tensors)}
        self._kv_groups = tuple(wire_groups)
        (
            self._engine_group_info_specs,
            self._kernel_group_to_engine_group,
        ) = self._build_engine_group_info_specs()
        self._req_client = RequestClientFactory.create(self.server_url)
        self._transfer_ctx: Optional["TransferContext"] = None
        self._registered = False
        self._closed = False
        self._lookups: dict[str, LMCacheLookupOperation] = {}
        self._active_sessions: set[str] = set()
        self._store_submitted_tokens: dict[str, int] = {}
        self._control_futures: list[Any] = []
        self._heartbeat_stop = threading.Event()
        self._heartbeat_thread: Optional[threading.Thread] = None
        self._heartbeat_lock = threading.Lock()

        self.chunk_size = self._get_chunk_size()
        if self.chunk_size <= 0 or any(
            self.chunk_size % group.tokens_per_block for group in self._kv_groups
        ):
            raise ValueError(
                f"LMCache chunk size {self.chunk_size} must be a positive "
                "multiple of every SGLang group tokens_per_block"
            )
        # Engine-driven currently accepts one transfer group and needs its
        # physical block count. LMCache-driven derives per-group counts from
        # EngineGroupInfo and ignores this compatibility argument.
        self.blocks_in_chunk = self.chunk_size // self._kv_groups[0].tokens_per_block
        self.register_kv_cache()
        logger.info("UnifiedLMCacheMPConnector initialized succeed.")

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
        request_configs: Optional[dict[str, Any]] = None,
    ) -> Any:
        # First Party
        from lmcache.v1.multiprocess.custom_types import IPCCacheServerKey

        return IPCCacheServerKey.from_token_ids(
            model_name=self.model_name,
            world_size=self.kv_world_size,
            worker_id=worker_id,
            # Sharded KV has one reader per object. TP-replicated MLA maps all
            # TP ranks in a PP stage to one object, so lookup must reserve one
            # read lock for every rank that retrieves that object.
            num_kv_readers=self.num_kv_readers,
            token_ids=operation.token_ids,
            start=start,
            end=end,
            request_id=operation.request_id,
            cache_salt=operation.cache_salt,
            request_configs=request_configs,
        )

