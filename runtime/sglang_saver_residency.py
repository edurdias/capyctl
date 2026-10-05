"""Saver residency of an enrolled SGLang 0.5.20 scheduler on torch-memory-saver 0.0.10.

SPEC §9.2: SGLang's release must be verified by what is actually mapped, not by
the release route answering. torch-memory-saver 0.0.10 exports no allocation
snapshot (verified read-only on host-a, 2026-09-23: its preload library
exports tms_pause, tms_resume and the region setters only), so the patched
`tms_snapshot_v1` reader (memory_saver_observer) cannot run against it. This
module observes the same aggregate facts another way:

- The saver's allocations are the segments of its per-tag torch MemPools
  (`_TorchMemorySaverImpl._mem_pools`, keyed by tag, backup flags and device).
  Under hook mode `preload` each segment is one cudaMalloc the preload library
  served with CUDA VMM (cuMemCreate + cuMemMap).
- A pause unmaps and releases that physical memory and keeps the virtual
  reservation; a resume maps fresh physical memory at the same address.
- So whether physical memory backs a segment is read from the driver with
  `cuMemRetainAllocationHandle` at the segment address (the retained handle is
  released at once): success is mapped, CUDA_ERROR_INVALID_VALUE is unmapped,
  anything else fails the observation closed.

Call only on the enrolled scheduler thread at a safe point (the bridge's tick),
after the saver has initialized. Nothing here imports an engine package, loads a
library (libcuda is only looked up if already loaded), calls a saver control, or
turns mapped bytes into readiness, release or qualification evidence; the host
fuses these facts with its own. CPU and fake-driver tests are not qualification.
"""

import ctypes
import os
import sys

from . import sglang_saver_binding as saver
from .memory_saver_observer import AllocationAggregate, ObservationError, SaverObservation
from .sglang_scheduler_observer import BridgeError


# Tags the SGLang recipe allocates in saver regions (sglang/srt/constants.py).
# CUDA graph memory is captured through the saver's graph path, not a MemPool.
TAGS = frozenset({"weights", "kv_cache"})
# The preload library's own exports this installation serves (no snapshot).
EXPORTS = ("tms_pause", "tms_resume", "tms_set_current_tag", "tms_set_interesting_region",
           "tms_set_enable_cpu_backup")
# The saver switches the recipe renders (sglang_saver_binding.check_values).
_SWITCHES = ("enable_memory_saver", "enable_weights_cpu_backup",
             "enable_draft_weights_cpu_backup")
# torch-memory-saver 0.0.10's host shadows for a CPU-backup region (its
# CpuBackupBackend); either keeps the weights copy in host RAM.
_BACKUP_BACKENDS = frozenset({"pinned", "mmap"})
_MAX_SEGMENTS = 4096
_CUDA_SUCCESS = 0
_CUDA_ERROR_INVALID_VALUE = 1
# The single-rank topology the recipe renders (sglang_server_args).
_TOPOLOGY = dict(tp_size=1, dp_size=1, pp_size=1, ep_size=1, dcp_size=1, attn_cp_size=1,
                 moe_dp_size=1, enable_dp_attention=False, enable_dp_lm_head=False,
                 enable_prefill_cp=False, speculative_algorithm=None, disaggregation_mode="null")


def server_arg(args, name):
    """One declared ServerArgs field, bypassing properties and lazy getters.

    SGLang 0.5.20's ServerArgs is a msgspec Struct whose fields live in slots,
    not `__dict__`; an older dataclass-shaped record keeps them in `__dict__`.
    """
    fields = getattr(type(args), "__struct_fields__", None)
    if type(fields) is tuple:
        if name not in fields:
            raise saver.SaverBindingError("invalid_chain")
        return object.__getattribute__(args, name)
    values = saver._fields(args)
    if name not in values:
        raise saver.SaverBindingError("invalid_chain")
    return values[name]


def topology(scheduler, weight_restore="disk_reload"):
    """The recipe's single-rank topology, or BridgeError('topology').

    ADR 0014 A17: speculative decoding (a named `speculative_algorithm`) is
    admitted only for a `resident` launch. SGLang 0.5.21 releases the draft
    model's weights with the target's and reloads every weight runner from the
    target's checkpoint, so only a park that never releases the weights region
    keeps the draft intact.
    """
    try:
        args = saver._exact(saver._fields(scheduler).get("server_args"),
                            "sglang.srt.server_args", "ServerArgs")
        for name, value in _TOPOLOGY.items():
            current = server_arg(args, name)
            if (name == "speculative_algorithm" and weight_restore == "resident"
                    and type(current) is str and current != ""):
                continue
            if type(current) is not type(value) or current != value:
                raise BridgeError("topology")
    except BridgeError:
        raise
    except Exception:
        raise BridgeError("topology") from None


