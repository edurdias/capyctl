"""Capability probes against synthetic engine package trees (ADR 0008).

Owner decision 2026-09-23: engine installation files get no hard-coded hashes
and no permission rule; the internals capyctl hooks are probed by shape at launch
and a missing one refuses only the dependent feature. Every tree here is a
synthetic stand-in written by the test (a "custom build" whose file bytes match
no stock release). These are CPU fixtures, never evidence that a build serves
a model or parks (AGENTS.md: CPU and fake tests are not qualification).
"""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import unittest

from runtime import engine_capabilities as capabilities
from runtime import sglang_server_args


REPO = Path(__file__).resolve().parents[2]
TESTS = Path(__file__).resolve().parent
PROBE = REPO / "runtime" / "engine_capabilities.py"
RESERVED = (tuple(sglang_server_args._RESERVED_CONSTANT) + tuple(sglang_server_args._RESERVED_BOUND)
            + ("trust_remote_code", "tokenizer_worker_num"))


def write(root, relative, text):
    path = Path(root) / relative
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(textwrap.dedent(text))


def sglang_tree(root, saver_hooks=True, marker="custom-build"):
    """A synthetic installed `sglang` (and saver) tree with a custom build's bytes."""
    fields = ", ".join(repr(name) for name in (*RESERVED, "dtype", "context_length"))
    for package in ("sglang", "sglang/srt", "sglang/srt/entrypoints", "sglang/srt/utils",
                    "sglang/srt/observability", "sglang/srt/managers"):
        write(root, package + "/__init__.py", "# %s\n" % marker)
    write(root, "sglang/srt/server_args.py", f"""
        # {marker}: a patched ServerArgs record, not SGLang 0.5.20's bytes.
        class ServerArgs:
            __struct_fields__ = ({fields},)
            @staticmethod
            def add_cli_args(parser):
                pass
            def resolve_once(self):
                pass
            def resolved_dict(self):
                return {{}}
        """)
    routes = ["/generate", "/v1/models"]
    if saver_hooks:
        routes += list(capabilities.SGLANG_DEEP_ROUTES)
    write(root, "sglang/srt/entrypoints/http_server.py", f"""
        # {marker}
        LAUNCHED = []
        class _Route:
            def __init__(self, path):
                self.path = path
        class _App:
            routes = [_Route(path) for path in {routes!r}]
        app = _App()
        def launch_server(server_args, run_scheduler_process_func=None):
            LAUNCHED.append(server_args)
        """)
    if saver_hooks:
        write(root, "sglang/srt/utils/torch_memory_saver_adapter.py", f"""
            # {marker}
            import torch_memory_saver
            import_error = None
            _memory_saver = torch_memory_saver.torch_memory_saver
            class TorchMemorySaverAdapter:
                @staticmethod
                def create(enable):
                    return _TorchMemorySaverAdapterReal()
            class _TorchMemorySaverAdapterReal(TorchMemorySaverAdapter):
                def region(self, tag, enable_cpu_backup=False):
                    pass
                def pause(self, tag):
                    pass
                def resume(self, tag):
                    pass
            """)
        write(root, "torch_memory_saver/__init__.py", """
            from .entrypoint import TorchMemorySaver
            torch_memory_saver = TorchMemorySaver()
            """)
        write(root, "torch_memory_saver/entrypoint.py", """
            class TorchMemorySaver:
                pass
            class _TorchMemorySaverImpl:
                pass
            """)
        write(root, "torch_memory_saver/hooks/__init__.py", "")
        write(root, "torch_memory_saver/hooks/mode_preload.py", "class HookUtilModePreload:\n    pass\n")
        write(root, "torch_memory_saver/binary_wrapper.py", "class BinaryWrapper:\n    pass\n")
        # The pool snapshot the SGLang 0.5.20 saver observation reads.
        write(root, "torch/__init__.py", "")
        write(root, "torch/cuda/__init__.py", "")
        write(root, "torch/cuda/memory.py", """
            class MemPool:
                def snapshot(self, include_traces=True):
                    return []
            """)
    else:
        # A build without the saver: SGLang's own fallback shape.
        write(root, "sglang/srt/utils/torch_memory_saver_adapter.py", f"""
            # {marker}
            import_error = ImportError("torch_memory_saver")
            class TorchMemorySaverAdapter:
                @staticmethod
                def create(enable):
                    return None
            """)
    write(root, "sglang/srt/observability/metrics_collector.py", f"""
        # {marker}
        NAMES = ("sglang:num_running_reqs", "sglang:num_queue_reqs", "sglang:token_usage")
        """)
    write(root, "sglang/srt/managers/scheduler.py", f"""
        # {marker}
        class Scheduler:
            def process_input_requests(self, recv_reqs):
                return None
            def run_event_loop(self):
                return None
        def run_scheduler_process(*args):
            return None
        """)


