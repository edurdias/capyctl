"""CPU-only subprocess tests; never import SGLang or touch a device."""

import os
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import unittest
import venv


ROOT = str(Path(__file__).resolve().parents[2])


class StartupGuardsTests(unittest.TestCase):
    def test_no_site_flag_prevents_installed_hooks_before_protected_entry(self):
        # -I disables user site and environment paths, not installed .pth code.
        # Use only a fresh stdlib-only test environment, never an engine install.
        with tempfile.TemporaryDirectory() as root:
            environment = Path(root, "python")
            venv.EnvBuilder(with_pip=False).create(environment)
            python = str(environment / "bin" / "python")
            site = subprocess.run(
                [python, "-I", "-B", "-c",
                 "import sysconfig; print(sysconfig.get_path('purelib'))"],
                capture_output=True, text=True, timeout=15, check=True)
            marker = Path(root, "site-hook-executed")
            Path(site.stdout.strip(), "startup_probe.pth").write_text(
                "import pathlib; pathlib.Path(" + repr(str(marker)) +
                ").write_text('executed before entry')\n")
            body = "import sys; print(sys.flags.isolated, sys.flags.no_site)"
            before = subprocess.run([python, "-I", "-B", "-c", body],
                                    capture_output=True, timeout=15, check=True)
            self.assertEqual(before.stdout, b"1 0\n")
            self.assertTrue(marker.exists(), "positive control did not execute installed hook")
            marker.unlink()
            protected = subprocess.run([python, "-IS", "-B", "-c", body],
                                       capture_output=True, timeout=15, check=True)
            self.assertEqual(protected.stdout, b"1 1\n")
            self.assertEqual(protected.stderr, b"")
            self.assertFalse(marker.exists(), "installed hook ran before the protected entry")

    def child(self, body):
        return subprocess.run(
            [sys.executable, "-IS", "-B", "-c",
             "import sys\nsys.path.insert(0, " + repr(ROOT) + ")\n" +
             textwrap.dedent(body)], capture_output=True, timeout=10,
            env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"})

    def test_containment_suppresses_python_native_and_child_output(self):
        child = self.child('''
            from runtime.sglang_startup_guards import contain_startup_output
            import ctypes, logging, os, subprocess, traceback
            contain_startup_output()
            print("PRIVATE-PRINT", flush=True)
            logging.error("PRIVATE-LOG")
            try:
                raise ValueError("PRIVATE-TRACE")
            except ValueError:
                traceback.print_exc()
            os.write(1, b"PRIVATE-WRITE")
            os.write(2, b"PRIVATE-WRITE")
            libc = ctypes.CDLL(None)
            libc.puts(b"PRIVATE-C")
            libc.fflush(None)
            subprocess.run([sys.executable, "-IS", "-B", "-c",
                            "import os; os.write(2,b'PRIVATE-CHILD')"], check=True)
        ''')
        self.assertEqual(child.returncode, 0, child.stderr)
        self.assertEqual((child.stdout, child.stderr), (b"", b""))

    def test_containment_survives_unhandled_exception_and_buffered_shutdown(self):
        child = self.child('''
            from runtime.sglang_startup_guards import contain_startup_output
            print("PRIVATE-BUFFER", end="")
            contain_startup_output()
            raise RuntimeError("PRIVATE-UNHANDLED")
        ''')
        self.assertNotEqual(child.returncode, 0)
        self.assertEqual((child.stdout, child.stderr), (b"", b""))

    def test_empty_plugin_environment_does_not_bypass_installed_entry_points(self):
        # Omitting the platform group check would execute its registered loader.
        for group in ("sglang.srt.platforms", "sglang.srt.plugins"):
            with self.subTest(group=group), tempfile.TemporaryDirectory() as root:
                dist = Path(root, "fixture-1.0.dist-info")
                dist.mkdir()
                (dist / "METADATA").write_text("Name: fixture\nVersion: 1.0\n")
                (dist / "entry_points.txt").write_text(
                    f"[{group}]\nprivate-name = module_that_must_not_load:run\n")
                child = self.child(f'''
                    from runtime.sglang_startup_guards import enforce_closed_plugins, StartupGuardError
                    import os
                    sys.path.insert(0, {root!r})
                    os.environ["SGLANG_PLUGINS"] = ""
                    os.environ.pop("SGLANG_PLATFORM", None)
                    try:
                        enforce_closed_plugins()
                    except StartupGuardError as error:
                        assert str(error) == "external_plugins_present"
                        assert "module_that_must_not_load" not in sys.modules
                    else:
                        raise AssertionError("plugin accepted")
                ''')
                self.assertEqual(child.returncode, 0, child.stderr)

    def test_nonempty_plugin_or_platform_selection_rejected(self):
        for name in ("SGLANG_PLUGINS", "SGLANG_PLATFORM"):
            child = self.child(f'''
                from runtime.sglang_startup_guards import enforce_closed_plugins, StartupGuardError
                import os
                os.environ[{name!r}] = "PRIVATE-SELECTION"
                try:
                    enforce_closed_plugins()
                except StartupGuardError as error:
                    assert str(error) == "external_plugin_selection"
                else:
                    raise AssertionError("selection accepted")
            ''')
            self.assertEqual(child.returncode, 0, child.stderr)

    def test_prior_native_import_is_too_late(self):
        child = self.child('''
            from runtime.sglang_startup_guards import enforce_closed_plugins, StartupGuardError
            sys.modules["sglang.srt.plugins"] = object()
            try:
                enforce_closed_plugins()
            except StartupGuardError as error:
                assert str(error) == "native_already_imported"
            else:
                raise AssertionError("late guard accepted")
        ''')
        self.assertEqual(child.returncode, 0, child.stderr)

    def test_empty_inventory_allows_preimport_check_without_importing_native(self):
        child = self.child('''
            from runtime.sglang_startup_guards import enforce_closed_plugins
            import os
            os.environ.pop("SGLANG_PLATFORM", None)
            os.environ.pop("SGLANG_PLUGINS", None)
            enforce_closed_plugins()
            assert not any(n == "sglang" or n.startswith("sglang.") for n in sys.modules)
        ''')
        self.assertEqual(child.returncode, 0, child.stderr)

    def test_output_setup_failure_denies_without_native_import(self):
        child = self.child('''
            from runtime.sglang_startup_guards import contain_startup_output, StartupGuardError
            import resource
            resource.setrlimit(resource.RLIMIT_NOFILE, (0, 0))
            try:
                contain_startup_output()
            except StartupGuardError as error:
                assert str(error) == "output_containment_failed"
            else:
                raise AssertionError("failed setup accepted")
            assert "sglang" not in sys.modules
        ''')
        self.assertEqual(child.returncode, 0, child.stderr)

    def test_metadata_discovery_failure_is_closed_and_sanitized(self):
        child = self.child('''
            from runtime.sglang_startup_guards import enforce_closed_plugins, StartupGuardError
            import os
            os.environ.pop("SGLANG_PLATFORM", None)
            os.environ.pop("SGLANG_PLUGINS", None)
            class BrokenMetadataFinder:
                def find_distributions(self, context):
                    raise RuntimeError("PRIVATE-INVENTORY-PATH")
            sys.meta_path.append(BrokenMetadataFinder())
            try:
                enforce_closed_plugins()
            except StartupGuardError as error:
                assert str(error) == "plugin_inventory_unavailable"
                assert "PRIVATE" not in repr(error)
            else:
                raise AssertionError("unknown inventory accepted")
        ''')
        self.assertEqual(child.returncode, 0, child.stderr)

    def test_spawn_preparation_contains_output_before_process_unpickle(self):
        # Exercise CPython's actual spawn preparation phase. It runs the main
        # script before unpickling the Process (and its native argument classes).
        child = self.child(f'''
            import multiprocessing.spawn, os
            os.environ.pop("SGLANG_PLATFORM", None)
            os.environ.pop("SGLANG_PLUGINS", None)
            multiprocessing.spawn.prepare({{"init_main_from_path": {str(Path(ROOT, "runtime/sglang_entry.py"))!r}}})
            assert "sglang" not in sys.modules
            assert "torch" not in sys.modules
            os.write(1, b"PRIVATE-AFTER-PREPARATION")
            os.write(2, b"PRIVATE-ARGUMENT-UNPICKLE")
        ''')
        self.assertEqual(child.returncode, 0, child.stderr)
        self.assertEqual((child.stdout, child.stderr), (b"", b""))

    def test_spawn_preparation_denies_plugins_before_process_unpickle(self):
        for name in ("SGLANG_PLUGINS", "SGLANG_PLATFORM"):
            child = self.child(f'''
                import multiprocessing.spawn, os
                os.environ[{name!r}] = "PRIVATE-SELECTION"
                try:
                    multiprocessing.spawn.prepare({{"init_main_from_path": {str(Path(ROOT, "runtime/sglang_entry.py"))!r}}})
                except BaseException:
                    os._exit(73)
                os._exit(74)
            ''')
            self.assertEqual(child.returncode, 73, child.stderr)
            self.assertEqual((child.stdout, child.stderr), (b"", b""))

    def test_spawn_preparation_denies_installed_plugin_without_loading_target(self):
        with tempfile.TemporaryDirectory() as root:
            dist = Path(root, "fixture-1.0.dist-info")
            dist.mkdir()
            (dist / "METADATA").write_text("Name: fixture\nVersion: 1.0\n")
            (dist / "entry_points.txt").write_text(
                "[sglang.srt.plugins]\nprivate = module_that_must_not_load:run\n")
            child = self.child(f'''
                import multiprocessing.spawn, os
                sys.path.insert(0, {root!r})
                os.environ.pop("SGLANG_PLATFORM", None)
                os.environ.pop("SGLANG_PLUGINS", None)
                try:
                    multiprocessing.spawn.prepare({{"init_main_from_path": {str(Path(ROOT, "runtime/sglang_entry.py"))!r}}})
                except BaseException:
                    os._exit(73 if "module_that_must_not_load" not in sys.modules else 75)
                os._exit(74)
            ''')
            self.assertEqual(child.returncode, 73, child.stderr)
            self.assertEqual((child.stdout, child.stderr), (b"", b""))

    def test_real_spawn_guards_imports_triggered_by_argument_unpickling(self):
        for denied in (False, True):
            with self.subTest(denied=denied), tempfile.TemporaryDirectory() as root:
                marker = Path(root, "argument-imported")
                Path(root, "argument_probe.py").write_text(
                    "import os, sys\nfrom pathlib import Path\n"
                    "assert sys.flags.isolated == 1 and sys.flags.no_site == 1\n"
                    f"Path({str(marker)!r}).write_text('imported')\n"
                    "os.write(1, b'PRIVATE-UNPICKLE-OUTPUT')\n"
                    "os.write(2, b'PRIVATE-UNPICKLE-ERROR')\n")
                child = self.child(f'''
                    import multiprocessing, os
                    sys.path.insert(0, {root!r})
                    os.environ.pop("SGLANG_PLATFORM", None)
                    os.environ.pop("SGLANG_PLUGINS", None)
                    if {denied!r}:
                        os.environ["SGLANG_PLUGINS"] = "PRIVATE-SELECTION"
                    # The protected wrapper is the actual production main path.
                    sys.modules["__main__"].__file__ = {str(Path(ROOT, "runtime/sglang_entry.py"))!r}
                    class DeferredArgument:
                        def __reduce__(self):
                            return (__import__, ("argument_probe",))
                    process = multiprocessing.get_context("spawn").Process(
                        target=id, args=(DeferredArgument(),))
                    process.start()
                    process.join(5)
                    if process.is_alive():
                        process.kill()
                        process.join(2)
                        raise AssertionError("spawn did not finish")
                    assert process.exitcode == {1 if denied else 0}, process.exitcode
                ''')
                self.assertEqual(child.returncode, 0, child.stderr)
                self.assertEqual((child.stdout, child.stderr), (b"", b""))
                self.assertEqual(marker.exists(), not denied)


if __name__ == "__main__":
    unittest.main()
