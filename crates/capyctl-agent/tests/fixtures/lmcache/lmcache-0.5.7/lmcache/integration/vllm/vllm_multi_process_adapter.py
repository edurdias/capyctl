# Excerpt of LMCache v0.5.7 lmcache/integration/vllm/vllm_multi_process_adapter.py (Apache-2.0), kept for CapyCTL's LMCache launch-time check tests.
# Source: https://github.com/LMCache/LMCache/blob/v0.5.7/lmcache/integration/vllm/vllm_multi_process_adapter.py

class ExtraConfigDefault(enum.Enum):
    """Centralized default values for extra_config keys.

    Each member's *name* is the key used in the extra_config dict, and
    its ``default`` attribute is the default value. Defaults live in an
    attribute rather than the enum value because equal enum values
    silently alias members (two ``False`` defaults would collapse into
    one member); each member's ``_value_`` is a unique ordinal instead.
    """

    #: The default value for this key (annotation only -- enum members
    #: are created from the assigned literals below, not from this).
    default: Any

    def __new__(cls, default: Any) -> "ExtraConfigDefault":
        obj = object.__new__(cls)
        obj._value_ = len(cls.__members__)
        obj.default = default
        return obj

    # Timeout (seconds) for blocking MQ requests: initial
    # chunk-size query, KV cache registration/unregistration,
    # and other synchronous operations.
    mq_timeout = 300.0
    # Interval (seconds) between periodic heartbeat pings
    # to the server.
    heartbeat_interval = 10.0
    # Poll status replies without blocking the scheduler by default.
    nonblocking_lookup_status = True
    # Routing mode for ``create_transfer_context``: ``auto`` resolves to
    # the device spec's declared default (derived from
    # ``is_lmcache_driven_available()``: CUDA and NPU -> lmcache_driven;
    # opt-in-only stacks and others -> engine_driven);
    # ``lmcache_driven`` forces the IPC / SHM zero-copy path where the
    # LMCache server pulls data via device handles;
    # ``engine_driven`` forces the worker-side gather/scatter copy path.
    # Mirrors the ``LMCACHE_MP_TRANSFER_MODE`` env var; this extra_config
    # key wins when both are set.
    mp_transfer_mode = "auto"
    # Whether IPC mechanisms must work across isolated containers (no
    # shared host IPC namespace or /dev/shm); see
    # lmcache.v1.platform.ipc_policy. Must match the LMCache server's
    # ``--isolated-ipc`` setting.
    isolated_ipc = False
    # Whether the engine allocates its KV cache through the CUDA VMM API
    # (vLLM's ``--enable-cumem-allocator``), so KV registration must use
    # VMM IPC instead of legacy CUDA IPC handles; see
    # lmcache.v1.platform.ipc_policy.
    use_vmm_api = False
    # Must match the MP server's --hash-algorithm setting because KV events
    # expose the same chunk hashes used by server-side object keys.
    hash_algorithm = "blake3"

_EXTRA_CONFIG_KEY_PREFIX = "lmcache.mp."

