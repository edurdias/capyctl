"""CPU-only tests against the pinned, patched saver source. Never downloads."""

import ctypes
import hashlib
import importlib
import importlib.util
import io
import os
from pathlib import Path, PurePosixPath
import subprocess
import struct
import tarfile
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
STUB = ROOT / "runtime/tests/memory_saver_stub"
PATCH = ROOT / "runtime/patches/torch-memory-saver-0.0.9.post1-observer.patch"
SOURCE_SHA256 = "25fd4b691ed3242c3a18b2bef0dbe9de84d2e7068b96a37686a923d55c274f43"


def compile_fixture(command):
    result = subprocess.run(command, capture_output=True, text=True)
    if result.returncode or result.stderr:
        raise AssertionError(result.stdout + result.stderr)


def extract_source(archive, destination):
    if not archive or not Path(archive).is_file():
        raise AssertionError("Prerequisite: set TMS_SOURCE_ARCHIVE to the pinned source archive")
    with open(archive, "rb") as stream:
        digest = hashlib.sha256()
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    if digest.hexdigest() != SOURCE_SHA256:
        raise AssertionError("Source archive SHA-256 mismatch")
    with tarfile.open(archive, "r:gz") as source:
        for member in source.getmembers():
            path = PurePosixPath(member.name)
            if (path.is_absolute() or ".." in path.parts or
                    not (member.isfile() or member.isdir()) or
                    not path.parts or path.parts[0] != "torch_memory_saver-0.0.9.post1"):
                raise AssertionError("Unsafe source archive member")
        source.extractall(destination)
    return destination / "torch_memory_saver-0.0.9.post1"


class NativeTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temporary = tempfile.TemporaryDirectory(prefix="mllm-tms-observer-")
        cls.addClassCleanup(cls.temporary.cleanup)
        cls.build = Path(cls.temporary.name)
        source = extract_source(os.environ.get("TMS_SOURCE_ARCHIVE"), cls.build)
        if not PATCH.is_file():
            raise AssertionError("Missing observer patch/export")
        result = subprocess.run(["patch", "--batch", "--fuzz=0", "-p1", "-i", str(PATCH)],
                                cwd=source, capture_output=True, text=True)
        if result.returncode:
            raise AssertionError(result.stdout + result.stderr)
        common = ["g++", "-std=c++17", "-pthread", "-DUSE_CUDA", "-Wall", "-Wextra",
                  "-Wno-unused-function", "-Wno-unused-parameter", "-Werror",
                  "-I", str(STUB), "-I", str(source / "csrc"),
                  str(source / "csrc/core.cpp"), str(source / "csrc/entrypoint.cpp"),
                  str(STUB / "driver.cpp")]
        cls.binary = cls.build / "scenarios"
        compile_fixture(common + [str(STUB / "scenarios.cpp"), "-o", str(cls.binary)])
        cls.libraries = []
        for mode in ("TORCH", "PRELOAD"):
            library = cls.build / f"saver_{mode.lower()}.so"
            compile_fixture(common + [f"-DTMS_HOOK_MODE_{mode}", "-shared", "-fPIC",
                                      "-Wl,-Bsymbolic", "-o", str(library)])
            cls.libraries.append(library)

    def run_scenario(self, name):
        result = subprocess.run([str(self.binary), name], capture_output=True, text=True, timeout=15)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_export_contract(self):
        self.run_scenario("abi")

    def test_actual_records_and_complete_bounds(self):
        self.run_scenario("records")

    def test_maximum_capacity_is_complete(self):
        self.run_scenario("maximum_capacity")

    def test_counter_overflow_and_fallback_error_latch_invalid(self):
        for scenario in ("counter_overflow", "fallback_error"):
            with self.subTest(scenario=scenario):
                self.run_scenario(scenario)

    def test_impossible_native_metadata_is_rejected(self):
        self.run_scenario("metadata_corruption")

    def test_duplicate_insertion_invalidates_observation(self):
        self.run_scenario("duplicate_insertion")

    def test_native_invalid_metadata(self):
        for scenario in ("invalid_tag_length", "invalid_tag_nul", "invalid_device", "invalid_size"):
            with self.subTest(scenario=scenario):
                self.run_scenario(scenario)

    def test_untracked_mapping_window_is_busy(self):
        self.run_scenario("malloc_window")

    def test_erased_free_window_is_busy(self):
        self.run_scenario("free_window")

    def test_overlapping_malloc_free(self):
        self.run_scenario("overlap")

    def test_pause_resume_contention_and_fallback_cleanup(self):
        for scenario in ("pause_contention", "resume_contention", "fallback"):
            with self.subTest(scenario=scenario):
                self.run_scenario(scenario)

    def test_exceptions_latch_invalid_observation(self):
        for action in ("malloc", "free", "pause", "resume", "fallback"):
            with self.subTest(action=action):
                self.run_scenario("exception_" + action)

    def test_checked_driver_errors_terminate_without_observation(self):
        for action in ("malloc", "free", "pause", "resume"):
            with self.subTest(action=action):
                result = subprocess.run([str(self.binary), "fatal_" + action],
                                        capture_output=True, text=True, timeout=15)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("CUresult error", result.stderr)
                self.assertEqual(result.stdout, "")

    def test_reader_uses_only_selected_compiled_library(self):
        reader = importlib.import_module("runtime.memory_saver_observer")
        libraries = [ctypes.CDLL(str(path)) for path in self.libraries]
        pointers = []
        for library, size, device, tag in zip(libraries, (4096, 8192), (0, 1), (b"weights", b"kv")):
            library.stub_allocate.argtypes = [ctypes.c_uint64, ctypes.c_int, ctypes.c_char_p,
                                             ctypes.c_bool, ctypes.POINTER(ctypes.c_void_p)]
            library.stub_free.argtypes = [ctypes.c_void_p]
            pointer = ctypes.c_void_p()
            self.assertEqual(library.stub_allocate(size, device, tag, False, ctypes.byref(pointer)), 0)
            pointers.append(pointer)
        try:
            with mock.patch.object(ctypes, "CDLL", side_effect=AssertionError("must use supplied library")):
                first = reader.observe_saver(libraries[0])
                second = reader.observe_saver(libraries[1])
            self.assertEqual((first.mapped_bytes, second.mapped_bytes), (4096, 8192))
            self.assertEqual([(group.device, group.tag) for group in first.groups], [(0, "weights")])
            self.assertEqual([(group.device, group.tag) for group in second.groups], [(1, "kv")])
        finally:
            for library, pointer in zip(libraries, pointers):
                self.assertEqual(library.stub_free(pointer), 0)


