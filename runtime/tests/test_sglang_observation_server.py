"""CPU-only listener custody and real transport composition tests."""
import dataclasses
import importlib
import json
import os
from pathlib import Path
import socket
import stat
import struct
import tempfile
import threading
import unittest
from unittest import mock

from runtime import sglang_saver_binding as saver
from test_sglang_observation_transport import BridgeFixture


class ObservationServerTests(unittest.TestCase):
    def setUp(self):
        self.assertIsNotNone(importlib.util.find_spec("runtime.sglang_observation_server"),
                             "protected scheduler observation listener is not implemented")
        self.module = importlib.import_module("runtime.sglang_observation_server")
        # The checkout may be group-writable. Never relax custody or chmod it.
        self.directory = tempfile.TemporaryDirectory(prefix="obs-", dir=Path.home())
        self.addCleanup(self.directory.cleanup)
        self.path = Path(self.directory.name) / "o"
        self.owner = saver.current_process_identity()
        self.bridge = BridgeFixture(self.owner)

    def server(self, **changes):
        values = dict(path=str(self.path), bridge=self.bridge, binding_id="binding-1",
                      incarnation_id="incarnation-1", expected_owner=self.owner,
                      expected_peer=self.owner)
        values.update(changes)
        return self.module.SchedulerObservationServer.start(**values)

    def request(self, request_id="read-1"):
        client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.addCleanup(client.close)
        client.settimeout(3)
        client.connect(str(self.path))
        raw = json.dumps(dict(version=1, request_id=request_id, timeout_ms=1000)).encode()
        client.sendall(struct.pack("!I", len(raw)) + raw)
        return client

    def read(self, client):
        result = bytearray()
        while True:
            try:
                chunk = client.recv(65540)
            except ConnectionResetError:
                break
            if not chunk:
                break
            result.extend(chunk)
        if not result:
            return None
        self.assertEqual(struct.unpack("!I", result[:4])[0], len(result) - 4)
        return json.loads(result[4:])

    def test_protected_listener_serves_real_transport_and_removes_only_owned_socket(self):
        server = self.server()
        self.addCleanup(server.close)
        self.assertEqual(stat.S_IMODE(self.path.stat().st_mode), 0o600)
        value = self.read(self.request())
        self.assertEqual(value["status"], "observed")
        self.assertEqual(value["observation"]["allocations"]["mapped_bytes"], 4096)
        self.assertEqual(len(self.bridge.calls), 1)
        # The retained transport replay fence survives successive connections.
        self.assertIsNone(self.read(self.request()))
        self.assertEqual(len(self.bridge.calls), 1)
        self.assertEqual(self.read(self.request("read-2"))["request_id"], "read-2")
        server.close()
        self.assertFalse(self.path.exists())

    def test_transport_activity_covers_custody_and_clears_after_success_or_denial(self):
        # T20/T21/T22: fairness spans authentication too, without trusting the peer.
        server = self.server()
        self.addCleanup(server.close)
        checks = []
        cleared = threading.Event()
        original_clear = self.bridge.transport_active.clear
        def clear():
            original_clear()
            cleared.set()
        original = server._custody.check
        def check():
            checks.append(self.bridge.transport_active.is_set())
            return original()
        with mock.patch.object(server._custody, "check", check), mock.patch.object(
                self.bridge.transport_active, "clear", clear):
            self.assertEqual(self.read(self.request())["status"], "observed")
            self.assertTrue(cleared.wait(2))
            cleared.clear()
            self.assertFalse(self.bridge.transport_active.is_set())
            self.assertIsNone(self.read(self.request()))  # Replay denied.
            self.assertTrue(cleared.wait(2))
            self.assertFalse(self.bridge.transport_active.is_set())
        self.assertEqual(checks, [True, True])

    def test_existing_paths_and_unprotected_parent_are_never_repaired(self):
        self.path.write_text("retain me")
        with self.assertRaises(self.module.ObservationServerError):
            self.server()
        self.assertEqual(self.path.read_text(), "retain me")
        self.path.unlink()
        os.chmod(self.directory.name, 0o755)
        with self.assertRaises(self.module.ObservationServerError):
            self.server()
        self.assertFalse(self.path.exists())
        self.assertEqual(stat.S_IMODE(os.stat(self.directory.name).st_mode), 0o755)

    def test_wrong_scheduler_identity_is_rejected_before_socket_creation(self):
        with self.assertRaises(self.module.ObservationServerError):
            self.server(expected_owner=dataclasses.replace(self.owner, start_ticks=self.owner.start_ticks + 1))
        self.assertFalse(self.path.exists())
        self.assertEqual(self.bridge.calls, [])

    def test_close_interrupts_pending_transport_without_an_observed_result(self):
        self.bridge.wait = True
        server = self.server()
        self.addCleanup(server.close)
        client = self.request()
        self.assertTrue(self.bridge.requested.wait(2))
        self.assertTrue(self.bridge.transport_active.is_set())
        server.close()
        self.assertFalse(self.bridge.transport_active.is_set())
        value = self.read(client)
        self.assertTrue(value is None or value["status"] == "uncertain")
        self.assertIsNone(self.bridge.pending)
        self.assertFalse(self.path.exists())

    def test_foreign_peer_never_reaches_bridge(self):
        # The configured controller is a real different process, not an invented PID.
        peer = self.module._process_identity(os.getppid())
        server = self.server(expected_peer=peer)
        self.addCleanup(server.close)
        self.assertIsNone(self.read(self.request()))
        self.assertEqual(self.bridge.calls, [])

    def test_replaced_socket_path_is_retained_and_never_unlinked(self):
        server = self.server()
        parked_path = self.path.with_name("retained")
        self.path.rename(parked_path)
        self.path.write_text("not our socket")
        try:
            with self.assertRaises(self.module.ObservationServerError):
                server.close()
            self.assertEqual(self.path.read_text(), "not our socket")
        finally:
            self.path.unlink()
            parked_path.rename(self.path)
            server.close()
        self.assertFalse(self.path.exists())

    def test_aliases_and_changed_parent_permissions_deny_connections(self):
        alias = Path(self.directory.name) / "alias"
        alias.symlink_to(self.directory.name, target_is_directory=True)
        with self.assertRaises(self.module.ObservationServerError):
            self.server(path=str(alias / "o"))
        self.assertFalse(self.path.exists())
        server = self.server()
        try:
            os.chmod(self.directory.name, 0o755)
            self.assertIsNone(self.read(self.request()))
            self.assertEqual(self.bridge.calls, [])
            self.assertFalse(self.bridge.transport_active.is_set())
            with self.assertRaises(self.module.ObservationServerError):
                server.close()
        finally:
            os.chmod(self.directory.name, 0o700)
            server.close()

    def test_stalled_bridge_retains_custody_until_thread_really_finishes(self):
        entered = threading.Event()
        release = threading.Event()
        original = self.bridge.poll

        def stalled(request_id):
            entered.set()
            release.wait(10)
            return original(request_id)

        self.bridge.poll = stalled
        server = self.server()
        client = self.request()
        self.assertTrue(entered.wait(2))
        try:
            with self.assertRaises(self.module.ObservationServerError):
                server.close()
            self.assertTrue(self.path.exists())
        finally:
            release.set()
            server.close()
        self.assertIsNone(self.read(client))
        self.assertFalse(self.path.exists())


if __name__ == "__main__":
    unittest.main()
