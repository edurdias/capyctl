"""SGLang 0.5.20 + torch-memory-saver 0.0.10 saver residency, against fakes.

Every engine object here is a stand-in with the real shapes read on host-a
(2026-09-23, read-only): a msgspec-style ServerArgs, the saver's per-tag
MemPools and a fake CUDA driver whose mappings the test flips as a release and
a resume would. These are CPU fixtures, never qualification (AGENTS.md).
"""
import ctypes
import sys
import types
import unittest
from unittest import mock

from runtime import sglang_saver_binding as saver
from runtime import sglang_saver_residency as residency
from runtime.sglang_scheduler_observer import BridgeError


FIELDS = ("enable_memory_saver", "enable_weights_cpu_backup", "enable_draft_weights_cpu_backup",
          "admin_api_key", *residency._TOPOLOGY)


class ServerArgs:
    """msgspec Struct shape: fields in slots, a __struct_fields__ tuple."""
    __struct_fields__ = FIELDS
    __slots__ = FIELDS

    def __init__(self, **values):
        defaults = dict(residency._TOPOLOGY, enable_memory_saver=True,
                        enable_weights_cpu_backup=False, enable_draft_weights_cpu_backup=False,
                        admin_api_key="a" * 64)
        defaults.update(values)
        for name in FIELDS:
            object.__setattr__(self, name, defaults[name])


class MemPool:
    def __init__(self, segments):
        self.segments = segments

    def snapshot(self, include_traces=True):
        assert include_traces is False
        return [dict(segment) for segment in self.segments]


class Driver:
    """Fake cuMemRetainAllocationHandle: an address is mapped or it is not."""

    def __init__(self):
        self.unmapped = set()
        self.calls = 0

    def mapped(self, address):
        self.calls += 1
        return address not in self.unmapped


def segment(address, size, device=0):
    return dict(address=address, total_size=size, device=device, segment_type="large")


class Fakes:
    def __init__(self, test, pools=None, args=None):
        modules = {}

        def module(name, **values):
            loaded = types.ModuleType(name)
            vars(loaded).update(values)
            modules[name] = loaded
            return loaded

        Scheduler = type("Scheduler", (), {"process_input_requests": lambda self, reqs: reqs})
        Real = type("_TorchMemorySaverAdapterReal", (), {})
        TorchMemorySaver = type("TorchMemorySaver", (), {})
        Impl = type("_TorchMemorySaverImpl", (), {})
        Hook = type("HookUtilModePreload", (), {})
        Wrapper = type("BinaryWrapper", (), {})
        module("sglang.srt.managers.scheduler", Scheduler=Scheduler)
        module("sglang.srt.server_args", ServerArgs=ServerArgs)
        instance = TorchMemorySaver()
        module("sglang.srt.utils.torch_memory_saver_adapter", _TorchMemorySaverAdapterReal=Real,
               import_error=None, _memory_saver=instance)
        module("torch_memory_saver", torch_memory_saver=instance)
        module("torch_memory_saver.entrypoint", TorchMemorySaver=TorchMemorySaver,
               _TorchMemorySaverImpl=Impl)
        module("torch_memory_saver.hooks.mode_preload", HookUtilModePreload=Hook)
        module("torch_memory_saver.binary_wrapper", BinaryWrapper=Wrapper)
        module("torch.cuda.memory", MemPool=MemPool)
        test.addCleanup(mock.patch.stopall)
        mock.patch.dict(sys.modules, modules).start()
        self.build = saver.TrustedSaverBuild("/private/torch_memory_saver_hook_mode_preload.so",
                                             "b" * 64, "preload")
        cdll = ctypes.CDLL(None)
        cdll._name = self.build.path
        wrapper = Wrapper()
        wrapper.cdll = cdll
        self.impl = Impl()
        self.impl._hook_mode = "preload"
        self.impl._hook_util = Hook()
        self.impl._binary_wrapper = wrapper
        self.impl._mem_pools = pools if pools is not None else {
            ("weights", False, False, "", 0): MemPool([segment(0x1000_0000, 4096),
                                                       segment(0x2000_0000, 8192)]),
            ("kv_cache", False, False, "", 0): MemPool([segment(0x3000_0000, 16384)]),
        }
        instance._impl = self.impl
        self.scheduler = Scheduler()
        self.scheduler.server_args = args if args is not None else ServerArgs()
        self.scheduler.memory_saver_adapter = Real()
        self.owner = saver.current_process_identity()
        self.library = saver.LoadedSaverLibrary("b" * 64, 1, 2, 3, 4, 5)
        mock.patch.object(saver, "_library", return_value=self.library).start()
        self.driver = Driver()

    def observe(self, weight_restore="disk_reload"):
        return residency.observe_scheduler_saver(self.scheduler, expected_owner=self.owner,
                                                 build=self.build, driver=self.driver,
                                                 weight_restore=weight_restore)


