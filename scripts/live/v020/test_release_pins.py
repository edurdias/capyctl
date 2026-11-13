"""CPU-only artifact integrity tests; not native engine qualification."""
import hashlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import release_pins


class ReleasePinsTests(unittest.TestCase):
    def override_packages(self):
        return {
            "vllm": "0.34.0rc1", "lmcache": "0.5.7", "torch": "2.14.0+cu130",
            "opentelemetry-api": "1.45.1", "opentelemetry-sdk": "1.45.1",
            "opentelemetry-exporter-otlp": "1.45.1",
            "opentelemetry-exporter-prometheus": "0.66b1",
        }

    def test_lmcache_override_records_installed_versions_and_owner_reason(self):
        source = {"version": "0.5.7", "archive_sha256": "a" * 64}
        receipt = release_pins.lmcache_034_provenance(self.override_packages(), source)
        self.assertTrue(receipt["test_only"])
        self.assertFalse(receipt["qualified"])
        self.assertIn("Owner-approved", receipt["reason"])
        self.assertIn("opentelemetry-api>=1.45.1", receipt["reason"])
        self.assertEqual(receipt["build_torch"], "2.14.0+cu130")
        self.assertEqual(receipt["source"], source)
        self.assertEqual(receipt["resolver"], "uv --override")
        self.assertFalse(receipt["build_isolation"])
        self.assertEqual(receipt["packages"], self.override_packages())

    def test_lmcache_override_rejects_wrong_installed_family(self):
        for name, version in (("opentelemetry-api", "1.40.0"),
                              ("opentelemetry-exporter-prometheus", "0.61b0"),
                              ("lmcache", "0.5.6"), ("vllm", "0.33.0")):
            with self.subTest(name=name):
                packages = self.override_packages()
                packages[name] = version
                with self.assertRaises(ValueError):
                    release_pins.lmcache_034_provenance(packages, {"version": "0.5.7"})

    def response(self, content):
        response = io.BytesIO(content)
        response.status = 200
        return response

    @patch.object(release_pins.os, "killpg")
    @patch.object(release_pins.subprocess, "Popen")
    @patch.object(release_pins.subprocess, "run")
    @patch.object(release_pins.urllib.request, "build_opener")
    def test_mp_smoke_records_import_metrics_and_stops_owned_process(
            self, opener, run, popen, killpg):
        process = popen.return_value
        process.pid, process.returncode = 123, 0
        process.poll.return_value = None
        metrics = (b'# TYPE target_info gauge\ntarget_info{service_name="lmcache"} 1\n'
                   b"# TYPE lmcache_mp_event_bus_dropped_events_total counter\n"
                   b"lmcache_mp_event_bus_dropped_events_total 0\n")
        opener.return_value.open.side_effect = [
            self.response(b'{"is_healthy":true}'), self.response(metrics)]
        with tempfile.TemporaryDirectory() as directory:
            evidence = Path(directory)
            release_pins.verify_lmcache_startup(Path("/test-only-env"), evidence, "CACHE")
            receipt = json.loads((evidence / "CACHE.startup.json").read_text())
            self.assertTrue(receipt["import_lmcache"])
            self.assertTrue(receipt["mp_startup"])
            self.assertTrue(receipt["metrics"])
            self.assertTrue(receipt["process_stopped"])
            self.assertFalse(receipt["qualified"])
            self.assertEqual(receipt["native_metric_names"],
                             ["lmcache_mp_event_bus_dropped_events_total"])
            self.assertFalse(receipt["row_metrics_verified"])
            self.assertEqual(receipt["required_row_counter_families"],
                             ["lookup_hit_tokens", "lookup_requested_tokens", "num_chunks_loaded"])
            self.assertEqual((evidence / "CACHE.startup.metrics.txt").read_bytes(), metrics)
        self.assertEqual(run.call_args.kwargs["timeout"], 30)
        command = popen.call_args.args[0]
        self.assertEqual(command[0], "/test-only-env/bin/lmcache")
        self.assertIn("engine_driven", command)
        self.assertIn("--no-l1-use-lazy", command)
        self.assertEqual(command[command.index("--host") + 1], "127.0.0.1")
        self.assertEqual(command[command.index("--http-host") + 1], "127.0.0.1")
        self.assertTrue(popen.call_args.kwargs["start_new_session"])
        self.assertEqual(popen.call_args.kwargs["env"]["LMCACHE_TRACK_USAGE"], "false")
        killpg.assert_called_once_with(123, release_pins.signal.SIGTERM)
        process.wait.assert_called_once_with(timeout=10)

    @patch.object(release_pins.os, "killpg")
    @patch.object(release_pins.subprocess, "Popen")
    @patch.object(release_pins.subprocess, "run")
    def test_mp_smoke_exit_fails_instead_of_qualifying(self, run, popen, killpg):
        process = popen.return_value
        process.pid, process.returncode = 123, 1
        process.poll.return_value = 1
        with tempfile.TemporaryDirectory() as directory:
            evidence = Path(directory)
            with self.assertRaisesRegex(RuntimeError, "process exited"):
                release_pins.verify_lmcache_startup(Path("/test-only-env"), evidence, "CACHE")
            receipt = json.loads((evidence / "CACHE.startup.json").read_text())
            self.assertTrue(receipt["import_lmcache"])
            self.assertFalse(receipt["mp_startup"])
            self.assertFalse(receipt["metrics"])
            self.assertFalse((evidence / "CACHE.startup.metrics.txt").exists())
        killpg.assert_called_once()

    @patch.object(release_pins.os, "killpg")
    @patch.object(release_pins.subprocess, "Popen")
    @patch.object(release_pins.subprocess, "run")
    @patch.object(release_pins.time, "monotonic", side_effect=[0, 0, 91, 92])
    def test_mp_smoke_deadline_kills_unresponsive_server(self, clock, run, popen, killpg):
        process = popen.return_value
        process.pid, process.returncode = 123, -9
        process.wait.side_effect = [release_pins.subprocess.TimeoutExpired("lmcache", 10), -9]
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(TimeoutError, "exceeded 90 seconds"):
                release_pins.verify_lmcache_startup(Path("/test-only-env"), Path(directory), "CACHE")
            receipt = json.loads((Path(directory) / "CACHE.startup.json").read_text())
            self.assertFalse(receipt["mp_startup"])
            self.assertTrue(receipt["process_stopped"])
        self.assertEqual([call.args[1] for call in killpg.call_args_list],
                         [release_pins.signal.SIGTERM, release_pins.signal.SIGKILL])
        self.assertEqual([call.kwargs["timeout"] for call in process.wait.call_args_list], [10, 5])

    @patch.object(release_pins.subprocess, "Popen")
    @patch.object(release_pins.subprocess, "run",
                  side_effect=release_pins.subprocess.CalledProcessError(1, "import lmcache"))
    def test_import_failure_never_starts_mp_server(self, run, popen):
        with tempfile.TemporaryDirectory() as directory:
            evidence = Path(directory)
            with self.assertRaises(release_pins.subprocess.CalledProcessError):
                release_pins.verify_lmcache_startup(Path("/test-only-env"), evidence, "CACHE")
            receipt = json.loads((evidence / "CACHE.startup.json").read_text())
            self.assertFalse(receipt["import_lmcache"])
            self.assertFalse(receipt["mp_startup"])
        popen.assert_not_called()

    @patch.object(release_pins.os, "killpg")
    @patch.object(release_pins.subprocess, "Popen")
    @patch.object(release_pins.subprocess, "run")
    @patch.object(release_pins.urllib.request, "build_opener")
    @patch.object(release_pins.time, "sleep")
    @patch.object(release_pins.time, "monotonic", side_effect=[0, 0, 1, 91, 92])
    def test_target_info_and_python_metrics_do_not_prove_native_metrics(
            self, clock, sleep, opener, run, popen, killpg):
        process = popen.return_value
        process.pid, process.returncode = 123, 0
        process.poll.return_value = None
        opener.return_value.open.side_effect = [
            self.response(b'{"is_healthy":true}'),
            self.response(b"# TYPE python_gc_objects_collected_total counter\n"
                          b"python_gc_objects_collected_total 1\n"
                          b"# TYPE target_info gauge\ntarget_info 1\n")]
        with tempfile.TemporaryDirectory() as directory:
            evidence = Path(directory)
            with self.assertRaisesRegex(TimeoutError, "exceeded 90 seconds"):
                release_pins.verify_lmcache_startup(Path("/test-only-env"), evidence, "CACHE")
            receipt = json.loads((evidence / "CACHE.startup.json").read_text())
            self.assertFalse(receipt["metrics"])
            self.assertFalse((evidence / "CACHE.startup.metrics.txt").exists())

    def test_architecture_selection_and_provenance(self):
        for arch, alias in (("x86_64", "amd64"), ("aarch64", "arm64")):
            pin = release_pins.vllm_wheel(arch)
            self.assertEqual(pin, release_pins.vllm_wheel(alias))
            self.assertIn("/52aeb580658571613f8ab28a075a06fa30279beb/", pin.url)
            self.assertTrue(pin.filename.endswith(f"manylinux_2_28_{arch}.whl"))
            self.assertEqual(len(pin.sha256), 64)
        self.assertNotEqual(release_pins.vllm_wheel("x86_64").sha256,
                            release_pins.vllm_wheel("aarch64").sha256)
        with self.assertRaisesRegex(ValueError, "unsupported architecture"):
            release_pins.vllm_wheel("riscv64")

    def fixture_pin(self):
        data = b"downloaded artifact bytes"
        return data, release_pins.WheelPin(
            "https://example.invalid/vllm.whl", hashlib.sha256(data).hexdigest())

    def test_download_checks_bytes_and_reuses_verified_file(self):
        data, pin = self.fixture_pin()
        with tempfile.TemporaryDirectory() as directory:
            destination = Path(directory) / "downloads"
            with patch.object(release_pins.urllib.request, "urlopen", return_value=io.BytesIO(data)):
                path = release_pins.download_wheel(pin, destination)
            self.assertEqual(path, destination / "vllm.whl")
            self.assertEqual(path.read_bytes(), data)
            with patch.object(release_pins.urllib.request, "urlopen", side_effect=AssertionError("unexpected network")):
                self.assertEqual(release_pins.download_wheel(pin, destination), path)

    def test_download_mismatch_never_publishes_wheel(self):
        _, pin = self.fixture_pin()
        with tempfile.TemporaryDirectory() as directory:
            destination = Path(directory)
            with patch.object(release_pins.urllib.request, "urlopen", return_value=io.BytesIO(b"corrupt")):
                with self.assertRaisesRegex(ValueError, "SHA256 mismatch"):
                    release_pins.download_wheel(pin, destination)
            self.assertEqual(list(destination.iterdir()), [])

    def test_existing_corrupt_wheel_is_rejected_without_network(self):
        _, pin = self.fixture_pin()
        with tempfile.TemporaryDirectory() as directory:
            destination = Path(directory)
            (destination / pin.filename).write_bytes(b"corrupt cache")
            with patch.object(release_pins.urllib.request, "urlopen", side_effect=AssertionError("unexpected network")):
                with self.assertRaisesRegex(ValueError, "SHA256 mismatch"):
                    release_pins.download_wheel(pin, destination)

    def test_interrupted_download_removes_partial_file(self):
        _, pin = self.fixture_pin()
        with tempfile.TemporaryDirectory() as directory:
            destination = Path(directory)
            with patch.object(release_pins.urllib.request, "urlopen", side_effect=OSError("network interrupted")):
                with self.assertRaisesRegex(OSError, "network interrupted"):
                    release_pins.download_wheel(pin, destination)
            self.assertEqual(list(destination.iterdir()), [])


if __name__ == "__main__":
    unittest.main()
