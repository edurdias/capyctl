"""CPU-only subprocess tests; never import SGLang or touch a device."""

import os
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import unittest


ROOT = str(Path(__file__).resolve().parents[2])


class StartupGuardsTests(unittest.TestCase):
    def child(self, body):
        return subprocess.run(
            [sys.executable, "-I", "-B", "-c",
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
            subprocess.run([sys.executable, "-I", "-B", "-c",
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


if __name__ == "__main__":
    unittest.main()
