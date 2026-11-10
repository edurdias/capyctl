# Excerpt of LMCache v0.5.7 lmcache/v1/distributed/config.py (Apache-2.0), kept for CapyCTL's LMCache launch-time check tests.
# Source: https://github.com/LMCache/LMCache/blob/v0.5.7/lmcache/v1/distributed/config.py

def add_storage_manager_args(
    parser: argparse.ArgumentParser,
) -> argparse.ArgumentParser:
    """
    Add storage manager configuration arguments to an existing parser.

    This function allows other modules to integrate storage manager arguments
    into their own argument parsers. Arguments are organized into groups to
    avoid naming conflicts with other modules.

    Args:
        parser: The argument parser to add arguments to.

    Returns:
        argparse.ArgumentParser: The same parser with storage manager
            arguments added.

    Example:
        >>> # In another module that needs its own arguments
        >>> parser = argparse.ArgumentParser(description="My Application")
        >>> parser.add_argument("--my-arg", type=str)
        >>> add_storage_manager_args(parser)
        >>> args = parser.parse_args()
        >>> config = parse_args_to_config(args)
    """
    # First Party
    from lmcache.v1.gpu_connector._gds_backends import available_backends

    parser.add_argument(
        "--l1-manager",
        action="append",
        default=[],
        metavar="JSON",
        help=(
            "Tagged L1 JSON: type (DRAM, DEVDAX, GDS), tag, size_gb "
            "and backend fields. Repeat for multiple L1s."
        ),
    )
    # L1 Memory Manager Config
    memory_group = parser.add_argument_group(
        "L1 Memory Manager", "Configuration for L1 memory manager"
    )
    memory_group.add_argument(
        "--l1-size-gb",
        type=float,
        default=None,
        help="The size of L1 memory in GB.",
    )
    memory_group.add_argument(
        "--l1-use-lazy",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="Whether to use lazy loading for L1 memory. (Default is True)",
    )
    memory_group.add_argument(
        "--l1-init-size-gb",
        type=int,
        default=20,
        help="The initial size (GB) when using lazy allocation. Default is 20.",
    )
    memory_group.add_argument(
        "--l1-align-bytes",
        type=int,
        default=4096,
        help="The alignment size in bytes. Default is 4KB (4096 bytes).",
    )
    memory_group.add_argument(
        "--l1-use-hugepages",
        action=argparse.BooleanOptionalAction,
        default=False,
        help=(
            "Allocate the L1 pool from the pre-allocated 2 MiB hugepage pool "
            "instead of regular pinned memory. Requires pre-allocated "
            "hugepages (sysctl vm.nr_hugepages) and is incompatible with "
            "shared memory (--shm-name) and lazy allocation (--l1-use-lazy). "
            "Default is False."
        ),
    )
    memory_group.add_argument(
        "--l1-devdax-path",
        type=str,
        default=None,
        help=(
            "Optional /dev/dax device or mmap-able file to use as the L1 "
            "backing arena. When set, L1 lazy allocation and SHM transfer "
            "advertising must be disabled because the L1 bytes live in the DAX "
            'map. Set --no-l1-use-lazy and --shm-name "". The whole L1 pool '
            "lives on the device; a dax L2 adapter must use another range."
        ),
    )
    memory_group.add_argument(
        "--l1-devdax-offset-gb",
        type=float,
        default=0.0,
        help=(
            "Byte offset into --l1-devdax-path, in GiB, where the L1 mapping "
            "starts (default 0). Lets several LMCache servers share one CXL "
            "device by mapping disjoint ranges. Must land on a page boundary; "
            "the kernel also enforces the device alignment (usually 2 MiB)."
        ),
    )
    memory_group.add_argument(
        "--l1-mlock",
        action="store_true",
        help=(
            "ROCm only: mlock the L1 buffer so memory compaction cannot migrate "
            "it. Requires sysctl vm.compact_unevictable_allowed=0 and an "
            "unlimited memlock ulimit (or CAP_IPC_LOCK)."
        ),
    )

    # GDS L1 tier (optional, opt-in via --gds-l1-path)
    gds_group = parser.add_argument_group(
        "GDS L1 tier",
        "Configuration for the GDS L1 tier. Setting --gds-l1-path makes the "
        "L1 medium an NVMe slab accessed via GPUDirect Storage DMA instead of "
        "pinned DRAM; --l1-size-gb then sizes the slab. cuFile, hipFile, and "
        "phx use a slab file, while uGDS uses a dedicated raw device. "
        "Disable byte-array L2 adapters when this is on.",
    )
    gds_group.add_argument(
        "--gds-l1-path",
        type=str,
        default=None,
        help="NVMe directory for cuFile/hipFile/phx, or /dev/ugds_drvX for "
        "uGDS. Setting this enables GDS L1.",
    )
    gds_group.add_argument(
        "--gds-l1-use-direct-io",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="Open the slab file with O_DIRECT (required for the GDS DMA fast "
        "path on ext4). Default True.",
    )
    gds_group.add_argument(
        "--gds-l1-backend",
        default="auto",
        choices=("auto", *available_backends()),
        help="GDS backend. auto selects the default for this environment.",
    )
    # L1 Manager Config (TTL settings)
    ttl_group = parser.add_argument_group(
        "L1 Manager TTL", "TTL configuration for L1 manager locks"
    )
    ttl_group.add_argument(
        "--l1-write-ttl-seconds",
        type=int,
        default=600,
        help="Time to live for each object's write lock. Default is 600s.",
    )
    ttl_group.add_argument(
        "--l1-read-ttl-seconds",
        type=int,
        default=300,
        help="Time to live for each object's read lock. Default is 300s.",
    )

    # Eviction Config
    eviction_group = parser.add_argument_group(
        "Eviction Policy", "Configuration for eviction policies"
    )
    eviction_group.add_argument(
        "--eviction-policy",
        type=str,
        choices=["LRU", "ARC", "IsolatedLRU", "noop"],
        default=None,
        help="The eviction policy to use ('LRU', 'ARC', 'IsolatedLRU', or 'noop'). "
        "'ARC' balances recent and frequently reused keys adaptively. "
        "'IsolatedLRU' maintains one LRU list per cache_salt and requires "
        "quotas keyed by cache_salt to be configured via the HTTP API.",
    )
    eviction_group.add_argument(
        "--eviction-trigger-watermark",
        type=float,
        default=0.8,
        help="The memory usage watermark to trigger eviction (0.0 to 1.0). "
        "Default is 0.8.",
    )
    eviction_group.add_argument(
        "--eviction-ratio",
        type=float,
        default=0.2,
        help="The fraction of memory to evict when triggered (0.0 to 1.0). "
        "Default is 0.2.",
    )

    # L2 Policies
    # Import here to break circular dependency:
    # config.py <-> storage_controllers (via eviction_controller)
    # Safe because config.py is fully initialized by the time this
    # function is called.
    # First Party
    from lmcache.v1.distributed.storage_controllers.prefetch_policy import (
        get_registered_prefetch_policies,
    )
    from lmcache.v1.distributed.storage_controllers.store_policy import (
        get_registered_store_policies,
    )
    import lmcache.v1.distributed.storage_controllers  # noqa: F401

    policy_group = parser.add_argument_group(
        "L2 Policies", "Store and prefetch policy selection for L2 adapters"
    )
    policy_group.add_argument(
        "--l2-store-policy",
        type=str,
        choices=get_registered_store_policies(),
        default="default",
        help="L2 store policy. Determines which adapters receive each key "
        "and whether keys are deleted from L1 after L2 store. "
        "Default is 'default' (store all keys to all adapters, keep L1).",
    )
    policy_group.add_argument(
        "--l2-prefetch-policy",
        type=str,
        choices=get_registered_prefetch_policies(),
        default="default",
        help="L2 prefetch policy. Determines which adapter loads each key "
        "when multiple adapters have it. "
        "Default is 'default' (pick the first adapter by index).",
    )
    policy_group.add_argument(
        "--l2-prefetch-inflight-watermark",
        type=float,
        default=0.5,
        help="Fraction of L1 capacity, in (0, 1], that in-flight L2 prefetch "
        "requests may stage before new prefetch requests wait. Default is 0.5.",
    )
    # Deprecated: released deployers (operator) still render this flag into
    # server args, and rejecting it crashes the pod on startup.
    policy_group.add_argument(
        "--l2-prefetch-max-in-flight",
        type=int,
        default=None,
        help="Deprecated and ignored; prefetch admission follows "
        "--l2-prefetch-inflight-watermark.",
    )
    policy_group.add_argument(
        "--periodic-notifier-interval-ms",
        type=int,
        default=5,
        help="Interval in ms for the periodic event notifier heartbeat. Default is 5.",
    )

    # Adapter config
    add_l2_adapters_args(parser)
    return parser