def record(*, address=0x100000, size=4096, backup=0, device=0, state=1,
           enabled=0, tag=b"weights", length=None, padding=b""):
    length = len(tag) if length is None else length
    return struct.pack("=QQQiIII64s", address, size, backup, device, state, enabled,
                       length, tag + padding)


class SuppliedExport:
    """Only ABI transport is doubled; validation and aggregation remain real."""
    def __init__(self, rows=(), status=0, count=None, exception=False):
        self.rows, self.status, self.count = rows, status, count
        self.exception = exception
        self.calls = []

    def __call__(self, version, size, buffer, capacity, count):
        self.calls.append((version, size, capacity))
        if self.exception:
            raise RuntimeError("transport error containing private address 0x100000")
        if self.rows:
            payload = b"".join(self.rows)
            ctypes.memmove(buffer, payload, len(payload))
        ctypes.cast(count, ctypes.POINTER(ctypes.c_uint32))[0] = (
            len(self.rows) if self.count is None else self.count)
        return self.status


class SuppliedLibrary:
    def __init__(self, *args, **kwargs):
        self.tms_snapshot_v1 = SuppliedExport(*args, **kwargs)


class ReaderTests(unittest.TestCase):
    def reader(self):
        self.assertIsNotNone(importlib.util.find_spec("runtime.memory_saver_observer"),
                             "Missing same-library reader")
        return importlib.import_module("runtime.memory_saver_observer")

    def test_complete_multi_device_aggregation_is_immutable_and_private(self):
        reader = self.reader()
        library = SuppliedLibrary([
            record(address=0x100000, size=4096),
            record(address=0x200000, size=2048, state=2),
            record(address=0x100000, size=8192, device=1, tag=b"kv"),
            record(address=0x400000, size=512, tag="unknown-λ".encode()),
        ])
        with mock.patch.object(ctypes, "CDLL", side_effect=AssertionError("second library loaded")):
            result = reader.observe_saver(library)
        self.assertEqual(library.tms_snapshot_v1.calls, [(1, 104, 4096)])
        self.assertEqual((result.allocation_count, result.virtual_bytes, result.mapped_bytes,
                          result.backup_bytes), (4, 14848, 12800, 0))
        self.assertEqual([(g.device, g.tag, g.allocation_count, g.active_count, g.paused_count,
                           g.virtual_bytes, g.mapped_bytes) for g in result.groups],
                         [(0, "unknown-λ", 1, 1, 0, 512, 512),
                          (0, "weights", 2, 1, 1, 6144, 4096),
                          (1, "kv", 1, 1, 0, 8192, 8192)])
        with self.assertRaises(AttributeError):
            result.mapped_bytes = 0
        with self.assertRaises(AttributeError):
            result.groups[0].tag = "changed"
        for rendered in (repr(result), repr(result.groups)):
            self.assertNotIn("address", rendered)
            self.assertNotIn("0x100000", rendered)
            self.assertNotIn("1048576", rendered)

    def test_empty_observation(self):
        result = self.reader().observe_saver(SuppliedLibrary())
        self.assertEqual((result.groups, result.allocation_count, result.virtual_bytes,
                          result.mapped_bytes, result.backup_bytes), ((), 0, 0, 0, 0))

    def test_maximum_records_and_tag_boundaries(self):
        rows = [record(address=4096 + index * 2, size=1, tag=b"" if index == 0 else b"x" * 63)
                for index in range(4096)]
        result = self.reader().observe_saver(SuppliedLibrary(rows))
        self.assertEqual((result.allocation_count, result.mapped_bytes), (4096, 4096))
        self.assertEqual([(group.tag, group.allocation_count) for group in result.groups],
                         [("", 1), ("x" * 63, 4095)])

    def test_absent_symbol_and_wrong_layout(self):
        reader = self.reader()
        with self.assertRaises(reader.ObservationError) as raised:
            reader.observe_saver(object())
        self.assertEqual(raised.exception.status, "unsupported")
        class WrongLayout(ctypes.Structure):
            _fields_ = [("wrong", ctypes.c_uint8)]
        library = SuppliedLibrary()
        with mock.patch.object(reader, "_RecordV1", WrongLayout):
            with self.assertRaises(reader.ObservationError) as raised:
                reader.observe_saver(library)
        self.assertEqual(raised.exception.status, "unsupported")
        self.assertEqual(library.tms_snapshot_v1.calls, [])

    def test_statuses_and_invalid_counts_have_no_retry(self):
        reader = self.reader()
        for status, name in [(1, "busy"), (2, "invalid"), (3, "overflow"),
                             (4, "unsupported"), (5, "internal"), (99, "invalid")]:
            library = SuppliedLibrary(status=status)
            with self.subTest(status=status), self.assertRaises(reader.ObservationError) as raised:
                reader.observe_saver(library)
            self.assertEqual(raised.exception.status, name)
            self.assertEqual(len(library.tms_snapshot_v1.calls), 1)
        for status, count in ((0, 4097), (1, 1)):
            with self.subTest(status=status, count=count), self.assertRaises(reader.ObservationError):
                reader.observe_saver(SuppliedLibrary(status=status, count=count))

    def test_malformed_records_are_rejected_without_addresses(self):
        reader = self.reader()
        malformed = [
            record(address=0), record(size=0), record(address=2**64 - 1, size=2),
            record(device=-1), record(state=0), record(state=3), record(enabled=2),
            record(backup=1), record(enabled=0, backup=4096),
            record(state=2, enabled=1, backup=0), record(length=64),
            record(tag=b"a\0b"), record(tag=b"\xff"), record(padding=b"nonzero"),
        ]
        for index, row in enumerate(malformed):
            with self.subTest(index=index), self.assertRaises(reader.ObservationError) as raised:
                reader.observe_saver(SuppliedLibrary([record(address=0x500000), row]), require_no_backup=False)
            self.assertEqual(raised.exception.status, "invalid")
            self.assertNotIn("0x100000", str(raised.exception))
            self.assertNotIn("1048576", repr(raised.exception))

    def test_duplicate_and_overlapping_ranges(self):
        reader = self.reader()
        for other in (0x100000, 0x100001, 0x100FFF):
            with self.subTest(other=other), self.assertRaises(reader.ObservationError):
                reader.observe_saver(SuppliedLibrary([record(), record(address=other, state=2)]))
        result = reader.observe_saver(SuppliedLibrary([record(), record(address=0x101000)]))
        self.assertEqual(result.mapped_bytes, 8192)

    def test_aggregate_overflow_across_devices(self):
        reader = self.reader()
        with self.assertRaises(reader.ObservationError) as raised:
            reader.observe_saver(SuppliedLibrary([
                record(address=1, size=2**63, device=0),
                record(address=1, size=2**63, device=1),
            ]))
        self.assertEqual(raised.exception.status, "overflow")

    def test_backup_policy_and_honest_generic_aggregation(self):
        reader = self.reader()
        for state, backup in ((1, 0), (1, 4096), (2, 4096)):
            library = SuppliedLibrary([record(enabled=1, state=state, backup=backup)])
            with self.subTest(state=state, backup=backup), self.assertRaises(reader.ObservationError) as raised:
                reader.observe_saver(library)
            self.assertEqual(raised.exception.status, "backup_disallowed")
            result = reader.observe_saver(library, require_no_backup=False)
            self.assertEqual(result.backup_bytes, backup)
            self.assertEqual(result.groups[0].backup_enabled_count, 1)

    def test_transport_errors_are_sanitized(self):
        reader = self.reader()
        library = SuppliedLibrary(exception=True)
        with self.assertRaises(reader.ObservationError) as raised:
            reader.observe_saver(library)
        self.assertEqual(raised.exception.status, "internal")
        self.assertNotIn("0x100000", repr(raised.exception))
        self.assertTrue(raised.exception.__suppress_context__)


