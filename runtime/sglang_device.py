"""Import-free Linux physical inventory and closed single-GPU placement check.

This observes trusted host kernel mounts, not containers with substituted /proc
or /sys. It neither attests mount provenance nor grants launch authority. The
service must provision that trust, bind/freeze a TrustedDeviceMapping against
its policy revision, and guard imports/environment mutation through launch.
The inventory digest is versioned evidence, NOT a reinterpretation of the
existing opaque hardware_fingerprint. No model-name or ordinal inference occurs.

The only supported inherited CUDA namespace is one complete verified UUID.
Setting that environment before any CUDA import is an external guarded-service
obligation. This module never changes it, imports native libraries, or opens the
entrypoint. Reobserve immediately before use; results are not durable authority.
"""

from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import socket
import stat
import time

from .sglang_server_args import ObservedPlacement, _validated_public


class DeviceObservationError(Exception):
    def __init__(self):
        super().__init__("device_observation_denied")


@dataclass(frozen=True, repr=False)
class PhysicalDevice:
    physical_gpu_uuid: str
    pci_address: str
    device_minor: int
    vendor_id: str
    device_id: str


@dataclass(frozen=True, repr=False)
class DeviceInventory:
    host_id: str
    architecture: str
    boot_id: str
    devices: tuple
    digest: str
    observed_at_ns: int


@dataclass(frozen=True, repr=False)
class TrustedDeviceMapping:
    """Service-owned mapping; constructing this is not proof of trust.

    Never populate from a management request, candidate self-report, or unbound
    current inventory. Policy provisioning must independently authorize the
    physical UUID and exact inventory digest for this logical device.
    """

    host_id: str
    hardware_fingerprint: str
    device_id: str
    memory_domain: str
    physical_gpu_uuid: str
    inventory_digest: str


_UUID = re.compile(r"GPU-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")
_BDF = re.compile(r"[0-9a-f]{4}:[0-9a-f]{2}:[0-9a-f]{2}\.[0-7]")
_BOOT = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")


def _read(path):
    # Kernel pseudo-files often report st_size=0. Bound actual bytes, not stat.
    fd = os.open(path, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW | os.O_NONBLOCK)
    try:
        if not stat.S_ISREG(os.fstat(fd).st_mode):
            raise ValueError()
        with os.fdopen(fd, "rb", closefd=False) as stream:
            raw = stream.read(16385)
        if len(raw) > 16384:
            raise ValueError()
        return raw.decode("ascii").strip()
    finally:
        os.close(fd)


def _collect_inventory(proc, sysfs, host, architecture):
    """Private filesystem plumbing; public collector has no path override."""
    try:
        if (type(host) is not str or not re.fullmatch(r"[A-Za-z0-9_.-]{1,253}", host)
                or architecture not in ("aarch64", "x86_64")):
            raise ValueError()
        boot = _read(proc / "sys/kernel/random/boot_id")
        if not _BOOT.fullmatch(boot):
            raise ValueError()
        paths = []
        with os.scandir(proc / "driver/nvidia/gpus") as entries:
            for entry in entries:
                if (len(paths) >= 256 or not _BDF.fullmatch(entry.name)
                        or not entry.is_dir(follow_symlinks=False)):
                    raise ValueError()
                paths.append(entry.name)
        if not paths:
            raise ValueError()
        devices = []
        for bdf in sorted(paths):
            fields = {}
            for line in _read(proc / "driver/nvidia/gpus" / bdf / "information").splitlines():
                key, separator, value = line.partition(":")
                if not separator or key.strip() in fields:
                    raise ValueError()
                fields[key.strip()] = value.strip()
            uuid = fields["GPU UUID"]
            minor = fields["Device Minor"]
            if (not _UUID.fullmatch(uuid) or fields["Bus Location"] != bdf
                    or fields["GPU Excluded"] != "No"
                    or not re.fullmatch(r"0|[1-9][0-9]{0,4}", minor)):
                raise ValueError()
            # Linux sysfs PCI entries are kernel-owned directory symlinks; leaf
            # attributes must be regular, non-symlink files under trusted mounts.
            pci = sysfs / "bus/pci/devices" / bdf
            vendor = _read(pci / "vendor")
            product = _read(pci / "device")
            if vendor != "0x10de" or not re.fullmatch(r"0x[0-9a-f]{4}", product):
                raise ValueError()
            devices.append(PhysicalDevice(uuid, bdf, int(minor), vendor, product))
        if (len({d.physical_gpu_uuid for d in devices}) != len(devices)
                or len({d.device_minor for d in devices}) != len(devices)):
            raise ValueError()
        if _read(proc / "sys/kernel/random/boot_id") != boot:
            raise ValueError()
        material = {"schema": "mllm-nvidia-inventory-v1", "host": host,
                    "architecture": architecture, "boot_id": boot,
                    "devices": [[d.physical_gpu_uuid, d.pci_address, d.device_minor,
                                 d.vendor_id, d.device_id] for d in devices]}
        digest = hashlib.sha256(json.dumps(material, sort_keys=True,
                                          separators=(",", ":")).encode()).hexdigest()
        return DeviceInventory(host, architecture, boot, tuple(devices), digest,
                               time.monotonic_ns())
    except Exception:
        raise DeviceObservationError() from None


def collect_inventory():
    """Fresh local inventory only; no request-selected roots or subprocesses."""
    try:
        if platform.system() != "Linux":
            raise ValueError()
        return _collect_inventory(Path("/proc"), Path("/sys"), socket.gethostname(),
                                  platform.machine())
    except Exception:
        raise DeviceObservationError() from None


def observe_placement(spec, trusted_mapping):
    """Corroborate frozen logical selection with freshly collected inventory.

    The mapping must originate in trusted service policy, not the candidate.
    No caller-supplied inventory or timestamp is accepted as fresh evidence.
    Collection is repeated to detect concurrent inventory/namespace change;
    preventing later mutation remains the guarded launcher's obligation.
    """
    try:
        public = _validated_public(spec)
        if type(trusted_mapping) is not TrustedDeviceMapping:
            raise ValueError()
        for key, value in public["device"].items():
            if getattr(trusted_mapping, key) != value:
                raise ValueError()
        uuid = trusted_mapping.physical_gpu_uuid
        if type(uuid) is not str or not _UUID.fullmatch(uuid):
            raise ValueError()
        before = collect_inventory()
        if (before.host_id != trusted_mapping.host_id
                or before.digest != trusted_mapping.inventory_digest
                or uuid not in {d.physical_gpu_uuid for d in before.devices}
                or os.environ.get("CUDA_VISIBLE_DEVICES") != uuid):
            raise ValueError()
        after = collect_inventory()
        if (after.digest != before.digest
                or os.environ.get("CUDA_VISIBLE_DEVICES") != uuid):
            raise ValueError()
        return ObservedPlacement(
            binding_id=public["binding_id"], incarnation=public["incarnation"],
            **public["device"], physical_gpu_uuid=uuid,
            cuda_visible_uuids=(uuid,), cuda_index=0)
    except Exception:
        raise DeviceObservationError() from None
