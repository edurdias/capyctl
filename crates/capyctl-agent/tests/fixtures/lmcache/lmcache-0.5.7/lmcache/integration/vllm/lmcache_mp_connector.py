# Excerpt of LMCache v0.5.7 lmcache/integration/vllm/lmcache_mp_connector.py (Apache-2.0), kept for CapyCTL's LMCache launch-time check tests.
# Source: https://github.com/LMCache/LMCache/blob/v0.5.7/lmcache/integration/vllm/lmcache_mp_connector.py

class LMCacheMPConnector(KVConnectorBase_V1, SupportsHMA):

    """
    The connector for LMCache multi-process mode.

    Extra configs (kv_transfer_config.extra_config):

    Multi-server deployment:
    - lmcache.mp.server_urls: server URL list or comma-separated string,
      e.g. "tcp://host1:6667,tcp://host2:6667".

    Single-server deployment:
    - lmcache.mp.host: the host of the LMCache server.
    - lmcache.mp.port: the port of the LMCache server.

    - lmcache.mp.mq_timeout: timeout (seconds) for message queue requests.
    - lmcache.mp.heartbeat_interval: interval (seconds) between server
      heartbeat pings.
    - lmcache.mp.eager_prefetch: submit the LMCache lookup when a request
      enters vLLM's waiting queue. Disabled by default.
    - lmcache.mp.save_decode_cache: also store chunks containing generated
      tokens. Defaults to False; only complete prompt chunks are stored.
    """

