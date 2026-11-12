"""CPU-only artifact integrity tests; not native engine qualification."""
import hashlib
import io
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import release_pins


class ReleasePinsTests(unittest.TestCase):
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
