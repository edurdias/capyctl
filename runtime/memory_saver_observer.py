"""Bounded observation of an already-loaded, patched CUDA memory saver.

These aggregates describe the saver allocation map only. They do not authorize
dispatch, acknowledge parking/restoration, prove saver enablement, or establish
whole-process residency. This module never loads a library or imports an engine.
"""

import ctypes
from dataclasses import dataclass


_MAX_RECORDS = 4096
_MAX_U64 = (1 << 64) - 1
_STATUSES = {1: "busy", 2: "invalid", 3: "overflow", 4: "unsupported", 5: "internal"}


class _RecordV1(ctypes.Structure):
    _fields_ = [
        ("address", ctypes.c_uint64),
        ("size_bytes", ctypes.c_uint64),
        ("backup_bytes", ctypes.c_uint64),
        ("device", ctypes.c_int32),
        ("state", ctypes.c_uint32),
        ("backup_enabled", ctypes.c_uint32),
        ("tag_length", ctypes.c_uint32),
        ("tag", ctypes.c_ubyte * 64),
    ]


class ObservationError(RuntimeError):
    """A failed observation, with a fixed status and no allocation addresses."""

    def __init__(self, status):
        self.status = status
        super().__init__(f"Memory saver observation: {status}")


@dataclass(frozen=True)
class AllocationAggregate:
    device: int
    tag: str
    allocation_count: int
    active_count: int
    paused_count: int
    virtual_bytes: int
    mapped_bytes: int
    backup_bytes: int
    backup_enabled_count: int


@dataclass(frozen=True)
class SaverObservation:
    groups: tuple[AllocationAggregate, ...]
    allocation_count: int
    virtual_bytes: int
    mapped_bytes: int
    backup_bytes: int


def _check_layout():
    expected_offsets = {
        "address": 0, "size_bytes": 8, "backup_bytes": 16, "device": 24,
        "state": 28, "backup_enabled": 32, "tag_length": 36, "tag": 40,
    }
    if ctypes.sizeof(ctypes.c_void_p) != 8 or ctypes.sizeof(_RecordV1) != 104:
        raise ObservationError("unsupported")
    if any(getattr(_RecordV1, field, None) is None or
           getattr(_RecordV1, field).offset != offset
           for field, offset in expected_offsets.items()):
        raise ObservationError("unsupported")


def _sum_u64(values):
    total = sum(values)
    if total > _MAX_U64:
        raise ObservationError("overflow")
    return total


def observe_saver(cdll, *, require_no_backup=True):
    """Read tms_snapshot_v1 from this object only, once, without a retry.

    Pass the actual saver's existing ``_binary_wrapper.cdll``. Resolving or
    initializing that saver is the caller's responsibility. Unknown tags remain
    visible; the downstream recipe must decide which tags it supports.
    """
    _check_layout()
    try:
        export = cdll.tms_snapshot_v1
    except AttributeError:
        raise ObservationError("unsupported") from None
    buffer = (_RecordV1 * _MAX_RECORDS)()
    count = ctypes.c_uint32()
    try:
        export.argtypes = [ctypes.c_uint32, ctypes.c_uint32, ctypes.POINTER(_RecordV1),
                           ctypes.c_uint32, ctypes.POINTER(ctypes.c_uint32)]
        export.restype = ctypes.c_uint32
        status = export(1, 104, buffer, _MAX_RECORDS, ctypes.byref(count))
    except Exception:
        raise ObservationError("internal") from None
    if count.value > _MAX_RECORDS or (status != 0 and count.value != 0):
        raise ObservationError("invalid")
    if status != 0:
        raise ObservationError(_STATUSES.get(status, "invalid"))

    # Addresses stay in this temporary validation list, never in public results.
    ranges = {}
    grouped = {}
    backup_present = False
    for index in range(count.value):
        record = buffer[index]
        if (record.address == 0 or record.size_bytes == 0 or
                record.size_bytes > _MAX_U64 - record.address or
                record.device < 0 or record.state not in (1, 2) or
                record.backup_enabled not in (0, 1) or
                record.backup_bytes not in (0, record.size_bytes) or
                (not record.backup_enabled and record.backup_bytes != 0) or
                (record.backup_enabled and record.state == 2 and record.backup_bytes == 0) or
                record.tag_length > 63):
            raise ObservationError("invalid")
        tag_bytes = bytes(record.tag)
        if b"\0" in tag_bytes[:record.tag_length] or any(tag_bytes[record.tag_length:]):
            raise ObservationError("invalid")
        try:
            tag = tag_bytes[:record.tag_length].decode("utf-8", errors="strict")
        except UnicodeDecodeError:
            raise ObservationError("invalid") from None
        ranges.setdefault(record.device, []).append((record.address, record.address + record.size_bytes))
        backup_present |= bool(record.backup_enabled or record.backup_bytes)
        # Seven counters correspond to the aggregate fields after device/tag.
        counters = grouped.setdefault((record.device, tag), [0] * 7)
        values = (1, int(record.state == 1), int(record.state == 2), record.size_bytes,
                  record.size_bytes if record.state == 1 else 0, record.backup_bytes,
                  record.backup_enabled)
        for field, value in enumerate(values):
            counters[field] = _sum_u64((counters[field], value))

    for device_ranges in ranges.values():
        device_ranges.sort()
        previous_end = 0
        for start, end in device_ranges:
            if start < previous_end:
                raise ObservationError("invalid")
            previous_end = end
    groups = tuple(AllocationAggregate(device, tag, *values)
                   for (device, tag), values in sorted(grouped.items()))
    virtual_bytes = _sum_u64(group.virtual_bytes for group in groups)
    mapped_bytes = _sum_u64(group.mapped_bytes for group in groups)
    backup_bytes = _sum_u64(group.backup_bytes for group in groups)
    if require_no_backup and backup_present:
        raise ObservationError("backup_disallowed")
    return SaverObservation(groups, count.value, virtual_bytes, mapped_bytes, backup_bytes)