def host_backed_pools(weights_backup=True, kv_backup=False):
    """The pools a host_backed launch allocates: weights in a CPU-backup region."""
    return {
        ("weights", weights_backup, False, "pinned" if weights_backup else "", 0):
            MemPool([segment(0x1000_0000, 4096)]),
        ("kv_cache", kv_backup, False, "pinned" if kv_backup else "", 0):
            MemPool([segment(0x3000_0000, 16384)]),
    }


class HostBackedResidencyTests(unittest.TestCase):
    # T22 / ADR 0019: the weights region's CPU backup is accepted only for a
    # launch that declared `cpu_backup`; the kv_cache region never has one.
    def test_backup_accepted_for_host_backed(self):
        fakes = Fakes(self, pools=host_backed_pools(),
                      args=ServerArgs(enable_weights_cpu_backup=True))
        groups = {group.tag: group for group in fakes.observe("cpu_backup").allocations.groups}
        self.assertEqual(groups["weights"].mapped_bytes, 4096)
        self.assertEqual(groups["kv_cache"].mapped_bytes, 16384)

    def test_backup_refused_for_deep(self):
        fakes = Fakes(self, pools=host_backed_pools(),
                      args=ServerArgs(enable_weights_cpu_backup=True))
        with self.assertRaises(saver.SaverBindingError) as caught:
            fakes.observe("disk_reload")
        self.assertEqual(caught.exception.code, "configuration_mismatch")
        fakes = Fakes(self, pools=host_backed_pools())
        with self.assertRaises(saver.SaverBindingError) as caught:
            fakes.observe("disk_reload")
        self.assertEqual(caught.exception.code, "unsupported")

    def test_missing_backup_refused_for_host_backed(self):
        fakes = Fakes(self, args=ServerArgs())
        with self.assertRaises(saver.SaverBindingError) as caught:
            fakes.observe("cpu_backup")
        self.assertEqual(caught.exception.code, "configuration_mismatch")
        fakes = Fakes(self, pools=host_backed_pools(weights_backup=False),
                      args=ServerArgs(enable_weights_cpu_backup=True))
        with self.assertRaises(saver.SaverBindingError) as caught:
            fakes.observe("cpu_backup")
        self.assertEqual(caught.exception.code, "unsupported")

    def test_draft_and_kv_cache_backup_always_refused(self):
        fakes = Fakes(self, pools=host_backed_pools(),
                      args=ServerArgs(enable_weights_cpu_backup=True,
                                      enable_draft_weights_cpu_backup=True))
        with self.assertRaises(saver.SaverBindingError) as caught:
            fakes.observe("cpu_backup")
        self.assertEqual(caught.exception.code, "configuration_mismatch")
        fakes = Fakes(self, pools=host_backed_pools(kv_backup=True),
                      args=ServerArgs(enable_weights_cpu_backup=True))
        with self.assertRaises(saver.SaverBindingError) as caught:
            fakes.observe("cpu_backup")
        self.assertEqual(caught.exception.code, "unsupported")

    def test_unknown_backup_backend_refused(self):
        pools = {("weights", True, False, "disk", 0): MemPool([segment(0x1000_0000, 4096)])}
        fakes = Fakes(self, pools=pools, args=ServerArgs(enable_weights_cpu_backup=True))
        with self.assertRaises(saver.SaverBindingError) as caught:
            fakes.observe("cpu_backup")
        self.assertEqual(caught.exception.code, "unsupported")