def probe_cli(engine, tree):
    result = subprocess.run([sys.executable, "-I", "-S", str(PROBE), engine, str(tree)],
                            capture_output=True, text=True, timeout=60)
    return result


class CapabilityProbeTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="capyctl-capabilities-")
        self.addCleanup(self.directory.cleanup)
        self.tree = Path(self.directory.name) / "site-packages"

    # T21 T22: a custom SGLang build (bytes match no stock release) that keeps
    # the shapes capyctl hooks reports every capability available.
    def test_custom_sglang_build_with_every_shape_is_fully_capable(self):
        sglang_tree(self.tree, saver_hooks=True)
        result = probe_cli("sglang", self.tree)
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(report["schema"], capabilities.SCHEMA)
        self.assertEqual(report["engine"], "sglang")
        self.assertEqual(report["capabilities"],
                         {"core": [], "deep_park": [], "metrics": [], "observation": []})

    # T21 T22: a build without the memory saver hooks lacks only deep_park
    # (and the saver-backed observation); core serving remains available.
    def test_build_without_saver_hooks_lacks_only_deep_park(self):
        sglang_tree(self.tree, saver_hooks=False)
        result = probe_cli("sglang", self.tree)
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)["capabilities"]
        self.assertEqual(report["core"], [])
        self.assertEqual(report["metrics"], [])
        self.assertIn("TorchMemorySaverAdapterReal.pause", report["deep_park"])
        self.assertIn("torch_memory_saver", report["deep_park"])
        self.assertIn("route:/release_memory_occupation", report["deep_park"])
        self.assertIn("TorchMemorySaver", report["observation"])
        # Labels are fixed probe names, never paths or exception text.
        self.assertNotIn(str(self.tree), result.stdout)

    def test_missing_engine_reports_every_capability_missing(self):
        self.tree.mkdir()
        result = probe_cli("sglang", self.tree)
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)["capabilities"]
        self.assertTrue(all(report[name] for name in capabilities.ENGINES["sglang"]))

    def test_tensorfold_probe_names_its_destinations_and_never_parks(self):
        # ADR 0023 §1, T41: core checks the serve destinations capyctl renders
        # or reserves; deep_park is always missing.
        import argparse
        parser = argparse.ArgumentParser()
        sub = parser.add_subparsers()
        serve = sub.add_parser("serve")
        for dest in capabilities.TENSORFOLD_DESTINATIONS:
            serve.add_argument("--" + dest.replace("_", "-"))
        report = capabilities.probe_tensorfold(parser, importer=lambda name: None)
        self.assertEqual(report.missing_labels("core"), ())
        self.assertEqual(report.missing_labels("deep_park"), ("unsupported",))
        bare = capabilities.probe_tensorfold(argparse.ArgumentParser(), importer=lambda name: None)
        self.assertIn("destination:context", bare.missing_labels("core"))

    def test_malformed_invocation_is_refused_without_output(self):
        for argv in ([], ["mystery", "/x"], ["sglang", "relative"]):
            with self.subTest(argv=argv):
                result = subprocess.run([sys.executable, "-I", "-S", str(PROBE), *argv],
                                        capture_output=True, text=True, timeout=30)
                self.assertEqual(result.returncode, 2)
                self.assertEqual(result.stdout, "")

    def test_required_capabilities_follow_the_memory_saver_setting(self):
        self.assertEqual(capabilities.sglang_required({"memory_saver": True}),
                         ("core", "deep_park"))
        self.assertEqual(capabilities.sglang_required({"memory_saver": False}), ("core",))

    # T21 T22 / ADR 0019: `host_backed` parks with the memory saver too, so it
    # needs deep_park, whose probe reads the weights CPU backup field; a build
    # whose ServerArgs lacks that field lacks deep_park.
    def test_host_backed_needs_deep_park_including_the_weights_backup_field(self):
        self.assertEqual(capabilities.sglang_required(
            {"memory_saver": True, "cpu_weight_backup": True}), ("core", "deep_park"))
        import types
        arguments = types.SimpleNamespace(ServerArgs=type("ServerArgs", (), {
            "__struct_fields__": ("enable_memory_saver",)}))
        missing = capabilities.sglang_deep_park(arguments, None, lambda name: None)
        self.assertIn("ServerArgs.enable_weights_cpu_backup", missing)
        self.assertNotIn("ServerArgs.enable_memory_saver", missing)

    def test_record_fields_read_struct_and_dataclass_shapes(self):
        import dataclasses

        @dataclasses.dataclass
        class Record:
            enable_memory_saver: bool = False

        class Struct:
            __struct_fields__ = ("enable_memory_saver",)

        self.assertEqual(capabilities.record_fields(Record), {"enable_memory_saver"})
        self.assertEqual(capabilities.record_fields(Struct), {"enable_memory_saver"})
        self.assertIsNone(capabilities.record_fields(object))