class CudaDriver:
    """`cuMemRetainAllocationHandle` over the libcuda this process already loaded."""

    def __init__(self):
        try:
            # RTLD_NOLOAD: never load a driver the engine did not load itself.
            library = ctypes.CDLL("libcuda.so.1", mode=os.RTLD_NOLOAD | ctypes.RTLD_LOCAL)
            retain = library.cuMemRetainAllocationHandle
            retain.argtypes = [ctypes.POINTER(ctypes.c_ulonglong), ctypes.c_void_p]
            retain.restype = ctypes.c_int
            release = library.cuMemRelease
            release.argtypes = [ctypes.c_ulonglong]
            release.restype = ctypes.c_int
        except Exception:
            raise ObservationError("unsupported") from None
        self._retain = retain
        self._release = release

    def mapped(self, address):
        handle = ctypes.c_ulonglong(0)
        status = self._retain(ctypes.byref(handle), ctypes.c_void_p(address))
        if status == _CUDA_SUCCESS:
            if self._release(handle) != _CUDA_SUCCESS:
                raise ObservationError("internal")
            return True
        if status == _CUDA_ERROR_INVALID_VALUE:
            return False
        raise ObservationError("internal")


def _chain(scheduler, build, weight_restore="disk_reload"):
    """The exact object chain from the Scheduler to the saver's pools and library."""
    saver._exact(scheduler, "sglang.srt.managers.scheduler", "Scheduler")
    args = saver._exact(saver._fields(scheduler).get("server_args"),
                        "sglang.srt.server_args", "ServerArgs")
    saver.check_values({name: server_arg(args, name) for name in _SWITCHES}, weight_restore)
    module_name = "sglang.srt.utils.torch_memory_saver_adapter"
    adapter = saver._exact(saver._fields(scheduler).get("memory_saver_adapter"), module_name,
                           "_TorchMemorySaverAdapterReal")
    module = vars(sys.modules[module_name])
    if module.get("import_error", True) is not None:
        raise saver.SaverBindingError("invalid_chain")
    instance = saver._exact(module.get("_memory_saver"), "torch_memory_saver.entrypoint",
                            "TorchMemorySaver")
    package = sys.modules.get("torch_memory_saver")
    if package is None or vars(package).get("torch_memory_saver") is not instance:
        raise saver.SaverBindingError("invalid_chain")
    impl = saver._fields(instance).get("_impl")
    if impl is None:
        raise saver.SaverBindingError("uninitialized")
    saver._exact(impl, "torch_memory_saver.entrypoint", "_TorchMemorySaverImpl")
    values = saver._fields(impl)
    if values.get("_hook_mode") != build.hook_mode:
        raise saver.SaverBindingError("configuration_mismatch")
    hook = saver._exact(values.get("_hook_util"), "torch_memory_saver.hooks.mode_preload",
                        "HookUtilModePreload")
    wrapper = saver._exact(values.get("_binary_wrapper"), "torch_memory_saver.binary_wrapper",
                           "BinaryWrapper")
    cdll = saver._fields(wrapper).get("cdll")
    if type(cdll) is not saver._CDLL_TYPE or cdll._name != build.path:
        raise saver.SaverBindingError("library_mismatch")
    pools = values.get("_mem_pools")
    if not isinstance(pools, dict):
        raise saver.SaverBindingError("invalid_chain")
    return args, adapter, instance, impl, hook, wrapper, cdll, pools


def _pool_class():
    module = sys.modules.get("torch.cuda.memory")
    pool = vars(module).get("MemPool") if module is not None else None
    if pool is None:
        raise saver.SaverBindingError("invalid_chain")
    return pool


def _integer(value, low, high=(1 << 64) - 1):
    if type(value) is not int or not low <= value <= high:
        raise ObservationError("invalid")
    return value


def _region_backup(tag, cpu_backup, backend, weight_restore):
    """ADR 0019: whether a region key's CPU-backup flags are the recipe's.

    Only the weights region of a launch that declared `cpu_backup` (the
    `host_backed` tier) has a backup, in a host-RAM backend; every other region
    has none, and a declared backup the weights region lacks is refused too.
    """
    expected = tag == "weights" and weight_restore == "cpu_backup"
    if cpu_backup is not expected:
        return False
    return backend in _BACKUP_BACKENDS if expected else backend == ""


