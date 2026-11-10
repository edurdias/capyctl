# Excerpt of LMCache v0.5.7 lmcache/v1/multiprocess/config.py (Apache-2.0), kept for CapyCTL's LMCache launch-time check tests.
# Source: https://github.com/LMCache/LMCache/blob/v0.5.7/lmcache/v1/multiprocess/config.py

def add_mp_server_args(
    parser: argparse.ArgumentParser,
) -> argparse.ArgumentParser:
    """
    Add MP server configuration arguments to an existing parser.

    Args:
        parser: The argument parser to add arguments to.

    Returns:
        The same parser with MP server arguments added.
    """
    add_api_key_args(parser)
    mp_group = parser.add_argument_group(
        "MP Server", "Configuration for the multiprocess cache server"
    )
    mp_group.add_argument(
        "--instance-id",
        type=str,
        default=None,
        help="Stable identity of this MP server. Used as the coordinator "
        "membership key and as the OTel 'service.instance.id' resource "
        "attribute on every metric and span. Defaults to a random UUID v4 "
        "minted at startup.",
    )
    mp_group.add_argument(
        "--transport",
        choices=("zmq", "grpc"),
        default="zmq",
        help="Request transport exposed by the cache server. Default is zmq.",
    )
    mp_group.add_argument(
        "--host",
        type=str,
        default="localhost",
        help="Host to bind the request server. Default is localhost.",
    )
    mp_group.add_argument(
        "--port",
        type=int,
        default=5555,
        help="Port to bind the request server. Default is 5555.",
    )
    mp_group.add_argument(
        "--chunk-size",
        type=int,
        default=256,
        help="Minimum chunk size for KV cache operations. The vLLM connector "
        "may negotiate this upward to satisfy model-specific KV geometry. "
        "Default is 256.",
    )
    mp_group.add_argument(
        "--null-block-id",
        type=int,
        default=0,
        help="Engine block ID that denotes absent KV data. Default is 0.",
    )
    mp_group.add_argument(
        "--max-workers",
        type=int,
        default=1,
        help="Base number of worker threads for both GPU and CPU pools. "
        "Default is 1. Can be overridden per-pool with "
        "--max-gpu-workers and --max-cpu-workers.",
    )
    mp_group.add_argument(
        "--max-gpu-workers",
        type=int,
        default=None,
        help="Worker threads for the GPU affinity pool (STORE/RETRIEVE). "
        "Defaults to --max-workers if not specified.",
    )
    mp_group.add_argument(
        "--max-cpu-workers",
        type=int,
        default=None,
        help="Worker threads for the normal CPU pool (LOOKUP, etc.). "
        "Defaults to --max-workers if not specified.",
    )
    mp_group.add_argument(
        "--grpc-server-workers",
        type=int,
        default=32,
        help="Worker threads for gRPC request dispatch. Only used by "
        "--transport grpc. Default is 32.",
    )
    mp_group.add_argument(
        "--grpc-server-mode",
        choices=("sync", "aio", "native"),
        default="sync",
        help="gRPC server implementation. aio receives requests on one event "
        "loop and hands only the handlers to worker threads. native does the "
        "HTTP/2 and gRPC framing in Rust, off the GIL, and needs the "
        "pygrpc-native package. Only used by --transport grpc. "
        "Default is sync.",
    )
    mp_group.add_argument(
        "--hash-algorithm",
        type=str,
        default="blake3",
        help="Hash algorithm for token-based operations "
        "(builtin, sha256_cbor, blake3). Default is blake3.",
    )
    mp_group.add_argument(
        "--engine-type",
        type=str,
        default="default",
        choices=["default", "blend"],
        help="Cache engine backend type. 'default' uses standard prefix caching; "
        "'blend' composes the blend module for non-prefix KV reuse. "
        "Default is 'default'.",
    )
    mp_group.add_argument(
        "--supported-transfer-mode",
        type=str,
        default="lmcache_driven",
        choices=["lmcache_driven", "engine_driven", "auto"],
        help="Supported transfer mode: 'lmcache_driven' for server-driven "
        "transfer (STORE/RETRIEVE, supports CUDA IPC and CPU SHM), "
        "'engine_driven' for engine-driven transfer (PREPARE/COMMIT), "
        "or 'auto' to enable both transfer paths. "
        "Default is 'lmcache_driven'.",
    )
    mp_group.add_argument(
        "--isolated-ipc",
        action=argparse.BooleanOptionalAction,
        default=False,
        help="Assume engine workers and this server run in containers that "
        "share no host IPC namespace or /dev/shm, and use IPC mechanisms "
        "that work there (CUDA: timeline-semaphore events instead of "
        "interprocess event handles). Must match the engine workers' "
        "lmcache.mp.isolated_ipc setting. (Default is False)",
    )
    mp_group.add_argument(
        "--runtime-plugin-locations",
        type=str,
        nargs="*",
        default=[],
        help="Paths to runtime plugin scripts or "
        "directories to launch alongside the server.",
    )
    mp_group.add_argument(
        "--runtime-plugin-config",
        type=str,
        default="{}",
        help="JSON string of extra key-value config forwarded to runtime "
        "plugins via LMCACHE_RUNTIME_PLUGIN_EXTRA_CONFIG. "
        'Example: \'{"plugin.frontend.heartbeat_url": '
        '"http://localhost:5000/heartbeat"}\'',
    )
    mp_group.add_argument(
        "--shm-name",
        type=str,
        default="",
        help="SHM segment name for engine-driven KV transfer. "
        'Default "" (not specified): disable SHM. '
        "Set to a name to create and use that specific SHM segment. "
        "(Only use this for engine_driven transfer mode, see "
        "`--supported-transfer-mode`.) ",
    )
    mp_group.add_argument(
        "--script-allowed-imports",
        type=str,
        nargs="*",
        default=[],
        help="Python modules that the /run_script endpoint is allowed to "
        "import. Example: --script-allowed-imports numpy pandas",
    )
    mp_group.add_argument(
        "--run-script-api-enabled",
        action=argparse.BooleanOptionalAction,
        default=False,
        help="Enable the /run_script HTTP endpoint, which executes "
        "caller-supplied Python in-process (full remote code execution; the "
        "restricted builtins are not a security boundary). Default is False. "
        "Only enable it on a trusted network.",
    )
    mp_group.add_argument(
        "--separate-object-groups",
        action=argparse.BooleanOptionalAction,
        default=False,
        help="Split kernel groups into one object group per sliding-window size "
        "at KV-cache registration (for hybrid models). (Default is False)",
    )
    mp_group.add_argument(
        "--worker-reap-timeout-seconds",
        type=float,
        default=120.0,
        help="Silence budget (s) before a ping-proven worker's KV cache "
        "registration is reaped. 0 disables reaping. Must be >= 3 x the "
        "engine adapter's heartbeat interval. Default is 120.",
    )
    mp_group.add_argument(
        "--session-ttl-seconds",
        type=float,
        default=600.0,
        help="Seconds a request session may stay idle before it is reaped. "
        "Raise it above the longest engine queueing delay. Default is 600.",
    )
    mp_group.add_argument(
        "--kv-load-policy",
        type=str,
        default="DEFAULT",
        help="Per-request choice between serving a lookup and letting the "
        "engine recompute the prompt. DEFAULT serves every lookup unless the "
        "request sets lmcache.skip_load; 'package.module:ClassName' loads a "
        "custom KVLoadPolicy, constructed with --runtime-plugin-config.",
    )
    mp_group.add_argument(
        "--worker-registration-grace-seconds",
        type=float,
        default=3600.0,
        help="Silence budget (s) for a worker that registered but never "
        "pinged (model warmup or early death). Must be >= the worker reap "
        "timeout. Default is 3600.",
    )
    mp_group.add_argument(
        "--enable-segmented-prefix",
        action="store_true",
        help="CacheBlend (--engine-type blend) only: on a mid-prefix L2 "
        "retrieve failure, retain the gapped prefix so post-gap chunks stay "
        "L1-resident instead of truncating at the gap. No effect otherwise.",
    )
    mp_group.add_argument(
        "--enable-dedup-content",
        action="store_true",
        help="--engine-type blend only: skip fingerprint registration for a "
        "chunk whose content is already indexed, so the same text stored "
        "behind different prefixes is indexed once. No effect otherwise.",
    )
    mp_group.add_argument(
        "--enable-blend-reorder",
        action="store_true",
        help="--engine-type blend only: answer the cb_reorder_plan RPC, which "
        "orders a prompt so it starts with an exact copy of a cached prompt.",
    )
    mp_group.add_argument(
        "--kv-event-log-size",
        type=int,
        default=MPServerConfig.kv_event_log_size,
        help="CPU store/eviction records retained for KV-event subscribers. "
        "0 disables the channel. Default is %(default)s.",
    )
    mp_group.add_argument(
        "--server-module",
        action="append",
        default=[],
        help="JSON object describing a server module to load. "
        "Repeat to load multiple Python modules. Example: "
        '\'{"module_path":"my_pkg.server_module",'
        '"config":{"name":"demo"}}\'. '
        "Opt-in modules shipped with LMCache live under "
        "lmcache.v1.multiprocess.plugin_modules, e.g. "
        '\'{"module_path":"lmcache.v1.multiprocess.plugin_modules.qstore"}\' '
        "for query-tensor transfer.",
    )
    return parser

def add_http_frontend_args(
    parser: argparse.ArgumentParser,
) -> argparse.ArgumentParser:
    """
    Add HTTP frontend configuration arguments to an existing parser.

    Args:
        parser: The argument parser to add arguments to.

    Returns:
        The same parser with HTTP frontend arguments added.
    """
    http_group = parser.add_argument_group(
        "HTTP Frontend", "Configuration for the HTTP frontend server"
    )
    http_group.add_argument(
        "--http-host",
        type=str,
        default="127.0.0.1",
        help="Host to bind the HTTP server. Default is 127.0.0.1.",
    )
    http_group.add_argument(
        "--http-port",
        type=int,
        default=8080,
        help="Port to bind the HTTP server. Default is 8080.",
    )
    return parser

