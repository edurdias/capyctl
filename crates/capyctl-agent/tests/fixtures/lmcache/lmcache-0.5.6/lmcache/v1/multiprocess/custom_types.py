# Excerpt of LMCache v0.5.6 lmcache/v1/multiprocess/custom_types.py (Apache-2.0), kept for CapyCTL's LMCache launch-time check tests.
# SPDX-License-Identifier: Apache-2.0
# Standard
from dataclasses import dataclass, field
from typing import Any, Callable

# Third Party
import msgspec
import torch

# First Party
from lmcache.v1.platform.base.ipc_wrapper import (  # noqa: E402,F401
    DeviceIPCWrapper,
)

"""
Defines the types and the customized encoder/decoders for inter-process
communications.

Key Types:
- IPCCacheServerKey: Token-based cache key
  - Contains token_ids, start, end, request_id (all required)
  - Converted to ObjectKey for storage operations via ipc_key_to_object_keys()
"""


@dataclass(order=True, frozen=True)
class IPCCacheServerKey:
    """Cache key for the IPC (multiprocess) protocol.

    This key type is sent by the client over ZMQ (serialized via msgspec).

    The client sends token_ids, start, end, and request_id (all required).
    The server computes chunk hashes via TokenHasher and converts to
    ObjectKey for storage operations using ipc_key_to_object_keys().

    The request_id field is for session tracking and is NOT included
    in equality/hash comparisons (two keys with same content but different
    request_ids are considered equal for cache purposes).
    """

    model_name: str
    world_size: int
    worker_id: int | None

    token_ids: tuple[int, ...]  # frozen tuple for hashability
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


