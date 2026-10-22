# Excerpt of LMCache v0.5.6 lmcache/integration/vllm/lmcache_mp_connector.py (Apache-2.0), kept for CapyCTL's LMCache launch-time check tests.
class LMCacheMPConnector(KVConnectorBase_V1, SupportsHMA):
    """
    The connector for LMCache multi-process mode.

    Extra configs (kv_transfer_config.extra_config):

    Multi-server deployment:
    - lmcache.mp.server_urls: server URL list or comma-separated string,
      e.g. "tcp://host1:6667,tcp://host2:6667".

    Single-server deployment:

