"""CPU-only subprocess tests; never import SGLang or touch a device."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import textwrap
import unittest
import venv


ROOT = str(Path(__file__).resolve().parents[2])
# ADR 0008 amendment 2026-10-17: the SGLANG_PLUGINS selection CapyCTL renders
# (runtime/sglang_startup_guards.py NO_PLUGINS_SELECTION).
NO_PLUGINS_SELECTION = "capyctl-loads-no-plugins"


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

    # T21 / SPEC §13.3: the default guard keeps engine output (it reaches the
    # private log through capyctl's redacting writer) and still rejects plugins.
    def test_default_guard_keeps_output_and_still_rejects_plugins(self):
        child = self.child("""
            from runtime.sglang_startup_guards import preimport_guard, StartupGuardError
            import os
            os.environ.pop("CAPYCTL_DEBUG_ENGINE_LOGS", None)
            os.environ["SGLANG_PLATFORM"] = "untrusted"
            try:
                preimport_guard()
            except StartupGuardError as error:
                assert error.code == "external_plugin_selection"
                print("default-visible", flush=True)
                os.write(2, b"native-visible")
            else:
                raise AssertionError("plugin gate bypassed")
        """)
        self.assertEqual(child.returncode, 0, child.stderr)
        self.assertEqual(child.stdout, b"default-visible\n")
        self.assertEqual(child.stderr, b"native-visible")

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

    # SGLang 0.5.22's smg-grpc-servicer 0.14.0 entry points, as installed.
    SMG_PLUGINS = ("[sglang.srt.plugins]\n"
                   "smg-servicer = smg_grpc_servicer.sglang.plugin:register\n"
                   "smg-zmq = smg_grpc_servicer.sglang.zmq_plugin:register\n")

    @staticmethod
    def install(root, name, entry_points):
        dist = Path(root, name.replace("-", "_") + "-1.0.dist-info")
        dist.mkdir()
        (dist / "METADATA").write_text(f"Name: {name}\nVersion: 1.0\n")
        (dist / "entry_points.txt").write_text(entry_points)

    def closure(self, root, environment, expected):
        """Run the guard in a fresh child over `root`'s installed metadata."""
        child = self.child(f'''
            from runtime.sglang_startup_guards import enforce_closed_plugins, StartupGuardError
            import os
            sys.path.insert(0, {root!r})
            for name in ("SGLANG_PLUGINS", "SGLANG_PLATFORM"):
                os.environ.pop(name, None)
            os.environ.update({environment!r})
            try:
                enforce_closed_plugins()
                outcome = None
            except StartupGuardError as error:
                outcome = str(error)
            assert not any(n.startswith(("smg_grpc_servicer", "module_that_must_not_load"))
                           for n in sys.modules)
            assert outcome == {expected!r}, outcome
        ''')
        self.assertEqual(child.returncode, 0, child.stderr)

    # T21, ADR 0008 amendment 2026-10-17: SGLang 0.5.22 requires
    # smg-grpc-servicer, whose plugins are installed with every 0.5.22. Under
    # CapyCTL's selection, which names none of them, SGLang loads none; found
    # live on host A, where every 0.5.22 launch failed plugin_closure_failed.
    def test_sglang_required_plugins_pass_only_under_capyctl_selection(self):
        with tempfile.TemporaryDirectory() as root:
            self.install(root, "smg-grpc-servicer", self.SMG_PLUGINS)
            self.closure(root, {"SGLANG_PLUGINS": NO_PLUGINS_SELECTION}, None)
            for environment in ({}, {"SGLANG_PLUGINS": ""}):
                with self.subTest(environment=environment):
                    self.closure(root, environment, "external_plugins_present")

    def test_unknown_distribution_plugin_refused_under_capyctl_selection(self):
        selected = {"SGLANG_PLUGINS": NO_PLUGINS_SELECTION}
        for required in (False, True):
            with self.subTest(required=required), tempfile.TemporaryDirectory() as root:
                if required:
                    self.install(root, "smg-grpc-servicer", self.SMG_PLUGINS)
                self.install(root, "fixture",
                             "[sglang.srt.plugins]\nprivate = module_that_must_not_load:run\n")
                self.closure(root, selected, "external_plugins_present")
        # A required distribution's plugin named as the selection would load.
        with tempfile.TemporaryDirectory() as root:
            self.install(root, "smg-grpc-servicer",
                         f"[sglang.srt.plugins]\n{NO_PLUGINS_SELECTION} = "
                         "module_that_must_not_load:run\n")
            self.closure(root, selected, "external_plugins_present")

    def test_platform_entry_point_refused_under_capyctl_selection(self):
        with tempfile.TemporaryDirectory() as root:
            self.install(root, "smg-grpc-servicer",
                         "[sglang.srt.platforms]\nprivate = module_that_must_not_load:run\n")
            self.closure(root, {"SGLANG_PLUGINS": NO_PLUGINS_SELECTION},
                         "external_plugins_present")

    def test_other_selections_and_smg_settings_refused(self):
        selected = {"SGLANG_PLUGINS": NO_PLUGINS_SELECTION}
        with tempfile.TemporaryDirectory() as root:
            self.install(root, "smg-grpc-servicer", self.SMG_PLUGINS)
            for environment in (
                    {"SGLANG_PLUGINS": "smg-zmq"},
                    {"SGLANG_PLUGINS": NO_PLUGINS_SELECTION + ",smg-zmq"},
                    {"SGLANG_PLUGINS": " " + NO_PLUGINS_SELECTION},
                    {**selected, "SGLANG_PLATFORM": "cuda"},
                    {**selected, "SMG_SGLANG_ZMQ_HANDSHAKE": "tcp://127.0.0.1:1"},
                    {**selected, "SMG_SGLANG_ZMQ_ENGINE_INDEX": "0"},
                    {**selected, "smg_sglang_servicer_impl": "rust"}):
                with self.subTest(environment=environment):
                    self.closure(root, environment, "external_plugin_selection")

    def test_capyctl_selection_reaches_spawned_interpreters_and_their_guard(self):
        # The scheduler children inherit the selection; their __mp_main__
        # guard runs over the same installed metadata before unpickling.
        for handshake in (False, True):
            with self.subTest(handshake=handshake), tempfile.TemporaryDirectory() as root:
                self.install(root, "smg-grpc-servicer", self.SMG_PLUGINS)
                child = self.child(f'''
                    import multiprocessing, os
                    sys.path.insert(0, {root!r})
                    os.environ.pop("SGLANG_PLATFORM", None)
                    os.environ["SGLANG_PLUGINS"] = {NO_PLUGINS_SELECTION!r}
                    if {handshake!r}:
                        os.environ["SMG_SGLANG_ZMQ_HANDSHAKE"] = "tcp://127.0.0.1:1"
                    sys.modules["__main__"].__file__ = {str(Path(ROOT, "runtime/sglang_entry.py"))!r}
                    process = multiprocessing.get_context("spawn").Process(target=id, args=(0,))
                    process.start()
                    process.join(5)
                    if process.is_alive():
                        process.kill()
                        process.join(2)
                        raise AssertionError("spawn did not finish")
                    assert process.exitcode == {1 if handshake else 0}, process.exitcode
                ''')
                self.assertEqual(child.returncode, 0, child.stderr)

    def test_debug_opt_in_preserves_output_but_still_rejects_plugins(self):
        child = self.child("""
            from runtime.sglang_startup_guards import preimport_guard, StartupGuardError
            import os
            os.environ["CAPYCTL_DEBUG_ENGINE_LOGS"] = "1"
            os.environ["SGLANG_PLATFORM"] = "untrusted"
            try:
                preimport_guard()
            except StartupGuardError as error:
                assert error.code == "external_plugin_selection"
                print("debug-visible")
            else:
                raise AssertionError("plugin gate bypassed")
        """)
        self.assertEqual(child.returncode, 0, child.stderr)
        self.assertEqual(child.stdout, b"debug-visible\n")

    # T21: SPEC §13.3. Engine logs keep output but never a credential, at the
    # default level and under debug alike: log records and Python-level writes
    # are scrubbed of bearer values and credential-shaped runs (capyctl's keys
    # are 64 hex characters).
    def test_output_is_scrubbed_of_credentials(self):
        key = "ab" * 32
        for debug in ("1", None):
            with self.subTest(debug=debug):
                select = ('os.environ["CAPYCTL_DEBUG_ENGINE_LOGS"] = "1"' if debug
                          else 'os.environ.pop("CAPYCTL_DEBUG_ENGINE_LOGS", None)')
                child = self.child(f"""
                    import logging, os, sys
                    {select}
                    from runtime.sglang_startup_guards import preimport_guard
                    preimport_guard()
                    logging.basicConfig(stream=sys.stderr, level=logging.INFO, format="%(message)s")
                    logging.getLogger("sglang").info("server_args=ServerArgs(api_key=%r)", "{key}")
                    print("Authorization: Bearer {key} and plain", flush=True)
                    print("ordinary line", flush=True)
                """)
                self.assertEqual(child.returncode, 0, child.stderr)
                output = child.stdout + child.stderr
                self.assertNotIn(key.encode(), output)
                self.assertIn(b"<redacted>", child.stderr)
                self.assertIn(b"ordinary line", child.stdout)
                self.assertIn(b"and plain", child.stdout)

    # T37 (found live 2026-10-14): SGLang's `server_args` line keeps its model
    # paths. A `.` in a path (`.cache`, `capyctl-0.1.3`) used to end the path,
    # and the long run after it was blanked as a credential; a key, a
    # credential-length segment inside a path and a bearer value are still.
    def test_scrub_keeps_model_paths_and_still_redacts_keys(self):
        sys.path.insert(0, ROOT)
        self.addCleanup(sys.path.remove, ROOT)
        from runtime.sglang_startup_guards import scrub
        store = ("/home/edurdias/capyctl-0.1.3-live-runs/2026-10-14-1854-b18/state/models/"
                 "sources/hf/nvidia/Qwen3.8-27B-NVFP4@482ca0f3b9a1c2d3e4f5a6b7c8d9e0f1a2b3c4d5")
        hub = ("/home/edurdias/.cache/huggingface/hub/models--RadixArk--Qwen3.6-35B-A3B-NVFP4/"
               "snapshots/0123456789abcdef0123456789abcdef01234567")
        plain = "/var/lib/capyctl/models/org/model/snapshots/0123456789abcdef0123456789abcdef01234567"
        key = "ab" * 32
        line = (f"server_args=ServerArgs(model_path='{store}', tokenizer_path='{hub}', "
                f"download_dir={plain!r}, api_key='{key}')")
        out = scrub(line)
        self.assertIn(f"model_path='{store}'", out)
        self.assertIn(f"tokenizer_path='{hub}'", out)
        self.assertIn(f"download_dir='{plain}'", out)
        self.assertNotIn(key, out)
        self.assertIn("api_key='<redacted>'", out)
        for text in (f"GET /v1/models/x.{key} 401", f"/home/u/.cache/{key}/w.bin",
                     f"Authorization: Bearer {key[:20]}", f"token {key}"):
            with self.subTest(text=text):
                scrubbed = scrub(text)
                self.assertNotIn(key[:20], scrubbed)
                self.assertIn("<redacted>", scrubbed)

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

    def test_prior_dependency_import_is_too_late_without_loading_native_code(self):
        # Module sentinels exercise the guard without importing engine packages.
        # Accepting Torch before this guard would miss earlier native effects.
        for name in ("torch", "torch.cuda", "transformers", "transformers.models",
                     "torch_memory_saver", "torch_memory_saver.hooks"):
            with self.subTest(name=name):
                child = self.child(f'''
                    from runtime.sglang_startup_guards import enforce_closed_plugins, StartupGuardError
                    sys.modules[{name!r}] = object()
                    try:
                        enforce_closed_plugins()
                    except StartupGuardError as error:
                        assert str(error) == "native_already_imported"
                    else:
                        raise AssertionError("late dependency accepted")
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

    def test_spawn_preparation_runs_the_guard_before_process_unpickle(self):
        # Exercise CPython's actual spawn preparation phase. It runs the main
        # script before unpickling the Process (and its native argument classes).
        # SPEC §13.3: output after preparation is kept (the launcher's log
        # writer redacts it), and Python-level writes are scrubbed.
        key = "cd" * 32
        child = self.child(f'''
            import multiprocessing.spawn, os
            os.environ.pop("SGLANG_PLATFORM", None)
            os.environ.pop("SGLANG_PLUGINS", None)
            multiprocessing.spawn.prepare({{"init_main_from_path": {str(Path(ROOT, "runtime/sglang_entry.py"))!r}}})
            assert "sglang" not in sys.modules
            assert "torch" not in sys.modules
            print("key {key}", flush=True)
            os.write(2, b"AFTER-PREPARATION")
        ''')
        self.assertEqual(child.returncode, 0, child.stderr)
        self.assertEqual((child.stdout, child.stderr),
                         (b"key <redacted>\n", b"AFTER-PREPARATION"))

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
                # A denied spawn never unpickles its arguments; an admitted one
                # keeps its output for the launcher's log writer.
                probe = (b"", b"") if denied else (b"PRIVATE-UNPICKLE-OUTPUT",
                                                   b"PRIVATE-UNPICKLE-ERROR")
                self.assertEqual((child.stdout, child.stderr), probe)
                self.assertEqual(marker.exists(), not denied)


class CutlassDslSearchPathTests(unittest.TestCase):
    """SPEC §9.2, ADR 0008 amendment 2026-10-19: nvidia-cutlass-dsl's `cutlass`
    is reachable under the entry's -IS composition, its .pth hook never runs.

    Found live on host A 2026-10-19: SGLang 0.5.22's scheduler failed
    `No module named 'cutlass'` serving a dense BF16 model; the package sits
    under <site-packages>/nvidia_cutlass_dsl/dsl_packages, added to the path
    only by `nvidia_cutlass_dsl_packages.pth`. CPU fakes only; never evidence
    a build serves.
    """

    ENTRY = str(Path(ROOT, "runtime", "sglang_entry.py"))

    @staticmethod
    def cutlass_package(directory, records):
        """A fake `cutlass` that records where and under which flags it loaded."""
        package = Path(directory, "cutlass")
        package.mkdir(parents=True)
        (package / "__init__.py").write_text(
            "import os, sys\nfrom pathlib import Path\n"
            f"Path({str(records)!r}, str(os.getpid())).write_text("
            "__file__ + ' ' + str(sys.flags.isolated) + str(sys.flags.no_site))\n")

    def installation(self, root, symlinked):
        """A fresh stdlib-only venv laid out as nvidia-cutlass-dsl installs, hook included."""
        environment = Path(root, "python")
        venv.EnvBuilder(with_pip=False).create(environment)
        python = str(environment / "bin" / "python")
        site = Path(subprocess.run(
            [python, "-I", "-B", "-c", "import sysconfig; print(sysconfig.get_path('purelib'))"],
            capture_output=True, text=True, timeout=15, check=True).stdout.strip())
        records = Path(root, "records")
        records.mkdir()
        dsl = site / "nvidia_cutlass_dsl" / "dsl_packages"
        if symlinked:
            outside = Path(root, "outside")
            self.cutlass_package(outside, records)
            dsl.parent.mkdir()
            dsl.symlink_to(outside, target_is_directory=True)
        else:
            self.cutlass_package(dsl, records)
            # Appended after the stdlib, it cannot shadow a stdlib module.
            (dsl / "colorsys.py").write_text("raise AssertionError('stdlib shadowed')\n")
        marker = Path(root, "pth-hook-executed")
        # nvidia-cutlass-dsl 4.8.0's hook, with a marker write added.
        (site / "nvidia_cutlass_dsl_packages.pth").write_text(
            "import sys, os, pathlib, nvidia_cutlass_dsl; "
            f"pathlib.Path({str(marker)!r}).write_text('executed'); "
            "sys.path.insert(0, os.path.join(nvidia_cutlass_dsl.__path__[0], 'dsl_packages'))\n")
        return python, site, dsl, records, marker

    def composed(self, python, expect_cutlass):
        """Run the entry's own composition under -IS, then a real spawn child."""
        body = textwrap.dedent(f'''
            import importlib, json, multiprocessing, os, sys, types
            assert (sys.flags.isolated, sys.flags.no_site) == (1, 1)
            package = types.ModuleType("runtime")
            package.__path__ = [{str(Path(ROOT, "runtime"))!r}]
            sys.modules["runtime"] = package
            from runtime import sglang_entry as entry
            before = list(sys.path)
            entry._compose_search_path()
            assert sys.path[:len(before)] == before, sys.path
            print(json.dumps(sys.path[len(before):]))
            import colorsys
            try:
                import cutlass
            except ModuleNotFoundError:
                assert not {expect_cutlass!r}
                raise SystemExit(0)
            assert "nvidia_cutlass_dsl" not in sys.modules
            # SGLang's scheduler and detokenizer: spawned, the entry as main.
            os.environ.pop("SGLANG_PLATFORM", None)
            os.environ.pop("SGLANG_PLUGINS", None)
            sys.modules["__main__"].__file__ = {self.ENTRY!r}
            process = multiprocessing.get_context("spawn").Process(
                target=importlib.import_module, args=("cutlass",))
            process.start()
            process.join(10)
            if process.is_alive():
                process.kill()
                process.join(2)
                raise AssertionError("spawn did not finish")
            assert process.exitcode == 0, process.exitcode
        ''')
        child = subprocess.run([python, "-IS", "-B", "-c", body], capture_output=True,
                               text=True, timeout=30,
                               env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"})
        self.assertEqual(child.returncode, 0, child.stderr)
        return json.loads(child.stdout)

    # T21
    def test_cutlass_imports_under_the_entry_composition_and_its_pth_never_runs(self):
        with tempfile.TemporaryDirectory() as root:
            python, site, dsl, records, marker = self.installation(root, symlinked=False)
            # Positive control: without -S the vendor hook runs and adds the path.
            control = subprocess.run([python, "-I", "-B", "-c", "import cutlass"],
                                     capture_output=True, timeout=15)
            self.assertEqual(control.returncode, 0, control.stderr)
            self.assertTrue(marker.exists(), "positive control did not execute the hook")
            marker.unlink()
            for record in records.iterdir():
                record.unlink()
            self.assertEqual(self.composed(python, expect_cutlass=True), [str(site), str(dsl)])
            self.assertFalse(marker.exists(), "the .pth hook ran under the entry")
            # The entry's process and its spawned child both loaded this one
            # package, both under -IS.
            loaded = sorted(record.read_text() for record in records.iterdir())
            self.assertEqual(loaded, [str(dsl / "cutlass" / "__init__.py") + " 11"] * 2)

    # T21
    def test_symlinked_dsl_packages_leaving_the_tree_is_not_added(self):
        with tempfile.TemporaryDirectory() as root:
            python, site, _, records, marker = self.installation(root, symlinked=True)
            self.assertEqual(self.composed(python, expect_cutlass=False), [str(site)])
            self.assertFalse(marker.exists(), "the .pth hook ran under the entry")
            self.assertEqual(list(records.iterdir()), [])

    # T21
    def test_only_real_directories_inside_the_tree_are_composed(self):
        sys.path.insert(0, ROOT)
        self.addCleanup(sys.path.remove, ROOT)
        from runtime.sglang_startup_guards import trusted_search_path
        with tempfile.TemporaryDirectory() as root:
            site = Path(root, "site-packages")
            outside = Path(root, "outside", "dsl_packages")
            outside.mkdir(parents=True)
            site.mkdir()
            self.assertEqual(trusted_search_path(str(site)), [str(site)])
            vendor = site / "nvidia_cutlass_dsl"
            # The vendor directory itself a symlink out of the tree.
            vendor.symlink_to(outside.parent, target_is_directory=True)
            self.assertEqual(trusted_search_path(str(site)), [str(site)])
            vendor.unlink()
            vendor.mkdir()
            (vendor / "dsl_packages").write_text("not a directory")
            self.assertEqual(trusted_search_path(str(site)), [str(site)])
            (vendor / "dsl_packages").unlink()
            (vendor / "dsl_packages").mkdir()
            self.assertEqual(trusted_search_path(str(site)),
                             [str(site), str(vendor / "dsl_packages")])


if __name__ == "__main__":
    unittest.main()
