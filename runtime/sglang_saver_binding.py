"""Scoped observation of an already initialized pinned scheduler's saver.

Call only inside the enrolled scheduler process, after protected pinned code has
initialized its saver. This module does not import engine packages, construct a
saver or CDLL, call lifecycle controls, or establish launch/qualification authority.
Loaded module classes and the build input are trusted service inputs, not evidence
against hostile Python code. Backing-file provenance is not a memory attestation
or proof that CUDA symbol interposition routes every allocation through this saver.
"""

from contextlib import ExitStack
import ctypes
from dataclasses import dataclass, field
import hashlib
import os
import re
import stat
import sys

from .checkpoint_preflight import _check_platform, _check_root, _open_chain
from .memory_saver_observer import ObservationError, SaverObservation, observe_saver


_CDLL_TYPE = ctypes.CDLL
_EXPORTS = ("tms_snapshot_v1", "tms_pause", "tms_resume", "tms_set_current_tag",
            "tms_set_interesting_region", "tms_set_enable_cpu_backup")
_CODES = frozenset({"owner_mismatch", "invalid_process", "invalid_chain", "uninitialized",
                    "configuration_mismatch", "library_mismatch", "unsafe_library",
                    "changed", "busy", "invalid", "overflow", "unsupported", "internal",
                    "backup_disallowed"})


class SaverBindingError(Exception):
    """Closed status without native exception text, paths, or allocation addresses."""

    def __init__(self, code):
        self.code = code if code in _CODES else "invalid"
        super().__init__(self.code)


@dataclass(frozen=True)
class ProcessIdentity:
    pid: int
    start_ticks: int
    boot_id: str


@dataclass(frozen=True)
class TrustedSaverBuild:
    """Service-reviewed installed artifact; never populate from a candidate request."""

    path: str = field(repr=False)
    sha256: str
    hook_mode: str


@dataclass(frozen=True)
class LoadedSaverLibrary:
    sha256: str
    device: int
    inode: int
    size: int
    mtime_ns: int
    ctime_ns: int


@dataclass(frozen=True)
class SchedulerSaverObservation:
    owner: ProcessIdentity
    library: LoadedSaverLibrary
    hook_mode: str
    allocations: SaverObservation


def _read(path, limit):
    with open(path, "rb") as stream:
        value = stream.read(limit + 1)
    if len(value) > limit:
        raise SaverBindingError("invalid")
    return value.decode("ascii", errors="strict")


def current_process_identity():
    """Read this process only; fixed trusted Linux procfs is a precondition."""
    try:
        _check_platform()
        boot = _read("/proc/sys/kernel/random/boot_id", 64).strip()
        if re.fullmatch(r"[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}", boot) is None:
            raise ValueError
        pid = os.getpid()
        text = _read("/proc/self/stat", 16384)
        head, tail = text.rsplit(") ", 1)
        if int(head.split(" (", 1)[0]) != pid:
            raise ValueError
        start_ticks = int(tail.split()[19])
        if start_ticks <= 0:
            raise ValueError
        if _read("/proc/sys/kernel/random/boot_id", 64).strip() != boot:
            raise ValueError
        return ProcessIdentity(pid, start_ticks, boot)
    except Exception:
        raise SaverBindingError("invalid_process") from None


def _fields(value):
    # Bypass user-defined properties and lazy getters in the pinned wrappers.
    try:
        fields = object.__getattribute__(value, "__dict__")
        if type(fields) is dict:
            return fields
    except (AttributeError, TypeError):
        pass
    raise SaverBindingError("invalid_chain")


def _exact(value, module_name, class_name):
    module = sys.modules.get(module_name)
    if module is None or type(value) is not vars(module).get(class_name):
        raise SaverBindingError("invalid_chain")
    return value


def _chain(scheduler, build):
    _exact(scheduler, "sglang.srt.managers.scheduler", "Scheduler")
    args = _exact(_fields(scheduler).get("server_args"), "sglang.srt.server_args", "ServerArgs")
    values = _fields(args)
    if (values.get("enable_memory_saver") is not True
            or values.get("enable_weights_cpu_backup") is not False
            or values.get("enable_draft_weights_cpu_backup") is not False):
        raise SaverBindingError("configuration_mismatch")
    module_name = "sglang.srt.utils.torch_memory_saver_adapter"
    adapter = _exact(_fields(scheduler).get("memory_saver_adapter"), module_name,
                     "_TorchMemorySaverAdapterReal")
    module = vars(sys.modules[module_name])
    if module.get("import_error", True) is not None:
        raise SaverBindingError("invalid_chain")
    saver = _exact(module.get("_memory_saver"), "torch_memory_saver.entrypoint", "TorchMemorySaver")
    package = sys.modules.get("torch_memory_saver")
    if package is None or vars(package).get("torch_memory_saver") is not saver:
        raise SaverBindingError("invalid_chain")
    impl = _fields(saver).get("_impl")
    if impl is None:
        raise SaverBindingError("uninitialized")
    _exact(impl, "torch_memory_saver.entrypoint", "_TorchMemorySaverImpl")
    if (_fields(impl).get("_hook_mode") != build.hook_mode
            or _fields(impl).get("_primary_mem_pool") is None):
        raise SaverBindingError("configuration_mismatch")
    hook = _exact(_fields(impl).get("_hook_util"), "torch_memory_saver.hooks.mode_preload",
                  "HookUtilModePreload")
    pool = _exact(_fields(impl).get("_primary_mem_pool"), "torch.cuda.memory", "MemPool")
    wrapper = _exact(_fields(impl).get("_binary_wrapper"), "torch_memory_saver.binary_wrapper", "BinaryWrapper")
    cdll = _fields(wrapper).get("cdll")
    if type(cdll) is not _CDLL_TYPE or cdll._name != build.path:
        raise SaverBindingError("library_mismatch")
    return args, adapter, saver, impl, hook, pool, wrapper, cdll


