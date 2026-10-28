# Excerpt of LMCache v0.5.6 lmcache/v1/multiprocess/config.py (Apache-2.0), kept for CapyCTL's LMCache launch-time check tests.
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
        help="Host to bind the HTTP server. Default is 127.0.0.1; the admin "
        "API has no authentication, so only bind a non-loopback address on a "
        "trusted network.",
    )
    http_group.add_argument(
        "--http-port",
        type=int,
        default=8080,
        help="Port to bind the HTTP server. Default is 8080.",
    )
    return parser


