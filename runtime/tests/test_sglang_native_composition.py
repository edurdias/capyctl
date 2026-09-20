"""Synthetic composition tests; no GPU, no engine import, no qualification evidence."""

import builtins
import dataclasses
from dataclasses import replace
import json
import os
from pathlib import Path
import socket
import stat
import struct
import sys
import tempfile
import time
import unittest
from unittest import mock

from runtime import checkpoint_preflight as preflight
from runtime import sglang_device as device
from runtime import sglang_native_composition as composition
from runtime import sglang_saver_binding as saver
from runtime import sglang_server_args as server_args
from runtime import sglang_source_preflight as source
from runtime import sglang_startup_guards as guards
from test_sglang_entry import LaunchFixture
from test_sglang_observation_transport import BridgeFixture


UUID = "GPU-09631200-fdff-a345-295f-a1a6f84b2f84"
PACKAGE_ROOT = "/trusted/sglang/srt"
DIGEST = "a" * 64
NATIVE_ROOTS = ("sglang", "torch", "transformers", "torch_memory_saver")


def mapping(digest=DIGEST):
    return device.TrustedDeviceMapping(
        host_id="host-a", hardware_fingerprint="hardware-v1", device_id="gpu0",
        memory_domain="uma", physical_gpu_uuid=UUID, inventory_digest=digest)


class GatePatch:
    """Patch every composition gate with a recorder; overrides replace behavior."""

    def __init__(self, **overrides):
        self.order = []
        self.calls = {
            "verify_sglang_sources": lambda root: self.order.append("verify"),
            "revalidate_sglang_sources": lambda previous: self.order.append("revalidate"),
            "enforce_closed_plugins": lambda: self.order.append("plugins"),
            "observe_placement": lambda spec, trusted: self.order.append("placement"),
            "revalidate_checkpoint": lambda previous: self.order.append("checkpoint"),
        }
        self.calls.update(overrides)
        self._patches = None

    def __enter__(self):
        self._patches = [mock.patch.object(composition, name, value)
                         for name, value in self.calls.items()]
        for patch in self._patches:
            patch.start()
        return self

    def __exit__(self, *exception):
        for patch in reversed(self._patches):
            patch.stop()


class CompositionTests(LaunchFixture, unittest.TestCase):
    def setUp(self):
        super().setUp()
        self.spec = self.build()

    def compose(self, checkpoint=None, **changes):
        values = dict(package_root=PACKAGE_ROOT, trusted_mapping=None, placement_digest=None)
        values.update(changes)
        return composition.compose(self.spec, object() if checkpoint is None else checkpoint,
                                   **values)

    def test_failing_placement_keeps_gate_order_and_stops_before_checkpoint(self):
        def failing_placement(spec, trusted):
            gates.order.append("placement")
            raise device.DeviceObservationError()

        with GatePatch(observe_placement=failing_placement) as gates:
            with self.assertRaises(composition.NativeCompositionError) as caught:
                self.compose(trusted_mapping=mapping(), placement_digest=DIGEST)
        self.assertEqual(caught.exception.code, "placement_failed")
        self.assertEqual(gates.order, ["verify", "revalidate", "plugins", "placement"])

    def test_every_gate_failure_maps_to_its_closed_code_only(self):
        def source_failure(root):
            raise source.SourcePreflightError("artifact_changed")

        def plugin_failure():
            raise guards.StartupGuardError("external_plugins_present")

        def placement_failure(spec, trusted):
            raise device.DeviceObservationError()

        def checkpoint_failure(previous):
            raise preflight.CheckpointPreflightError("artifact_changed")

        cases = (("source_revalidation_failed", {"verify_sglang_sources": source_failure}),
                 ("plugin_closure_failed", {"enforce_closed_plugins": plugin_failure}),
                 ("placement_failed", {"observe_placement": placement_failure}),
                 ("checkpoint_revalidation_failed", {"revalidate_checkpoint": checkpoint_failure}))
        for expected, overrides in cases:
            with self.subTest(expected=expected), GatePatch(**overrides):
                with self.assertRaises(composition.NativeCompositionError) as caught:
                    self.compose(trusted_mapping=mapping(), placement_digest=DIGEST)
            self.assertEqual(caught.exception.code, expected)
            self.assertEqual(str(caught.exception), expected)
            self.assertNotIn(PACKAGE_ROOT, str(caught.exception))

    def test_asserted_digest_without_trusted_mapping_or_mismatched_is_placement_failure(self):
        with GatePatch() as gates:
            with self.assertRaises(composition.NativeCompositionError) as caught:
                self.compose(trusted_mapping=None, placement_digest=DIGEST)
            self.assertEqual(caught.exception.code, "placement_failed")
            self.assertEqual(gates.order, ["verify", "revalidate", "plugins"])
        with GatePatch() as gates:
            with self.assertRaises(composition.NativeCompositionError) as caught:
                self.compose(trusted_mapping=mapping(), placement_digest="b" * 64)
            self.assertEqual(caught.exception.code, "placement_failed")
            self.assertEqual(gates.order, ["verify", "revalidate", "plugins"])

    def test_success_contract_records_all_facts_and_explicit_unasserted_placement(self):
        sentinel = object()

        def returning(previous):
            gates.order.append("checkpoint")
            return sentinel

        with GatePatch(revalidate_checkpoint=returning) as gates:
            contract = self.compose(checkpoint=sentinel)
        self.assertEqual(gates.order, ["verify", "revalidate", "plugins", "checkpoint"])
        self.assertTrue(contract.sources_ok)
        self.assertTrue(contract.plugins_closed)
        self.assertFalse(contract.placement_asserted)
        self.assertIsNone(contract.placement_digest)
        self.assertIsNone(contract.placement)
        self.assertIs(contract.checkpoint, sentinel)
        self.assertEqual(contract.binding_id, self.public["binding_id"])
        self.assertEqual(contract.incarnation, self.public["incarnation"])
        self.assertLessEqual(contract.observed_at, time.time())
        self.assertNotIn(PACKAGE_ROOT, repr(contract))
        with self.assertRaises(dataclasses.FrozenInstanceError):
            contract.sources_ok = False

    def test_asserted_placement_carries_authorized_digest_and_observation(self):
        placement = server_args.ObservedPlacement(
            binding_id=self.public["binding_id"], incarnation=self.public["incarnation"],
            **self.public["device"], physical_gpu_uuid=UUID,
            cuda_visible_uuids=(UUID,), cuda_index=0)

        def returning(spec, trusted):
            return placement

        with GatePatch(observe_placement=returning):
            contract = self.compose(trusted_mapping=mapping(), placement_digest=DIGEST)
        self.assertTrue(contract.placement_asserted)
        self.assertEqual(contract.placement_digest, DIGEST)
        self.assertIs(contract.placement, placement)
        self.assertNotIn(UUID, repr(contract))

    def test_compose_never_imports_native_modules(self):
        original = builtins.__import__

        def guarded(name, *args, **kwargs):
            if name.split(".")[0] in NATIVE_ROOTS:
                raise AssertionError("native import during composition")
            return original(name, *args, **kwargs)

        with mock.patch("builtins.__import__", side_effect=guarded), GatePatch():
            self.compose()
        self.assertFalse(any(name.split(".")[0] in NATIVE_ROOTS for name in sys.modules))