def _protected(info):
    if info.st_uid not in (0, os.getuid()) or info.st_mode & 0o022:
        raise SaverBindingError("unsafe_library")


def _library(build, cdll):
    """Correlate existing exports with the protected hashed backing-file inode."""
    try:
        _check_root(build.path)
        parent, _, leaf = build.path.rpartition("/")
        with ExitStack() as stack:
            chain = _open_chain(parent or "/", stack)
            for directory in chain:
                _protected(os.fstat(directory))
            fd = os.open(leaf, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC,
                         dir_fd=chain[-1])
            stack.callback(os.close, fd)
            info = os.fstat(fd)
            _protected(info)
            if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or not 0 < info.st_size <= 64 * 1024 * 1024:
                raise SaverBindingError("unsafe_library")
            digest = hashlib.sha256()
            remaining = info.st_size
            while remaining:
                chunk = os.read(fd, min(1024 * 1024, remaining))
                if not chunk:
                    raise SaverBindingError("changed")
                digest.update(chunk)
                remaining -= len(chunk)
            if digest.hexdigest() != build.sha256:
                raise SaverBindingError("library_mismatch")
            result = LoadedSaverLibrary(build.sha256, info.st_dev, info.st_ino,
                                        info.st_size, info.st_mtime_ns, info.st_ctime_ns)
            mappings = _read("/proc/self/maps", 4 * 1024 * 1024).splitlines()
            for name in _EXPORTS:
                export = getattr(cdll, name)
                address = ctypes.cast(export, ctypes.c_void_p).value
                matches = []
                for line in mappings:
                    fields = line.split(maxsplit=5)
                    low, high = (int(part, 16) for part in fields[0].split("-"))
                    if address is not None and low <= address < high:
                        matches.append(fields)
                if len(matches) != 1:
                    raise SaverBindingError("library_mismatch")
                mapping = matches[0]
                major, minor = (int(part, 16) for part in mapping[3].split(":"))
                if (mapping[1] != "r-xp" or int(mapping[4]) != info.st_ino
                        or os.makedev(major, minor) != info.st_dev
                        or len(mapping) != 6 or mapping[5].endswith(" (deleted)")):
                    raise SaverBindingError("library_mismatch")
            after = os.fstat(fd)
            visible = os.stat(leaf, dir_fd=chain[-1], follow_symlinks=False)
            for current in (after, visible):
                if (current.st_dev, current.st_ino, current.st_size, current.st_mtime_ns,
                    current.st_ctime_ns) != (info.st_dev, info.st_ino, info.st_size,
                                            info.st_mtime_ns, info.st_ctime_ns):
                    raise SaverBindingError("changed")
                _protected(current)
            return result
    except SaverBindingError:
        raise
    except Exception:
        raise SaverBindingError("library_mismatch") from None


def observe_scheduler_saver(scheduler, *, expected_owner, build):
    """Make one snapshot call; preserve unknown residency and downstream authority.

    Recheck the object chain, process, backing file and export mappings afterwards.
    The caller must already have enrolled this exact process and verified immutable
    SGLang and saver Python sources. These are point-in-time checks, not a lock on
    arbitrary Python mutation. The recipe's backup flags and snapshot records are
    checked, but no future allocation policy or complete process footprint is proved.
    """
    try:
        if (type(build) is not TrustedSaverBuild or type(build.path) is not str
                or type(build.sha256) is not str or re.fullmatch("[0-9a-f]{64}", build.sha256) is None
                or build.hook_mode != "preload"):
            raise SaverBindingError("configuration_mismatch")
        owner = current_process_identity()
        if type(expected_owner) is not ProcessIdentity or expected_owner != owner:
            raise SaverBindingError("owner_mismatch")
        chain = _chain(scheduler, build)
        library = _library(build, chain[-1])
        allocations = observe_saver(chain[-1], require_no_backup=True)
        current = _chain(scheduler, build)
        if (any(left is not right for left, right in zip(chain, current))
                or current_process_identity() != owner or _library(build, current[-1]) != library):
            raise SaverBindingError("changed")
        return SchedulerSaverObservation(owner, library, build.hook_mode, allocations)
    except ObservationError as error:
        raise SaverBindingError(error.status) from None
    except SaverBindingError:
        raise
    except Exception:
        raise SaverBindingError("invalid") from None
