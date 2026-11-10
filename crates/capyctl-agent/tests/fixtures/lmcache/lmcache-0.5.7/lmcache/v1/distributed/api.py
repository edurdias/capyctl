# Excerpt of LMCache v0.5.7 lmcache/v1/distributed/api.py (Apache-2.0), kept for CapyCTL's LMCache launch-time check tests.
# Source: https://github.com/LMCache/LMCache/blob/v0.5.7/lmcache/v1/distributed/api.py

@dataclass(frozen=True)
class ObjectKeyIdentity:
    """Matching fields shared by local objects and internal wire messages."""

    chunk_hash: bytes
    kv_rank: int
    model_name: str
    object_group_id: int = 0
    cache_salt: str = ""

    def __post_init__(self) -> None:
        if "@" in self.model_name:
            raise ValueError("model_name must not contain '@'")
        if self.object_group_id < 0:
            raise ValueError("object_group_id must be >= 0")
        if any(char in self.cache_salt for char in "@/\\\x00"):
            raise ValueError("cache_salt must not contain '@', '/', '\\' or NUL")
        if len(self.cache_salt) > 128:
            raise ValueError("cache_salt exceeds max length 128")

@dataclass(frozen=True, init=False)
class ObjectKey:

    """
    The unique identifier for an object in the distributed storage manager
    """

    identity: ObjectKeyIdentity

    extra_attributes: dict[str, object] = field(
        default_factory=dict, compare=False, repr=False
    )

    """Request-scoped metadata exposed to L2 adapters.

    This field is excluded from equality, hashing, and ``repr``.
    Upstream modules may enrich it before dispatch.
    L1 managers and L2 adapters should not mutate it.
    """

    def __init__(
        self,
        chunk_hash: bytes | None = None,
        model_name: str | None = None,
        kv_rank: int | None = None,
        object_group_id: int | None = None,
        cache_salt: str | None = None,
        *,
        identity: ObjectKeyIdentity | None = None,
        extra_attributes: dict[str, object] | None = None,
    ) -> None:
        """Build a key from its identity or the existing flat constructor.

        Explicit flat fields override the supplied identity. This also retains
        ``dataclasses.replace(key, object_group_id=...)`` and other flat field
        replacements used by existing consumers.

        Args:
            chunk_hash: Content hash; required without ``identity``.
            model_name: Model name; required without ``identity``.
            kv_rank: Packed KV rank; required without ``identity``.
            object_group_id: Object group; defaults to 0 without an identity.
            cache_salt: Isolation salt; defaults to empty without an identity.
            identity: Immutable identity providing fields not supplied explicitly.
            extra_attributes: Request-scoped metadata exposed to L2 adapters,
                shallow-copied so the key can be enriched independently.

        Raises:
            TypeError: A required flat field is missing without ``identity``.
            ValueError: An identity field violates its invariant.
        """
        if identity is None:
            if chunk_hash is None or model_name is None or kv_rank is None:
                raise TypeError("chunk_hash, model_name and kv_rank are required")
            identity = ObjectKeyIdentity(
                chunk_hash=chunk_hash,
                model_name=model_name,
                kv_rank=kv_rank,
                object_group_id=object_group_id if object_group_id is not None else 0,
                cache_salt=cache_salt if cache_salt is not None else "",
            )
        elif (
            chunk_hash is not None
            or model_name is not None
            or kv_rank is not None
            or object_group_id is not None
            or cache_salt is not None
        ):
            identity = replace(
                identity,
                chunk_hash=chunk_hash
                if chunk_hash is not None
                else identity.chunk_hash,
                model_name=model_name
                if model_name is not None
                else identity.model_name,
                kv_rank=kv_rank if kv_rank is not None else identity.kv_rank,
                object_group_id=(
                    object_group_id
                    if object_group_id is not None
                    else identity.object_group_id
                ),
                cache_salt=cache_salt
                if cache_salt is not None
                else identity.cache_salt,
            )
        object.__setattr__(self, "identity", identity)
        object.__setattr__(
            self,
            "extra_attributes",
            dict(extra_attributes) if extra_attributes is not None else {},
        )

    @property
    def cache_salt(self) -> str:
        """Read cache_salt from the identity."""
        return self.identity.cache_salt

def ipc_key_to_object_keys(
    ipc_key: "IPCCacheServerKey",
    chunk_hashes: list[bytes],
    object_group_ids: list[int],
) -> list[list[ObjectKey]]:
    """
    Convert a single IPCCacheServerKey and its chunk hashes to per-object-group
    lists of ObjectKey.

    When the ipc_key's worker_id is None, each chunk hash is exploded into
    multiple ObjectKeys (one per worker in world_size).

    ``cache_salt`` is read directly from ``ipc_key`` so the produced
    ObjectKeys are per-user isolated whenever the sender set a non-empty
    salt. There is intentionally no separate ``cache_salt`` parameter —
    duplicating the source of truth would risk silent isolation bugs
    where a caller passes ``ipc_key`` but forgets the salt.

    ``request_configs`` is likewise read from ``ipc_key`` and exposed through
    every produced key's ``extra_attributes`` field. Each key receives a
    shallow copy so modules can enrich one key without affecting another.

    Args:
        ipc_key: The IPC key providing model_name, world_size, worker_id,
            cache_salt, and request_configs.
        chunk_hashes: List of chunk hash bytes, one per chunk.
        object_group_ids: Object group ids to produce keys for.

    Returns:
        list[list[ObjectKey]]: The i-th element is the list of ObjectKeys
        for ``object_group_ids[i]``.
    """
    cache_salt = ipc_key.cache_salt
    extra_attributes = (
        ipc_key.request_configs if ipc_key.request_configs is not None else {}
    )

    # The (chunk_hash, kv_rank) expansion is independent of the object group,
    # so compute it once and reuse it for every group.
    kv_ranks = _lookup_kv_ranks(ipc_key)

    return [
        [
            ObjectKey(
                chunk_hash=chunk_hash,
                model_name=ipc_key.model_name,
                kv_rank=kv_rank,
                object_group_id=object_group_id,
                cache_salt=cache_salt,
                extra_attributes=extra_attributes,
            )
            for chunk_hash in chunk_hashes
            for kv_rank in kv_ranks
        ]
        for object_group_id in object_group_ids
    ]

