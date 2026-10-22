# Excerpt of LMCache v0.5.6 lmcache/v1/distributed/config.py (Apache-2.0), kept for CapyCTL's LMCache launch-time check tests.
def add_storage_manager_args(
    parser: argparse.ArgumentParser,

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
            'map. Set --no-l1-use-lazy and --shm-name "". '
            "If a DAX L2 adapter with the same device_path is registered, "
            "that adapter's max_dax_size_gb is used as L1 overflow size."
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

