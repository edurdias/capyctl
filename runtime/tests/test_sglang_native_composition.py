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

from runtime import sglang_device as device
from runtime import sglang_native_composition as composition
from runtime import sglang_saver_binding as saver
from runtime import sglang_server_args as server_args
from runtime import sglang_startup_guards as guards
from test_sglang_entry import LaunchFixture
from test_sglang_observation_transport import BridgeFixture


UUID = "GPU-09631200-fdff-a345-295f-a1a6f84b2f84"
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
            "enforce_closed_plugins": lambda: self.order.append("plugins"),
            "observe_placement": lambda spec, trusted: self.order.append("placement"),
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

    def compose(self, **changes):
        values = dict(trusted_mapping=None, placement_digest=None)
        values.update(changes)
        return composition.compose(self.spec, **values)

    def test_failing_placement_keeps_gate_order(self):
        def failing_placement(spec, trusted):
            gates.order.append("placement")
            raise device.DeviceObservationError()

        with GatePatch(observe_placement=failing_placement) as gates:
            with self.assertRaises(composition.NativeCompositionError) as caught:
                self.compose(trusted_mapping=mapping(), placement_digest=DIGEST)
        self.assertEqual(caught.exception.code, "placement_failed")
        self.assertEqual(gates.order, ["plugins", "placement"])

    def test_every_gate_failure_maps_to_its_closed_code_only(self):
        def plugin_failure():
            raise guards.StartupGuardError("external_plugins_present")

        def placement_failure(spec, trusted):
            raise device.DeviceObservationError()

        cases = (("plugin_closure_failed", {"enforce_closed_plugins": plugin_failure}),
                 ("placement_failed", {"observe_placement": placement_failure}))
        for expected, overrides in cases:
            with self.subTest(expected=expected), GatePatch(**overrides):
                with self.assertRaises(composition.NativeCompositionError) as caught:
                    self.compose(trusted_mapping=mapping(), placement_digest=DIGEST)
            self.assertEqual(caught.exception.code, expected)
            self.assertEqual(str(caught.exception), expected)

    def test_asserted_digest_without_trusted_mapping_or_mismatched_is_placement_failure(self):
        with GatePatch() as gates:
            with self.assertRaises(composition.NativeCompositionError) as caught:
                self.compose(trusted_mapping=None, placement_digest=DIGEST)
            self.assertEqual(caught.exception.code, "placement_failed")
            self.assertEqual(gates.order, ["plugins"])
        with GatePatch() as gates:
            with self.assertRaises(composition.NativeCompositionError) as caught:
                self.compose(trusted_mapping=mapping(), placement_digest="b" * 64)
            self.assertEqual(caught.exception.code, "placement_failed")
            self.assertEqual(gates.order, ["plugins"])

    def test_success_contract_records_all_facts_and_explicit_unasserted_placement(self):
        # ADR 0014 §7: no checkpoint gate remains here; the host agent's
        # digest check (WE3) owns checkpoint identity. ADR 0008: no source
        # audit either; installation internals are probed by shape later.
        with GatePatch() as gates:
            contract = self.compose()
        self.assertEqual(gates.order, ["plugins"])
        self.assertFalse(hasattr(contract, "checkpoint"))
        self.assertFalse(hasattr(contract, "sources_ok"))
        self.assertTrue(contract.plugins_closed)
        self.assertFalse(contract.placement_asserted)
        self.assertIsNone(contract.placement_digest)
        self.assertIsNone(contract.placement)
        self.assertEqual(contract.binding_id, self.public["binding_id"])
        self.assertEqual(contract.incarnation, self.public["incarnation"])
        self.assertLessEqual(contract.observed_at, time.time())
        with self.assertRaises(dataclasses.FrozenInstanceError):
            contract.plugins_closed = False

    # T21 T22: ADR 0008 (owner decision 2026-09-23). No gate audits the
    # installation's files: composition takes no package root and the pinned
    # source audit modules are gone, so a custom build is not refused here.
    def test_no_gate_audits_installation_files(self):
        import importlib.util
        import inspect
        self.assertNotIn("package_root", inspect.signature(composition.compose).parameters)
        for retired in ("runtime.sglang_source_preflight", "runtime.saver_source_preflight"):
            self.assertIsNone(importlib.util.find_spec(retired))
        self.assertNotIn("source_revalidation_failed", composition._CODES)

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

        before = set(sys.modules)
        with mock.patch("builtins.__import__", side_effect=guarded), GatePatch():
            self.compose()
        added = set(sys.modules) - before
        self.assertEqual([name for name in added
                          if name.split(".")[0] in NATIVE_ROOTS], [])


class EnrollmentTests(unittest.TestCase):
    def setUp(self):
        # /tmp is not a protected installation ancestor; service home only.
        self.directory = tempfile.TemporaryDirectory(prefix="mllm-compose-obs-", dir=Path.home())
        self.addCleanup(self.directory.cleanup)
        os.chmod(self.directory.name, 0o700)
        self.identity = saver.current_process_identity()
        self.bridge = BridgeFixture(self.identity)
        self.contract = composition.NativeContract(
            plugins_closed=True, placement_asserted=False,
            placement_digest=None, placement=None,
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
