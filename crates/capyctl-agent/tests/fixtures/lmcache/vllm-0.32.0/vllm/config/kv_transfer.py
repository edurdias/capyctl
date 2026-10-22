# Excerpt of vLLM v0.32.0 vllm/config/kv_transfer.py (Apache-2.0), kept for CapyCTL's LMCache launch-time check tests.
class KVTransferConfig:
    """Configuration for distributed KV cache transfer."""

    kv_connector: str | None = None
    """The KV connector for vLLM to transmit KV caches between vLLM instances.
    """

    engine_id: str | None = None
    """The engine id for KV transfers."""

    kv_buffer_device: str = field(default_factory=kv_buffer_device_default_factory)
    """The device used by kv connector to buffer the KV cache. Choices are
    'cuda', 'cpu' and 'xpu'."""

    kv_role: KVRole | None = None
    """Whether this vLLM instance produces, consumes KV cache, or both. Choices
    are 'kv_producer', 'kv_consumer', and 'kv_both'."""

    kv_port: int = 14579
    """The KV connector port, used to build distributed connection."""

    kv_connector_extra_config: dict[str, Any] = field(default_factory=dict)
    """any extra config that the connector may need."""

    kv_connector_module_path: str | None = None
    """The Python module path to dynamically load the KV connector from.
    Only supported in V1."""

    enable_permute_local_kv: bool = False