def observe_pools(pools, driver, weight_restore="disk_reload"):
    """Aggregate mapped and paused saver segments per device and tag."""
    pool_class = _pool_class()
    grouped = {}
    ranges = {}
    seen = 0
    for key, pool in list(pools.items()):
        # (tag, cpu backup, disk backup, cpu backup backend, device): the
        # recipe allows no disk backup, and a CPU backup only for the weights
        # of a `host_backed` launch (ADR 0019); a deep launch reloads from disk.
        if type(key) is not tuple or len(key) != 5:
            raise ObservationError("invalid")
        tag, cpu_backup, disk_backup, backend, device = key
        if (type(tag) is not str or tag not in TAGS
                or not _region_backup(tag, cpu_backup, backend, weight_restore)
                or disk_backup is not False or type(device) is not int
                or device < 0):
            raise ObservationError("unsupported")
        if type(pool) is not pool_class:
            raise ObservationError("invalid")
        segments = pool.snapshot(include_traces=False)
        if type(segments) is not list:
            raise ObservationError("invalid")
        for segment in segments:
            seen += 1
            if seen > _MAX_SEGMENTS:
                raise ObservationError("overflow")
            if type(segment) is not dict:
                raise ObservationError("invalid")
            address = _integer(segment.get("address"), 1)
            size = _integer(segment.get("total_size"), 1)
            if _integer(segment.get("device"), 0, (1 << 31) - 1) != device:
                raise ObservationError("invalid")
            if size > (1 << 64) - 1 - address:
                raise ObservationError("overflow")
            ranges.setdefault(device, []).append((address, address + size))
            mapped = driver.mapped(address)
            counters = grouped.setdefault((device, tag), [0] * 5)
            counters[0] += 1
            counters[1 if mapped else 2] += 1
            counters[3] += size
            counters[4] += size if mapped else 0
    for device_ranges in ranges.values():
        device_ranges.sort()
        previous = 0
        for start, end in device_ranges:
            if start < previous:
                raise ObservationError("invalid")
            previous = end
    groups = tuple(AllocationAggregate(device, tag, count, active, paused, virtual, mapped, 0, 0)
                   for (device, tag), (count, active, paused, virtual, mapped)
                   in sorted(grouped.items()))
    return SaverObservation(groups, sum(g.allocation_count for g in groups),
                            sum(g.virtual_bytes for g in groups),
                            sum(g.mapped_bytes for g in groups), 0)


def observe_scheduler_saver(scheduler, *, expected_owner, build, driver=None,
                            weight_restore="disk_reload"):
    """One observation of the enrolled scheduler's saver pools; see the module notes.

    `weight_restore` is the launch's declared restore (the enrollment scope
    carries it from the protected entry): `cpu_backup` admits the weights
    region's host-RAM backup, `disk_reload` admits no backup at all.
    Rechecks the object chain, process and backing file afterwards, as the
    patched-saver reader does. Point-in-time facts only.
    """
    try:
        if (type(build) is not saver.TrustedSaverBuild or type(build.path) is not str
                or type(build.sha256) is not str or len(build.sha256) != 64
                or any(char not in "0123456789abcdef" for char in build.sha256)
                or build.hook_mode != "preload"):
            raise saver.SaverBindingError("configuration_mismatch")
        owner = saver.current_process_identity()
        if type(expected_owner) is not saver.ProcessIdentity or expected_owner != owner:
            raise saver.SaverBindingError("owner_mismatch")
        chain = _chain(scheduler, build, weight_restore)
        library = saver._library(build, chain[6], exports=EXPORTS)
        allocations = observe_pools(chain[7], driver if driver is not None else CudaDriver(),
                                    weight_restore)
        current = _chain(scheduler, build, weight_restore)
        if (any(left is not right for left, right in zip(chain, current))
                or saver.current_process_identity() != owner
                or saver._library(build, current[6], exports=EXPORTS) != library):
            raise saver.SaverBindingError("changed")
        return saver.SchedulerSaverObservation(owner, library, build.hook_mode, allocations)
    except ObservationError as error:
        raise saver.SaverBindingError(error.status) from None
    except saver.SaverBindingError:
        raise
    except Exception:
        raise saver.SaverBindingError("invalid") from None