class EnrollmentTests(unittest.TestCase):
    def setUp(self):
        # /tmp is not a protected installation ancestor; service home only.
        self.directory = tempfile.TemporaryDirectory(prefix="mllm-compose-obs-", dir=Path.home())
        self.addCleanup(self.directory.cleanup)
        os.chmod(self.directory.name, 0o700)
        self.identity = saver.current_process_identity()
        self.bridge = BridgeFixture(self.identity)
        self.contract = composition.NativeContract(
            sources_ok=True, plugins_closed=True, placement_asserted=False,
            placement_digest=None, placement=None, checkpoint=None,
            binding_id="binding-1", incarnation="incarnation-1", observed_at=time.time())

    def handle(self, **changes):
        values = dict(bridge=self.bridge, scheduler_identity=self.identity,
                      controller_identity=self.identity, socket_dir=self.directory.name)
        values.update(changes)
        return composition.enroll_and_observe(self.contract, **values)

    def test_attach_creates_owned_socket_and_close_removes_it(self):
        handle = self.handle()
        try:
            path = Path(handle.socket_path)
            self.assertEqual(path.parent, Path(self.directory.name))
            info = path.stat()
            self.assertTrue(stat.S_ISSOCK(info.st_mode))
            self.assertEqual(stat.S_IMODE(info.st_mode), 0o600)
            self.assertNotIn(str(path), repr(handle))
        finally:
            handle.close()
        self.assertFalse(path.exists())
        self.assertEqual(stat.S_IMODE(os.stat(self.directory.name).st_mode), 0o700)

    def test_wrong_scheduler_identity_is_refused_before_socket_creation(self):
        wrong = replace(self.identity, start_ticks=self.identity.start_ticks + 1)
        with self.assertRaises(composition.NativeCompositionError) as caught:
            self.handle(scheduler_identity=wrong)
        self.assertEqual(caught.exception.code, "observation_attach_failed")
        self.assertEqual(str(caught.exception), "observation_attach_failed")
        self.assertFalse((Path(self.directory.name) / "observe.sock").exists())

    def test_missing_service_directory_is_attach_failure(self):
        absent = str(Path(self.directory.name) / "absent")
        with self.assertRaises(composition.NativeCompositionError) as caught:
            self.handle(socket_dir=absent)
        self.assertEqual(caught.exception.code, "observation_attach_failed")
        self.assertFalse(Path(absent).exists())

    def test_served_observation_flows_through_the_attached_transport(self):
        handle = self.handle()
        try:
            client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            client.settimeout(3)
            self.addCleanup(client.close)
            client.connect(handle.socket_path)
            raw = json.dumps(dict(version=1, request_id="read-1", timeout_ms=1000)).encode()
            client.sendall(struct.pack("!I", len(raw)) + raw)
            result = bytearray()
            while True:
                chunk = client.recv(65540)
                if not chunk:
                    break
                result.extend(chunk)
            value = json.loads(bytes(result[4:]))
            self.assertEqual(value["status"], "observed")
            self.assertEqual(value["binding_id"], "binding-1")
            self.assertEqual(value["incarnation_id"], "incarnation-1")
        finally:
            handle.close()
        self.assertFalse(Path(handle.socket_path).exists())


if __name__ == "__main__":
    unittest.main()