ENTRY_SCRIPT = r"""
import io, importlib, json, os, sys
repo, tests, tree, memory_saver = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4] == "1"
sys.path[:0] = [repo, tests]
sys.path.append(tree)
from unittest import mock
from runtime import sglang_entry as entry
from runtime import sglang_native_composition as composition
from runtime import sglang_server_args as server_args
import test_sglang_entry as fixtures

fixture = fixtures.LaunchFixture()
fixture.setUp()
settings = fixture.public["settings"]
settings["memory_saver"] = memory_saver
settings["cpu_weight_backup"] = False
settings["weight_restore"] = "disk_reload"

def guarded_import():
    return (importlib.import_module("sglang.srt.server_args"),
            importlib.import_module("sglang.srt.entrypoints.http_server"))

checked = mock.Mock()
error = io.StringIO()
with mock.patch.object(composition, "enforce_closed_plugins"), \
        mock.patch.object(entry, "_guarded_engine_import", side_effect=guarded_import), \
        mock.patch.object(server_args, "construct_server_args", return_value=checked):
    status = entry.main(fixture.argv(), fixture.payloads().__getitem__, error)
launched = sys.modules.get("sglang.srt.entrypoints.http_server")
print(json.dumps({"status": status, "stderr": error.getvalue(),
                  "launched": len(launched.LAUNCHED) if launched else 0}))
"""


class SglangEntryCapabilityTests(unittest.TestCase):
    """The protected entry's launch boundary against synthetic custom builds."""

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="capyctl-custom-sglang-")
        self.addCleanup(self.directory.cleanup)
        self.tree = Path(self.directory.name) / "venv" / "site-packages"

    def launch(self, memory_saver):
        result = subprocess.run(
            [sys.executable, "-I", "-S", "-c", ENTRY_SCRIPT, str(REPO), str(TESTS),
             str(self.tree), "1" if memory_saver else "0"],
            capture_output=True, text=True, timeout=60)
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads(result.stdout.strip().splitlines()[-1])

    # T21 T22: a custom SGLang build launches both `restart_only` (no memory
    # saver) and `deep` (memory saver on); no source audit refuses it.
    def test_custom_build_launches_restart_only_and_deep(self):
        sglang_tree(self.tree, saver_hooks=True)
        for memory_saver in (False, True):
            with self.subTest(memory_saver=memory_saver):
                outcome = self.launch(memory_saver)
                self.assertEqual(outcome, {"status": 0, "stderr": "", "launched": 1})

    # T21 T22 T37: a group-writable engine installation is accepted; there is
    # no permission rule on engine installation files (ADR 0008).
    def test_group_writable_installation_is_accepted(self):
        sglang_tree(self.tree, saver_hooks=True)
        for directory, _, files in os.walk(self.directory.name):
            os.chmod(directory, 0o775)
            for name in files:
                os.chmod(os.path.join(directory, name), 0o664)
        self.assertEqual(self.launch(True), {"status": 0, "stderr": "", "launched": 1})

    # T21 T22: without the saver hooks a `deep` launch is refused with the
    # typed closed reason and a hint; `restart_only` still serves.
    def test_build_without_saver_hooks_refuses_deep_and_serves_restart_only(self):
        sglang_tree(self.tree, saver_hooks=False)
        refused = self.launch(True)
        self.assertEqual(refused["status"], 1)
        self.assertEqual(refused["launched"], 0)
        self.assertTrue(refused["stderr"].startswith(
            "sglang_startup_failed: capability_missing:deep_park ("), refused["stderr"])
        self.assertIn("restart_only", refused["stderr"])
        self.assertNotIn(str(self.tree), refused["stderr"])
        self.assertEqual(self.launch(False), {"status": 0, "stderr": "", "launched": 1})

    # T22: a build missing an interface every launch needs is refused for any
    # tier with its own closed category.
    def test_build_missing_core_refuses_every_launch(self):
        sglang_tree(self.tree, saver_hooks=True)
        (self.tree / "sglang/srt/server_args.py").write_text(
            "class ServerArgs:\n    __struct_fields__ = ('host',)\n")
        for memory_saver in (False, True):
            with self.subTest(memory_saver=memory_saver):
                outcome = self.launch(memory_saver)
                self.assertEqual((outcome["status"], outcome["launched"]), (1, 0))
                self.assertTrue(outcome["stderr"].startswith(
                    "sglang_startup_failed: capability_missing:core"), outcome["stderr"])


if __name__ == "__main__":
    unittest.main()
