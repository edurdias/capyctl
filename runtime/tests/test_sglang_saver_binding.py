"""CPU-only synthetic loaded modules and a tiny observer ABI library."""

import ctypes
import hashlib
import importlib
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import types
import unittest
from unittest import mock

from runtime import sglang_saver_binding


# The recipe's valid ServerArgs values for a launch without a weights backup.
BASE_VALUES = dict(enable_memory_saver=True, enable_weights_cpu_backup=False,
                   enable_draft_weights_cpu_backup=False)


class HostBackedBindingTest(unittest.TestCase):
    # T22 / ADR 0019: the weights backup is accepted only when the launch asked for it.
    def test_backup_accepted_for_host_backed(self):
        values = dict(BASE_VALUES, enable_weights_cpu_backup=True)
        sglang_saver_binding.check_values(values, weight_restore="cpu_backup")

    def test_no_backup_accepted_for_deep(self):
        sglang_saver_binding.check_values(dict(BASE_VALUES), weight_restore="disk_reload")

    def test_backup_refused_for_deep(self):
        values = dict(BASE_VALUES, enable_weights_cpu_backup=True)
        with self.assertRaises(sglang_saver_binding.SaverBindingError):
            sglang_saver_binding.check_values(values, weight_restore="disk_reload")

    def test_missing_backup_refused_for_host_backed(self):
        with self.assertRaises(sglang_saver_binding.SaverBindingError):
            sglang_saver_binding.check_values(dict(BASE_VALUES), weight_restore="cpu_backup")

    def test_draft_backup_always_refused(self):
        values = dict(BASE_VALUES, enable_weights_cpu_backup=True, enable_draft_weights_cpu_backup=True)
        with self.assertRaises(sglang_saver_binding.SaverBindingError):
            sglang_saver_binding.check_values(values, weight_restore="cpu_backup")

    def test_unknown_restore_and_disabled_saver_refused(self):
        for values, restore in ((dict(BASE_VALUES), "cpu"), (dict(BASE_VALUES), None),
                                (dict(BASE_VALUES, enable_memory_saver=False), "disk_reload")):
            with self.subTest(restore=restore):
                with self.assertRaises(sglang_saver_binding.SaverBindingError) as caught:
                    sglang_saver_binding.check_values(values, weight_restore=restore)
                self.assertEqual(caught.exception.code, "configuration_mismatch")


class SaverBindingTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory(prefix="mllm-saver-binding-", dir=Path.home())
        cls.root = Path(cls.temp.name)
        cls.root.chmod(0o700)
        source = cls.root / "fixture.c"
        source.write_text('''
#include <stdint.h>
struct record { uint64_t address, size, backup; int32_t device;
 uint32_t state, enabled, length; unsigned char tag[64]; };
static int mode;
void fixture_mode(int value) { mode = value; }
void tms_pause(void) {} void tms_resume(void) {}
void tms_set_current_tag(void) {} void tms_set_interesting_region(void) {}
void tms_set_enable_cpu_backup(void) {}
uint32_t tms_snapshot_v1(uint32_t version, uint32_t size,
 struct record *out, uint32_t capacity, uint32_t *count) {
 *count = 0;
 if (version != 1 || size != 104 || capacity < 1) return 2;
 if (mode == 2) return 1;
 if (mode == 3) return 0;
 out[0] = (struct record){ .address=4096, .size=4096, .device=0,
  .state=1, .enabled=(mode == 1), .length=7, .tag="weights" };
 *count = 1; return 0;
}
''')
        cls.path = cls.root / "observer.so"
        subprocess.run(["cc", "-shared", "-fPIC", "-Wall", "-Wextra", "-Werror",
                        "-o", str(cls.path), str(source)], check=True, capture_output=True)
        cls.path.chmod(0o500)
        cls.library = ctypes.CDLL(str(cls.path))  # Test fixture only; no CUDA symbols.
        cls.digest = hashlib.sha256(cls.path.read_bytes()).hexdigest()

    @classmethod
    def tearDownClass(cls):
        cls.temp.cleanup()

    def setUp(self):
        self.assertIsNotNone(importlib.util.find_spec("runtime.sglang_saver_binding"),
                             "scheduler binding has not been implemented")
        self.binding = importlib.import_module("runtime.sglang_saver_binding")
        self.library.fixture_mode(0)
        self.modules = {}
        def instance(module_name, class_name, **attributes):
            module = self.modules.setdefault(module_name, types.ModuleType(module_name))
            kind = type(class_name, (), {"__module__": module_name})
            setattr(module, class_name, kind)
            value = kind()
            value.__dict__.update(attributes)
            return value
        self.wrapper = instance("torch_memory_saver.binary_wrapper", "BinaryWrapper", cdll=self.library)
        self.hook = instance("torch_memory_saver.hooks.mode_preload", "HookUtilModePreload")
        self.pool = instance("torch.cuda.memory", "MemPool")
        self.impl = instance("torch_memory_saver.entrypoint", "_TorchMemorySaverImpl",
                             _binary_wrapper=self.wrapper, _hook_mode="preload",
                             _primary_mem_pool=self.pool, _hook_util=self.hook)
        self.saver = instance("torch_memory_saver.entrypoint", "TorchMemorySaver", _impl=self.impl)
        package = types.ModuleType("torch_memory_saver")
        package.torch_memory_saver = self.saver
        self.modules[package.__name__] = package
        self.adapter = instance("sglang.srt.utils.torch_memory_saver_adapter", "_TorchMemorySaverAdapterReal")
        adapter_module = self.modules[type(self.adapter).__module__]
        adapter_module._memory_saver = self.saver
        adapter_module.import_error = None
        self.args = instance("sglang.srt.server_args", "ServerArgs", enable_memory_saver=True,
                             enable_weights_cpu_backup=False, enable_draft_weights_cpu_backup=False)
        self.scheduler = instance("sglang.srt.managers.scheduler", "Scheduler",
                                  server_args=self.args, memory_saver_adapter=self.adapter)
        patcher = mock.patch.dict(sys.modules, self.modules)
        patcher.start()
        self.addCleanup(patcher.stop)
        self.owner = self.binding.current_process_identity()
        self.build = self.binding.TrustedSaverBuild(str(self.path), self.digest, "preload")

    def observe(self, **kwargs):
        extra = {"weight_restore": kwargs["weight_restore"]} if "weight_restore" in kwargs else {}
        return self.binding.observe_scheduler_saver(
            self.scheduler, expected_owner=kwargs.get("owner", self.owner),
            build=kwargs.get("build", self.build), **extra)

    def test_existing_singleton_observes_real_export_and_owner(self):
        with mock.patch.object(ctypes, "CDLL", side_effect=AssertionError("must not load")):
            result = self.observe()
        self.assertEqual(result.owner.pid, os.getpid())
        self.assertEqual(result.allocations.mapped_bytes, 4096)
        self.assertEqual(result.allocations.groups[0].tag, "weights")
        self.assertEqual(result.library.sha256, self.digest)
        self.assertNotIn(str(self.path), repr(result))

    def test_uninitialized_singleton_never_calls_lazy_initializer(self):
        self.saver._impl = None
        self.saver._ensure_initialized = lambda: self.fail("lazy initialization forbidden")
        with self.assertRaisesRegex(self.binding.SaverBindingError, "uninitialized"):
            self.observe()

    def test_noop_foreign_singleton_and_wrong_classes_fail(self):
        for field, replacement in (("memory_saver_adapter", object()), ("server_args", object())):
            original = getattr(self.scheduler, field)
            setattr(self.scheduler, field, replacement)
            with self.assertRaises(self.binding.SaverBindingError):
                self.observe()
            setattr(self.scheduler, field, original)
        self.modules["torch_memory_saver"].torch_memory_saver = object()
        with self.assertRaises(self.binding.SaverBindingError):
            self.observe()

    def test_disabled_and_backup_configuration_rejected_before_snapshot(self):
        for field, bad in (("enable_memory_saver", False), ("enable_memory_saver", 1),
                           ("enable_weights_cpu_backup", True),
                           ("enable_draft_weights_cpu_backup", True)):
            original = getattr(self.args, field)
            setattr(self.args, field, bad)
            with self.assertRaises(self.binding.SaverBindingError):
                self.observe()
            setattr(self.args, field, original)

    def test_snapshot_backup_and_busy_fail_without_retry(self):
        for mode, code in ((1, "backup_disallowed"), (2, "busy")):
            self.library.fixture_mode(mode)
            with self.assertRaisesRegex(self.binding.SaverBindingError, code):
                self.observe()

    # T22 / ADR 0019: a host_backed launch observes its weights backup; the
    # same snapshot is still refused for a launch that declared a disk reload.
    def test_weights_backup_observed_only_for_a_host_backed_launch(self):
        self.args.enable_weights_cpu_backup = True
        self.library.fixture_mode(1)
        result = self.observe(weight_restore="cpu_backup")
        self.assertEqual(result.allocations.groups[0].backup_enabled_count, 1)
        with self.assertRaisesRegex(self.binding.SaverBindingError, "configuration_mismatch"):
            self.observe()
        self.args.enable_weights_cpu_backup = False
        with self.assertRaisesRegex(self.binding.SaverBindingError, "configuration_mismatch"):
            self.observe(weight_restore="cpu_backup")

    def test_empty_snapshot_retains_only_scoped_facts(self):
        self.library.fixture_mode(3)
        result = self.observe()
        self.assertEqual(result.allocations.allocation_count, 0)
        self.assertFalse(hasattr(result, "qualified"))
        self.assertFalse(hasattr(result, "whole_process_bytes"))

    def test_expected_owner_must_match_current_process(self):
        from dataclasses import replace
        for key, value in (("pid", self.owner.pid + 1), ("start_ticks", self.owner.start_ticks + 1),
                           ("boot_id", "00000000-0000-0000-0000-000000000000")):
            with self.assertRaisesRegex(self.binding.SaverBindingError, "owner_mismatch"):
                self.observe(owner=replace(self.owner, **{key: value}))

    def test_hash_name_and_hook_must_match_trusted_build(self):
        from dataclasses import replace
        for key, value in (("sha256", "0" * 64), ("path", str(self.root / "other.so")),
                           ("hook_mode", "torch")):
            with self.assertRaises(self.binding.SaverBindingError):
                self.observe(build=replace(self.build, **{key: value}))

    def test_writable_loaded_backing_file_rejected(self):
        self.path.chmod(0o522)
        try:
            with self.assertRaises(self.binding.SaverBindingError):
                self.observe()
        finally:
            self.path.chmod(0o500)

    # T21 T37: owner decision 2026-09-23. The reviewed saver build follows the
    # owner-only rule: group write is trusted only under the owner's private
    # group; under a shared group it is refused.
    def test_group_writable_library_follows_the_owner_only_rule(self):
        from runtime import owner_only
        self.path.chmod(0o570)
        try:
            with mock.patch.object(owner_only, "system_private_group", return_value=True):
                self.assertEqual(self.observe().library.sha256, self.digest)
            with mock.patch.object(owner_only, "system_private_group", return_value=False):
                with self.assertRaisesRegex(self.binding.SaverBindingError, "unsafe_library"):
                    self.observe()
        finally:
            self.path.chmod(0o500)

    # T21 T37: found live 2026-09-23 (M28 on host-b). uv installs the saver
    # library as a hard link into its cache, so the installed file has several
    # names. The digest and the export mapping still bind the loaded inode, and
    # engine installation files carry no link-count rule (owner decision
    # 2026-09-23, ADR 0008); the hard-linked library is accepted.
    def test_hard_linked_library_is_accepted(self):
        import os
        link = self.root / "uv-cache-link.so"
        os.link(self.path, link)
        try:
            self.assertEqual(self.observe().library.sha256, self.digest)
        finally:
            link.unlink()

    def test_export_from_another_mapping_rejected(self):
        callback = ctypes.CFUNCTYPE(None)(lambda: None)
        with mock.patch.object(self.library, "tms_pause", callback):
            with self.assertRaisesRegex(self.binding.SaverBindingError, "library_mismatch"):
                self.observe()

    def test_wrong_hook_or_unrelated_pool_rejected(self):
        for field in ("_hook_util", "_primary_mem_pool"):
            original = getattr(self.impl, field)
            setattr(self.impl, field, object())
            with self.assertRaises(self.binding.SaverBindingError):
                self.observe()
            setattr(self.impl, field, original)

    def test_rebound_pool_after_snapshot_rejected(self):
        original = self.binding.observe_saver
        def replace_pool(cdll, **kwargs):
            result = original(cdll, **kwargs)
            self.impl._primary_mem_pool = type(self.pool)()
            return result
        with mock.patch.object(self.binding, "observe_saver", replace_pool):
            with self.assertRaises(self.binding.SaverBindingError):
                self.observe()

    def test_changed_chain_after_snapshot_rejected(self):
        original = self.binding.observe_saver
        def replace_chain(cdll, **kwargs):
            result = original(cdll, **kwargs)
            self.saver._impl = None
            return result
        with mock.patch.object(self.binding, "observe_saver", replace_chain):
            with self.assertRaises(self.binding.SaverBindingError):
                self.observe()

    def test_import_does_not_import_engine_or_load_library(self):
        result = subprocess.run([sys.executable, "-B", "-c", '''
import ctypes, sys
def deny(*args, **kwargs): raise AssertionError("library construction")
ctypes.CDLL = deny
import runtime.sglang_saver_binding
assert not any(name == "torch" or name.startswith("sglang.") for name in sys.modules)
'''], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()