class SaverResidencyTests(unittest.TestCase):
    # T22 T16: a resident launch maps every saver segment; a release that the
    # driver confirms leaves them all paused; a resume maps them again.
    def test_mapped_bytes_follow_the_driver_not_the_release_route(self):
        fakes = Fakes(self)
        resident = fakes.observe()
        self.assertEqual(resident.hook_mode, "preload")
        self.assertEqual(resident.library, fakes.library)
        groups = {group.tag: group for group in resident.allocations.groups}
        self.assertEqual((groups["weights"].mapped_bytes, groups["weights"].virtual_bytes), (12288, 12288))
        self.assertEqual((groups["kv_cache"].active_count, groups["kv_cache"].paused_count), (1, 0))
        self.assertEqual(resident.allocations.mapped_bytes, 28672)
        fakes.driver.unmapped |= {0x1000_0000, 0x2000_0000, 0x3000_0000}
        released = fakes.observe().allocations
        self.assertEqual(released.mapped_bytes, 0)
        self.assertEqual(released.virtual_bytes, 28672)
        self.assertTrue(all(group.paused_count == group.allocation_count for group in released.groups))
        fakes.driver.unmapped.clear()
        self.assertEqual(fakes.observe().allocations.mapped_bytes, 28672)

    # T20: a partial release (weights unmapped, cache still mapped) is reported
    # as exactly that, for the host to treat as partial evidence.
    def test_partial_release_is_visible_per_tag(self):
        fakes = Fakes(self)
        fakes.driver.unmapped |= {0x1000_0000, 0x2000_0000}
        groups = {g.tag: g for g in fakes.observe().allocations.groups}
        self.assertEqual(groups["weights"].mapped_bytes, 0)
        self.assertEqual(groups["kv_cache"].mapped_bytes, 16384)

    # T22: backups, unknown tags, overlapping or foreign segments and a wrong
    # recipe fail closed with a fixed category, never a guess.
    def test_unsupported_pool_shapes_fail_closed(self):
        for key, code in ((("weights", True, False, "pinned", 0), "unsupported"),
                          (("weights", False, True, "", 0), "unsupported"),
                          (("cuda_graph", False, False, "", 0), "unsupported"),
                          (("weights", False, False, ""), "invalid")):
            with self.subTest(key=key):
                fakes = Fakes(self, pools={key: MemPool([segment(0x1000, 4096)])})
                with self.assertRaises(saver.SaverBindingError) as caught:
                    fakes.observe()
                self.assertEqual(caught.exception.code, code)
        overlapping = {("weights", False, False, "", 0): MemPool([segment(0x1000, 8192),
                                                                  segment(0x2000, 4096)])}
        with self.assertRaises(saver.SaverBindingError):
            Fakes(self, pools=overlapping).observe()
        foreign = {("weights", False, False, "", 0): MemPool([segment(0x1000, 4096, device=1)])}
        with self.assertRaises(saver.SaverBindingError):
            Fakes(self, pools=foreign).observe()
        for args in (ServerArgs(enable_memory_saver=False), ServerArgs(enable_weights_cpu_backup=True)):
            with self.assertRaises(saver.SaverBindingError) as caught:
                Fakes(self, args=args).observe()
            self.assertEqual(caught.exception.code, "configuration_mismatch")

    def test_driver_failure_and_changed_chain_are_closed(self):
        fakes = Fakes(self)

        def broken(address):
            raise residency.ObservationError("internal")
        fakes.driver.mapped = broken
        with self.assertRaises(saver.SaverBindingError) as caught:
            fakes.observe()
        self.assertEqual(caught.exception.code, "internal")
        fakes = Fakes(self)
        fakes.impl._hook_mode = "torch"
        with self.assertRaises(saver.SaverBindingError):
            fakes.observe()
        fakes = Fakes(self)
        with self.assertRaises(saver.SaverBindingError) as caught:
            residency.observe_scheduler_saver(fakes.scheduler, expected_owner=saver.ProcessIdentity(
                1, 1, fakes.owner.boot_id), build=fakes.build, driver=fakes.driver)
        self.assertEqual(caught.exception.code, "owner_mismatch")

    def test_segment_bound_overflows_closed(self):
        segments = [segment(0x1000 + index * 4096, 4096) for index in range(4097)]
        with self.assertRaises(saver.SaverBindingError) as caught:
            Fakes(self, pools={("weights", False, False, "", 0): MemPool(segments)}).observe()
        self.assertEqual(caught.exception.code, "overflow")

    def test_topology_reads_struct_fields_and_refuses_multi_rank(self):
        fakes = Fakes(self)
        residency.topology(fakes.scheduler)
        fakes.scheduler.server_args = ServerArgs(tp_size=2)
        with self.assertRaisesRegex(BridgeError, "topology"):
            residency.topology(fakes.scheduler)
        fakes.scheduler.server_args = types.SimpleNamespace()
        with self.assertRaisesRegex(BridgeError, "topology"):
            residency.topology(fakes.scheduler)

    def test_cuda_driver_maps_retain_statuses(self):
        class Call:
            def __init__(self, status, release=0):
                self.status, self.release_status, self.released = status, release, []

            def retain(self, handle, address):
                handle._obj.value = 7
                return self.status

            def release(self, handle):
                self.released.append(handle.value)
                return self.release_status

        for status, expected in ((0, True), (1, False)):
            call = Call(status)
            driver = object.__new__(residency.CudaDriver)
            driver._retain, driver._release = call.retain, call.release
            self.assertIs(driver.mapped(0x1000), expected)
            self.assertEqual(call.released, [7] if expected else [])
        for call in (Call(201), Call(0, release=1)):
            driver = object.__new__(residency.CudaDriver)
            driver._retain, driver._release = call.retain, call.release
            with self.assertRaises(residency.ObservationError):
                driver.mapped(0x1000)


if __name__ == "__main__":
    unittest.main()
