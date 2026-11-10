# Excerpt of LMCache v0.5.7 lmcache/v1/multiprocess/custom_types.py (Apache-2.0), kept for CapyCTL's LMCache launch-time check tests.
# Source: https://github.com/LMCache/LMCache/blob/v0.5.7/lmcache/v1/multiprocess/custom_types.py

@dataclass(order=True, frozen=True)
class IPCCacheServerKey:
    """Cache key for the IPC (multiprocess) protocol.

    This key type is sent by the client over ZMQ (serialized via msgspec).

    The client sends token_bytes, start, end, and request_id (all required).
    The server computes chunk hashes via TokenHasher and converts to
    ObjectKey for storage operations using ipc_key_to_object_keys().

    ``token_bytes`` is packed by :mod:`lmcache.v1.multiprocess.token_codec`
    (see there for the layout and why no chunk hash changes). It replaced a
    ``token_ids`` tuple in place, so client and server must be upgraded
    together.

    The request_id field is for session tracking and is NOT included
    in equality/hash comparisons (two keys with same content but different
    request_ids are considered equal for cache purposes).
    """

    model_name: str
    world_size: int
    worker_id: int | None

    token_bytes: bytes  # packed big-endian uint32, one word per token
    start: int
    end: int

    # === Session tracking (not part of cache identity) ===
    request_id: str = field(compare=False)

    # === Per-user isolation salt (part of cache identity) ===
    # msgspec encodes dataclasses as maps, so forward wire compatibility
    # works by field name: an old payload without ``cache_salt`` decodes
    # on new code using the default "". Placing the field last is a style
    # choice — all defaulted fields must come after non-defaulted ones.
    #
    # Invariant: must not contain ``@``, ``/``, ``\``, or NUL, and
    # must be <= 128 chars — same rationale as ObjectKey (see
    # ObjectKey.cache_salt). Validated in __post_init__.
    cache_salt: str = ""

    # Request-scoped LMCache configuration passed across the IPC boundary.
    # It is metadata, not part of cache identity.
    request_configs: dict[str, Any] | None = field(default=None, compare=False)

    # Number of workers that retrieve this key's object; the server reserves
    # that many read locks (see ``require_num_kv_readers``). 0 = not sent;
    # lookups reject it.
    num_kv_readers: int = field(default=0, compare=False)

    # Duplicated from ObjectKey — cannot import ObjectKey here due to
    # circular dependency (api.py imports IPCCacheServerKey).
    _SALT_FORBIDDEN_CHARS = frozenset("@/\\\x00")
    _SALT_MAX_LEN = 128

    def __post_init__(self) -> None:
        if len(self.token_bytes) % TOKEN_STRIDE:
            raise ValueError(
                f"token_bytes of {len(self.token_bytes)} byte(s) is not a "
                f"whole number of {TOKEN_STRIDE}-byte token ids"
            )
        bad = self._SALT_FORBIDDEN_CHARS & set(self.cache_salt)
        if bad:
            raise ValueError(
                f"cache_salt must not contain {bad!r} (got {self.cache_salt!r})"
            )
        if len(self.cache_salt) > self._SALT_MAX_LEN:
            raise ValueError(
                f"cache_salt exceeds max length {self._SALT_MAX_LEN} "
                f"(got {len(self.cache_salt)})"
            )

    @classmethod
    def from_token_ids(
        cls,
        model_name: str,
        world_size: int,
        worker_id: int | None,
        token_ids: Sequence[int],
        start: int = 0,
        end: int = 0,
        request_id: str = "",
        cache_salt: str = "",
        num_kv_readers: int = 1,
        request_configs: dict[str, Any] | None = None,
    ) -> "IPCCacheServerKey":
        """Create a key from unpacked token ids.

        The canonical way to build a key from a token list: it owns the
        packing so callers never handle :mod:`token_codec` themselves.

        Args:
            model_name: Model the key belongs to.
            world_size: Number of TP workers.
            worker_id: Worker the key is scoped to, or ``None`` for all.
            token_ids: The token sequence.
            start: Start token index of the key's range.
            end: End token index of the key's range (exclusive).
            request_id: Request the key belongs to.
            cache_salt: Per-user isolation salt.
            num_kv_readers: Number of workers that will read the object.
            request_configs: Optional LMCache request configs.

        Returns:
            The key, with ``token_ids`` packed into ``token_bytes``.
        """
        return cls(
            model_name=model_name,
            world_size=world_size,
            worker_id=worker_id,
            num_kv_readers=num_kv_readers,
            token_bytes=pack_token_ids(token_ids),
            start=start,
            end=end,
            request_id=request_id,
            cache_salt=cache_salt,
            request_configs=request_configs,
        )

    @property
    def num_tokens(self) -> int:
        """How many token ids this key carries."""
        return num_packed_tokens(self.token_bytes)

    def require_num_kv_readers(self) -> int:
        """Declared reader count; rejects keys from pre-field clients.

        Each reader's retrieve releases one read lock, so the count must
        be exact: under-counting unpins an object mid-copy; over-counting
        only holds it to the TTL. 0 means the field was never sent --
        rejected, not guessed.
        """
        if self.num_kv_readers < 1:
            raise ValueError(
                f"num_kv_readers={self.num_kv_readers}: this server "
                "requires clients that send "
                "IPCCacheServerKey.num_kv_readers. Upgrade the LMCache "
                "client."
            )
        return self.num_kv_readers

    def no_worker_id_version(self) -> "IPCCacheServerKey":
        """Create a copy with worker_id=None for lookup requests."""
        return IPCCacheServerKey(
            model_name=self.model_name,
            world_size=self.world_size,
            worker_id=None,
            num_kv_readers=self.num_kv_readers,
            token_bytes=self.token_bytes,
            start=self.start,
            end=self.end,
            request_id=self.request_id,
            cache_salt=self.cache_salt,
            request_configs=self.request_configs,
        )

