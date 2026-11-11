# Excerpt of LMCache v0.5.6 lmcache/integration/vllm/vllm_multi_process_adapter.py (Apache-2.0), kept for CapyCTL's LMCache launch-time check tests.
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


# Backward-compatible aliases for callers that still pass these as
# positional/keyword args.
DEFAULT_MQ_TIMEOUT: float = ExtraConfigDefault.mq_timeout.default
DEFAULT_HEARTBEAT_INTERVAL: float = ExtraConfigDefault.heartbeat_interval.default

_EXTRA_CONFIG_KEY_PREFIX = "lmcache.mp."
