# Excerpt of LMCache v0.5.6 lmcache/v1/distributed/api.py (Apache-2.0), kept for CapyCTL's LMCache launch-time check tests.


@dataclass(frozen=True)
class ObjectKey:
    """
    The unique identifier for an object in the distributed storage manager
    """

    chunk_hash: bytes
    """ Content hash of this particular chunk """

    model_name: str
    """ Name of the model this chunk belongs to.

    Invariant: must not contain ``@``. The L2 adapters use ``@`` as the
    field separator in serialized keys/filenames and rely on this
    invariant for unambiguous parsing. HuggingFace model IDs use
    alphanumerics + ``/-_.`` so this rejects nothing that appears in
    practice.
    """

    kv_rank: int
    """ The rank that uniquely identifies the slice of the KV cache """

    object_group_id: int = 0
    """ Index of the object group this chunk belongs to. """

    cache_salt: str = ""
    """ Per-user isolation salt. Same content from different users with
    different cache_salt values produces different ObjectKeys, giving
    strict per-user cache isolation. Defaults to empty string, in which
    case serialized keys and filenames match the pre-cache_salt shape
    (no trailing salt field) — no migration is needed for un-salted
    deployments.

    Invariant: must not contain ``@``, ``/``, ``\\``, or NUL. The L2
    adapters use ``@`` as the field separator; ``/`` and ``\\`` are
    filesystem path separators (FS adapter embeds the salt into
    filenames); NUL terminates C strings (C++ connector). Max length
    128 to stay well within ``NAME_MAX`` (255) after the model, rank,
    hash, and extension are added.
    """

    _SALT_FORBIDDEN_CHARS = frozenset("@/\\\x00")
    _SALT_MAX_LEN = 128

    def __post_init__(self) -> None:
        if "@" in self.model_name:
            raise ValueError(
                f"model_name must not contain '@' (got {self.model_name!r})"
            )
        if self.object_group_id < 0:
            raise ValueError(
                f"object_group_id must be >= 0 (got {self.object_group_id})"
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

    def to_encoded_object_key(self) -> "EncodedObjectKey":
        """Return the JSON-safe :class:`EncodedObjectKey` projection."""
        return EncodedObjectKey(
            chunk_hash_hex=self.chunk_hash.hex(),
            model_name=self.model_name,
            kv_rank=self.kv_rank,
            object_group_id=self.object_group_id,
            cache_salt=self.cache_salt,
        )

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

    Args:
        ipc_key: The IPC key providing model_name, world_size, worker_id,
            and cache_salt.
        chunk_hashes: List of chunk hash bytes, one per chunk.
        object_group_ids: Object group ids to produce keys for.

    Returns:
        list[list[ObjectKey]]: The i-th element is the list of ObjectKeys
        for ``object_group_ids[i]``.
    """
    cache_salt = ipc_key.cache_salt

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
            )

