"""Synthetic kernel filesystem fixtures; never query or initialize a GPU."""

from dataclasses import replace
import importlib
import json
import os
from pathlib import Path
import tempfile
import sys
import unittest
from unittest import mock

from runtime import sglang_device as device
from test_sglang_entry import LaunchFixture


UUID = "GPU-09631200-fdff-a345-295f-a1a6f84b2f84"
BDF = "000f:01:00.0"


class DeviceTests(LaunchFixture, unittest.TestCase):
    def setUp(self):
        super().setUp()
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.proc = Path(self.temp.name) / "proc"
        self.sys = Path(self.temp.name) / "sys"
        self.info = self.proc / "driver/nvidia/gpus" / BDF / "information"
        self.info.parent.mkdir(parents=True)
        self.info.write_text(f"Model: Unknown\nGPU UUID: {UUID}\nBus Location: {BDF}\nDevice Minor: 0\nGPU Excluded: No\n")
        boot = self.proc / "sys/kernel/random/boot_id"
        boot.parent.mkdir(parents=True)
        boot.write_text("12345678-1234-1234-1234-123456789abc\n")
        self.pci = self.sys / "bus/pci/devices" / BDF
        self.pci.mkdir(parents=True)
        (self.pci / "vendor").write_text("0x10de\n")
        (self.pci / "device").write_text("0x2e12\n")

    def collect(self):
        return device._collect_inventory(self.proc, self.sys, "host-a", "aarch64")

    def policy(self):
        return device.TrustedDeviceMapping(
            host_id="host-a", hardware_fingerprint="hardware-v1",
            device_id="gpu0", memory_domain="uma", physical_gpu_uuid=UUID,
            inventory_digest=self.collect().digest)

    def observe(self, policy=None):
        with mock.patch.object(device, "collect_inventory", self.collect):
            return device.observe_placement(self.build(), policy or self.policy())

    def test_inventory_correlates_uuid_pci_and_host_without_model_name(self):
        inventory = self.collect()
        self.assertEqual(inventory.devices[0].physical_gpu_uuid, UUID)
        self.assertEqual(inventory.devices[0].pci_address, BDF)
        self.assertEqual(inventory.devices[0].device_id, "0x2e12")
        self.assertEqual(inventory.host_id, "host-a")
        self.assertEqual(len(inventory.digest), 64)
        self.assertGreater(inventory.observed_at_ns, 0)
        self.assertEqual(inventory.digest, self.collect().digest)
        (self.pci / "device").write_text("0x1234\n")
        self.assertNotEqual(inventory.digest, self.collect().digest)

    def test_malformed_missing_excluded_or_conflicting_evidence_denied(self):
        original = self.info.read_text()
        for bad in (original.replace(BDF, "0000:02:00.0"),
                    original + f"GPU UUID: {UUID}\n", original.replace("No", "Yes"),
                    original.replace(UUID, "GPU-short"), "x" * 16385):
            with self.subTest(bad=bad[:80]):
                self.info.write_text(bad)
                with self.assertRaises(device.DeviceObservationError):
                    self.collect()
        self.info.write_text(original)
        (self.pci / "vendor").write_text("0x1234\n")
        with self.assertRaises(device.DeviceObservationError):
            self.collect()

    def test_leaf_symlink_and_non_bdf_directory_denied(self):
        self.info.rename(self.info.with_name("saved"))
        self.info.symlink_to(self.info.with_name("saved"))
        with self.assertRaises(device.DeviceObservationError):
            self.collect()
        self.info.unlink()
        self.info.with_name("saved").rename(self.info)
        (self.info.parent.parent / "not-a-pci-address").mkdir()
        with self.assertRaises(device.DeviceObservationError):
            self.collect()

    def test_explicit_mapping_and_exact_uuid_namespace_produce_index_zero(self):
        with mock.patch.dict(os.environ, {"CUDA_VISIBLE_DEVICES": UUID}, clear=True):
            observation = self.observe()
        self.assertEqual(observation.cuda_index, 0)
        self.assertEqual(observation.cuda_visible_uuids, (UUID,))
        self.assertEqual(observation.hardware_fingerprint, "hardware-v1")
        self.assertEqual(observation.binding_id, self.public["binding_id"])

    # T14 T22 T37: the enrolled name may differ from the kernel hostname;
    # the frozen inventory digest still binds the physical host and boot.
    def test_enrolled_alias_uses_the_verified_physical_inventory(self):
        self.public["device"]["host_id"] = "gpu-box"
        policy = replace(self.policy(), host_id="gpu-box")
        with mock.patch.dict(os.environ, {"CUDA_VISIBLE_DEVICES": UUID}, clear=True):
            observed = self.observe(policy)
            self.assertEqual(observed.host_id, "gpu-box")
            changed = device._collect_inventory(self.proc, self.sys, "other-host", "aarch64")
            with mock.patch.object(device, "collect_inventory", return_value=changed):
                with self.assertRaises(device.DeviceObservationError):
                    device.observe_placement(self.build(), policy)

    def test_absent_ordinal_partial_or_multiple_namespace_denied_without_mutation(self):
        for namespace in (None, "", "0", "GPU-09631200", UUID + ",0", " " + UUID):
            env = {} if namespace is None else {"CUDA_VISIBLE_DEVICES": namespace}
            with mock.patch.dict(os.environ, env, clear=True):
                with self.assertRaises(device.DeviceObservationError):
                    self.observe()
                self.assertEqual(dict(os.environ), env)

    def test_mapping_must_match_launch_and_fresh_inventory(self):
        policy = self.policy()
        with mock.patch.dict(os.environ, {"CUDA_VISIBLE_DEVICES": UUID}, clear=True):
            for key in ("host_id", "hardware_fingerprint", "device_id", "memory_domain",
                        "physical_gpu_uuid", "inventory_digest"):
                with self.subTest(key=key), self.assertRaises(device.DeviceObservationError):
                    self.observe(replace(policy, **{key: "wrong"}))
            (self.pci / "device").write_text("0x1234\n")
            with self.assertRaises(device.DeviceObservationError):
                self.observe(policy)

    def test_error_and_repr_do_not_disclose_private_launch(self):
        with mock.patch.dict(os.environ, {}, clear=True):
            with self.assertRaises(device.DeviceObservationError) as caught:
                self.observe()
        self.assertEqual(str(caught.exception), "device_observation_denied")
        self.assertNotIn(UUID, repr(self.policy()))

    def test_duplicate_physical_identity_and_excess_inventory_denied(self):
        other = self.info.parent.parent / "000f:02:00.0"
        other.mkdir()
        (other / "information").write_text(self.info.read_text().replace(BDF, other.name))
        pci = self.pci.parent / other.name
        pci.mkdir()
        (pci / "vendor").write_text("0x10de\n")
        (pci / "device").write_text("0x2e12\n")
        with self.assertRaises(device.DeviceObservationError):
            self.collect()
        for domain in range(257):
            (self.info.parent.parent / f"{domain:04x}:03:00.0").mkdir()
        with self.assertRaises(device.DeviceObservationError):
            self.collect()

    def test_inventory_or_namespace_race_denied(self):
        policy = self.policy()
        def changing_collector():
            inventory = self.collect()
            (self.pci / "device").write_text("0x1234\n")
            return inventory
        with mock.patch.dict(os.environ, {"CUDA_VISIBLE_DEVICES": UUID}, clear=True):
            with mock.patch.object(device, "collect_inventory", changing_collector):
                with self.assertRaises(device.DeviceObservationError):
                    device.observe_placement(self.build(), policy)
            (self.pci / "device").write_text("0x2e12\n")
            def namespace_change():
                inventory = self.collect()
                os.environ["CUDA_VISIBLE_DEVICES"] = "0"
                return inventory
            with mock.patch.object(device, "collect_inventory", namespace_change):
                with self.assertRaises(device.DeviceObservationError):
                    device.observe_placement(self.build(), policy)

    def test_import_does_not_read_devices_or_import_native_packages(self):
        before = set(sys.modules)
        with mock.patch("os.open", side_effect=AssertionError("device read on import")):
            importlib.reload(device)
        self.assertFalse(any(name.split(".")[0] in {"torch", "sglang", "pynvml"}
                             for name in set(sys.modules) - before))

    def test_public_collector_has_no_caller_selected_paths(self):
        with self.assertRaises(TypeError):
            device.collect_inventory(self.proc, self.sys)

    def test_host_probe_failure_is_sanitized(self):
        with mock.patch("socket.gethostname", side_effect=OSError("private-host")):
            with self.assertRaises(device.DeviceObservationError) as caught:
                device.collect_inventory()
        self.assertEqual(str(caught.exception), "device_observation_denied")

    def test_boot_publication_prints_the_versioned_digest_and_devices(self):
        import io
        inventory = self.collect()
        stream = io.StringIO()
        with mock.patch.object(device, "collect_inventory", self.collect):
            device.publish_inventory(stream)
        published = json.loads(stream.getvalue())
        self.assertEqual(published["schema"], "mllm-nvidia-inventory-v1")
        self.assertEqual(published["digest"], inventory.digest)
        self.assertEqual(
            published["devices"],
            [{"physical_gpu_uuid": UUID, "pci_address": BDF, "device_minor": 0,
              "vendor_id": "0x10de", "device_id": "0x2e12"}])

    def test_boot_publication_refusal_prints_nothing(self):
        import io
        stream = io.StringIO()
        with mock.patch.object(
                device, "collect_inventory",
                mock.Mock(side_effect=device.DeviceObservationError())):
            with self.assertRaises(device.DeviceObservationError):
                device.publish_inventory(stream)
        self.assertEqual(stream.getvalue(), "")

    def test_main_module_refusal_exits_closed_with_an_empty_stdout(self):
        import io
        import runpy
        stdout, stderr = io.StringIO(), io.StringIO()
        # The probe seam is global, so a fresh module execution under runpy
        # reaches the same sanitized refusal on any host, GPU or not.
        with mock.patch("socket.gethostname", side_effect=OSError("private-host")):
            with mock.patch("sys.stdout", stdout), mock.patch("sys.stderr", stderr):
                with self.assertRaises(SystemExit) as caught:
                    runpy.run_module("runtime.sglang_device", run_name="__main__")
        self.assertEqual(caught.exception.code, 1)
        self.assertEqual(stdout.getvalue(), "")
        self.assertIn("device_observation_denied", stderr.getvalue())