class SourcePrerequisiteTests(unittest.TestCase):
    def test_missing_archive_is_a_failure(self):
        with tempfile.TemporaryDirectory(prefix="mllm-tms-prerequisite-") as directory:
            with self.assertRaisesRegex(AssertionError, "Prerequisite"):
                extract_source(None, Path(directory))

    def test_wrong_hash_rejected_before_extraction(self):
        with tempfile.TemporaryDirectory(prefix="mllm-tms-prerequisite-") as directory:
            archive = Path(directory) / "not-source.tar.gz"
            archive.write_bytes(b"not the pinned source")
            destination = Path(directory) / "uncreated"
            with self.assertRaisesRegex(AssertionError, "SHA-256 mismatch"):
                extract_source(archive, destination)
            self.assertFalse(destination.exists())

    def test_unsafe_members_rejected_before_extraction(self):
        for name, kind in (("../outside", tarfile.REGTYPE), ("/absolute", tarfile.REGTYPE),
                           ("torch_memory_saver-0.0.9.post1/link", tarfile.SYMTYPE),
                           ("torch_memory_saver-0.0.9.post1/hard", tarfile.LNKTYPE),
                           ("torch_memory_saver-0.0.9.post1/fifo", tarfile.FIFOTYPE)):
            with self.subTest(name=name), tempfile.TemporaryDirectory(prefix="mllm-tms-prerequisite-") as directory:
                stream = io.BytesIO()
                with tarfile.open(fileobj=stream, mode="w:gz") as archive:
                    member = tarfile.TarInfo(name)
                    member.type = kind
                    member.linkname = "../../outside" if kind in (tarfile.SYMTYPE, tarfile.LNKTYPE) else ""
                    archive.addfile(member)
                content = stream.getvalue()
                archive_path = Path(directory) / "unsafe.tar.gz"
                archive_path.write_bytes(content)
                destination = Path(directory) / "uncreated"
                with mock.patch.dict(extract_source.__globals__, SOURCE_SHA256=hashlib.sha256(content).hexdigest()):
                    with self.assertRaisesRegex(AssertionError, "Unsafe source archive member"):
                        extract_source(archive_path, destination)
                self.assertFalse(destination.exists())


if __name__ == "__main__":
    unittest.main()
