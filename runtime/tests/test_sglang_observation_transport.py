"""CPU socketpair tests for authenticated, bounded observation transport."""
import dataclasses
import importlib
import json
import os
import socket
import struct
import subprocess
import threading
import time
import types
import unittest
from unittest import mock

from runtime import sglang_saver_binding as saver
from runtime import sglang_scheduler_observer as bridge_module
from runtime.memory_saver_observer import AllocationAggregate, SaverObservation


class BridgeFixture:
    """Replace only the scheduler/native boundary; socket and proc checks are real."""
    def __init__(self, owner):
        self.owner = owner
        self.calls = []
        self.pending = None
        self.cancelled = False
        self.wait = False
        self.mutate = lambda value: value
        self.requested = threading.Event()
        self.observation = saver.SchedulerSaverObservation(owner,
            saver.LoadedSaverLibrary("a" * 64, 1, 2, 3, 4, 5), "preload",
            SaverObservation((AllocationAggregate(0, "weights", 1, 1, 0, 4096, 4096, 0, 0),),
                             1, 4096, 4096, 0))

    def request(self, request_id, *, timeout_ms):
        if self.pending is not None:
            raise bridge_module.BridgeError("busy")
        self.calls.append((request_id, timeout_ms))
        self.pending = request_id
        self.cancelled = False
        self.requested.set()

    def poll(self, request_id):
        if self.pending != request_id:
            raise bridge_module.BridgeError("unknown")
        if self.wait and not self.cancelled:
            return None
        self.pending = None
        now = time.monotonic_ns()
        return self.mutate(bridge_module.ObservationResult("binding-1", "incarnation-1", request_id,
            self.owner, now, now, "uncertain" if self.cancelled else "observed",
            None if self.cancelled else self.observation))

    def cancel(self, request_id):
        if self.pending != request_id:
            raise bridge_module.BridgeError("unknown")
        self.cancelled = True


class ObservationTransportTests(unittest.TestCase):
    def setUp(self):
        self.assertIsNotNone(importlib.util.find_spec("runtime.sglang_observation_transport"),
                             "authenticated observation transport is not implemented")
        self.module = importlib.import_module("runtime.sglang_observation_transport")
        self.owner = saver.current_process_identity()
        self.bridge = BridgeFixture(self.owner)
        self.transport = self.make_transport()

    def make_transport(self, **changes):
        values = dict(bridge=self.bridge, binding_id="binding-1", incarnation_id="incarnation-1",
                      expected_owner=self.owner, expected_peer=self.owner)
        values.update(changes)
        return self.module.ObservationTransport(**values)

    def connect(self, transport=None):
        server, client = socket.socketpair()
        client.settimeout(3)
        self.addCleanup(client.close)
        outcomes = []
        thread = threading.Thread(target=lambda: outcomes.append((transport or self.transport).serve(server)))
        thread.start()
        self.addCleanup(thread.join, 3)
        return client, thread, outcomes

    def frame(self, request_id="request-1", timeout_ms=1000):
        data = json.dumps(dict(version=1, request_id=request_id, timeout_ms=timeout_ms)).encode()
        return struct.pack("!I", len(data)) + data

    def read_response(self, client):
        data = bytearray()
        while True:
            try:
                chunk = client.recv(65540)
            except ConnectionResetError:
                break
            if not chunk:
                break
            data.extend(chunk)
        if not data:
            return None
        self.assertGreaterEqual(len(data), 4)
        size = struct.unpack("!I", data[:4])[0]
        self.assertLessEqual(size, 65536)
        self.assertEqual(size, len(data) - 4)
        return json.loads(data[4:])

    def test_normal_observation_has_explicit_bounded_projection(self):
        client, thread, outcomes = self.connect()
        client.sendall(self.frame())
        response = self.read_response(client)
        thread.join(3)
        self.assertEqual(outcomes, ["observed"])
        self.assertEqual(response["status"], "observed")
        self.assertEqual(response["request_id"], "request-1")
        self.assertEqual(response["observation"]["allocations"]["mapped_bytes"], 4096)
        self.assertEqual(set(response), {"version", "binding_id", "incarnation_id", "request_id",
                                          "owner", "started_ns", "finished_ns", "status", "observation"})
        self.assertNotIn("ready", json.dumps(response).lower())
        self.assertEqual(len(self.bridge.calls), 1)

    def test_wrong_peer_pid_and_start_identity_never_request_observation(self):
        for peer in (dataclasses.replace(self.owner, pid=os.getppid()),
                     dataclasses.replace(self.owner, start_ticks=self.owner.start_ticks + 1),
                     dataclasses.replace(self.owner, boot_id="0" * 8 + "-0000-0000-0000-" + "0" * 12)):
            with self.subTest(peer=peer):
                client, thread, outcomes = self.connect(self.make_transport(expected_peer=peer))
                self.assertIsNone(self.read_response(client))
                thread.join(3)
                self.assertEqual(outcomes, ["denied"])
        self.assertEqual(self.bridge.calls, [])

    def test_wrong_own_identity_never_requests_observation(self):
        client, thread, outcomes = self.connect(self.make_transport(
            expected_owner=dataclasses.replace(self.owner, start_ticks=self.owner.start_ticks + 1)))
        self.assertIsNone(self.read_response(client))
        thread.join(3)
        self.assertEqual(outcomes, ["denied"])
        self.assertEqual(self.bridge.calls, [])

    def test_malformed_duplicate_unknown_oversize_requests_are_denied(self):
        bodies = [b'{"version":1,"version":1,"request_id":"x","timeout_ms":50}',
                  b'{"version":1,"request_id":"x","timeout_ms":true}',
                  b'{"version":1,"request_id":"x","timeout_ms":0}',
                  b'{"version":1,"request_id":"x","timeout_ms":2001}',
                  b'{"version":1,"request_id":"x","timeout_ms":50,"control":"release"}',
                  b'[]', b'{', b'\xff', b'{}', b'a' * 1025]
        for body in bodies:
            with self.subTest(body=body[:100]):
                client, thread, outcomes = self.connect()
                client.sendall(struct.pack("!I", len(body)) + body)
                self.assertIsNone(self.read_response(client))
                thread.join(3)
                self.assertEqual(outcomes, ["denied"])
        self.assertEqual(self.bridge.calls, [])

    def test_duplicate_id_never_replays_bridge(self):
        for expected in ("observed", "denied"):
            client, thread, outcomes = self.connect()
            client.sendall(self.frame())
            response = self.read_response(client)
            thread.join(3)
            self.assertEqual(outcomes, [expected])
            self.assertEqual(response is None, expected == "denied")
        self.assertEqual(len(self.bridge.calls), 1)

    def test_disconnect_and_extra_input_cancel_and_consume_before_reuse(self):
        self.bridge.wait = True
        for index, disconnect in enumerate((True, False)):
            self.bridge.requested.clear()
            client, thread, outcomes = self.connect()
            client.sendall(self.frame(f"pending-{index}"))
            self.assertTrue(self.bridge.requested.wait(1))
            if disconnect:
                client.close()
            else:
                client.sendall(b"extra")
                self.assertIsNone(self.read_response(client))
            thread.join(3)
            self.assertEqual(outcomes, ["uncertain"])
            self.assertTrue(self.bridge.cancelled)
            self.assertIsNone(self.bridge.pending)
        self.bridge.wait = False
        client, thread, outcomes = self.connect()
        client.sendall(self.frame("next"))
        self.assertEqual(self.read_response(client)["status"], "observed")

    def test_pending_timeout_cancels_and_consumes(self):
        self.bridge.wait = True
        start = time.monotonic()
        client, thread, outcomes = self.connect()
        client.sendall(self.frame(timeout_ms=30))
        self.read_response(client)
        thread.join(3)
        self.assertLess(time.monotonic() - start, 0.5)
        self.assertEqual(outcomes, ["uncertain"])
        self.assertTrue(self.bridge.cancelled)
        self.assertIsNone(self.bridge.pending)

    def test_mismatched_correlation_and_native_errors_cannot_escape(self):
        mutations = [lambda value: dataclasses.replace(value, request_id="other"),
                     lambda value: dataclasses.replace(value, binding_id="other"),
                     lambda value: dataclasses.replace(value, incarnation_id="other"),
                     lambda value: dataclasses.replace(value, owner=dataclasses.replace(self.owner, pid=1)),
                     lambda value: dataclasses.replace(value, status="ready"),
                     lambda value: dataclasses.replace(value, finished_ns=0)]
        for index, mutate in enumerate(mutations):
            self.bridge.mutate = mutate
            client, thread, outcomes = self.connect()
            client.sendall(self.frame(f"mismatch-{index}"))
            response = self.read_response(client)
            thread.join(3)
            self.assertEqual(outcomes, ["uncertain"])
            self.assertEqual(response["status"], "uncertain")
            self.assertIsNone(response["observation"])
            self.assertEqual(response["request_id"], f"mismatch-{index}")

    def test_unknown_tags_and_oversize_result_fail_whole_observation(self):
        group = self.bridge.observation.allocations.groups[0]
        cases = [(dataclasses.replace(group, tag="/private/secret-0x1234"),),
                 (dataclasses.replace(group, tag="cuda_graph"),),
                 tuple(dataclasses.replace(group, device=index) for index in range(1000))]
        for index, groups in enumerate(cases):
            self.bridge.observation = dataclasses.replace(self.bridge.observation,
                allocations=SaverObservation(groups, len(groups), 4096 * len(groups), 4096 * len(groups), 0))
            client, thread, outcomes = self.connect()
            client.sendall(self.frame(f"redaction-{index}"))
            response = self.read_response(client)
            thread.join(3)
            self.assertEqual(outcomes, ["uncertain"])
            self.assertIsNone(response["observation"])
            self.assertNotIn("private", json.dumps(response))

    def test_simultaneous_connection_denied_without_disturbing_pending_request(self):
        self.bridge.wait = True
        first, thread, outcomes = self.connect()
        first.sendall(self.frame("first"))
        self.assertTrue(self.bridge.requested.wait(1))
        second, second_thread, second_outcomes = self.connect()
        self.assertIsNone(self.read_response(second))
        second_thread.join(3)
        self.assertEqual(second_outcomes, ["denied"])
        self.assertEqual(self.bridge.pending, "first")
        first.close()
        thread.join(3)

    def test_partial_slow_request_has_total_two_second_bound(self):
        client, thread, outcomes = self.connect()
        start = time.monotonic()
        client.sendall(b"\x00")
        self.assertIsNone(self.read_response(client))
        thread.join(3)
        self.assertLess(time.monotonic() - start, 2.5)
        self.assertEqual(outcomes, ["denied"])
        self.assertEqual(self.bridge.calls, [])

    # T21 T16: SPEC §9.2. The replay fence is a sliding window over the most
    # recent 4096 correlation IDs: a recent ID is never served twice, and a
    # long-lived launch keeps answering new observations (a fence that never
    # drained refused every Park once 4096 observations had been made).
    def test_replay_window_slides_over_recent_ids(self):
        for index in range(4096):
            client, thread, outcomes = self.connect()
            client.sendall(self.frame(f"id-{index}"))
            self.assertEqual(self.read_response(client)["status"], "observed")
            thread.join(3)
            client.close()
        client, thread, outcomes = self.connect()
        client.sendall(self.frame("id-4095"))
        self.assertIsNone(self.read_response(client))
        thread.join(3)
        self.assertEqual(outcomes, ["denied"])
        for request_id in ("new-id", "newer-id"):
            client, thread, outcomes = self.connect()
            client.sendall(self.frame(request_id))
            self.assertEqual(self.read_response(client)["status"], "observed")
            thread.join(3)
            client.close()
        # The newest IDs are inside the window and stay refused.
        client, thread, outcomes = self.connect()
        client.sendall(self.frame("new-id"))
        self.assertIsNone(self.read_response(client))
        thread.join(3)
        self.assertEqual(outcomes, ["denied"])
        self.assertEqual(len(self.bridge.calls), 4098)

    def test_peer_identity_rechecked_after_result_before_output(self):
        actual = self.module._process_identity
        def observed_identity(pid):
            identity = actual(pid)
            return dataclasses.replace(identity, start_ticks=identity.start_ticks + 1) if self.bridge.calls else identity
        with mock.patch.object(self.module, "_process_identity", observed_identity):
            client, thread, outcomes = self.connect()
            client.sendall(self.frame())
            self.assertIsNone(self.read_response(client))
            thread.join(3)
        self.assertEqual(outcomes, ["uncertain"])
        self.assertEqual(len(self.bridge.calls), 1)

    def test_service_uid_change_is_denied(self):
        with mock.patch.object(self.module.os, "geteuid", return_value=os.getuid() + 1):
            client, thread, outcomes = self.connect()
            self.assertIsNone(self.read_response(client))
            thread.join(3)
        self.assertEqual(outcomes, ["denied"])
        self.assertEqual(self.bridge.calls, [])

    def test_broken_cancel_consumption_poison_disallows_new_requests(self):
        self.bridge.wait = True
        original_poll = self.bridge.poll
        def broken_poll(request_id):
            value = original_poll(request_id)
            return dataclasses.replace(value, request_id="wrong") if value else None
        self.bridge.poll = broken_poll
        client, thread, outcomes = self.connect()
        client.sendall(self.frame(timeout_ms=20))
        self.read_response(client)
        thread.join(3)
        self.assertEqual(outcomes, ["uncertain"])
        self.bridge.wait = False
        client, thread, outcomes = self.connect()
        try:
            client.sendall(self.frame("next"))
        except BrokenPipeError:
            pass
        self.assertIsNone(self.read_response(client))
        thread.join(3)
        self.assertEqual(outcomes, ["denied"])
        self.assertEqual(len(self.bridge.calls), 1)

    def test_bridge_exceptions_are_sanitized_and_cancelled(self):
        original_poll = self.bridge.poll
        def failing_poll(request_id):
            if not self.bridge.cancelled:
                raise RuntimeError("private path /secrets/token 0x1234")
            return original_poll(request_id)
        self.bridge.poll = failing_poll
        client, thread, outcomes = self.connect()
        client.sendall(self.frame())
        response = self.read_response(client)
        thread.join(3)
        self.assertEqual(outcomes, ["uncertain"])
        self.assertEqual(response["status"], "uncertain")
        self.assertNotIn("secret", json.dumps(response))
        self.assertIsNone(self.bridge.pending)

    def test_real_bridge_safe_point_runs_elsewhere_and_emits_one_result(self):
        requested = threading.Event()
        class SignalledBridge(bridge_module.SchedulerObserverBridge):
            def request(self, request_id, *, timeout_ms):
                super().request(request_id, timeout_ms=timeout_ms)
                requested.set()
        scheduler = types.SimpleNamespace(server_args=types.SimpleNamespace(
            tp_size=1, dp_size=1, pp_size=1, ep_size=1, dcp_size=1, attn_cp_size=1, moe_dp_size=1,
            enable_dp_attention=False, enable_dp_lm_head=False, enable_prefill_cp=False,
            speculative_algorithm=None, disaggregation_mode="null"))
        bridge = SignalledBridge(scheduler, "binding-1", "incarnation-1", self.owner,
                                 saver.TrustedSaverBuild("/private/never-loaded.so", "a" * 64, "preload"))
        transport = self.make_transport(bridge=bridge)
        with mock.patch.object(bridge_module, "observe_scheduler_saver", return_value=self.bridge.observation):
            client, thread, outcomes = self.connect(transport)
            client.sendall(self.frame())
            self.assertTrue(requested.wait(1))
            bridge._tick()
            response = self.read_response(client)
            thread.join(3)
        self.assertEqual(outcomes, ["observed"])
        self.assertEqual(response["observation"]["allocations"]["allocation_count"], 1)
        with self.assertRaises(bridge_module.BridgeError):
            bridge.poll("request-1")

    def test_slow_reader_never_reports_partial_frame_as_success(self):
        group = self.bridge.observation.allocations.groups[0]
        groups = tuple(dataclasses.replace(group, device=index) for index in range(100))
        self.bridge.observation = dataclasses.replace(self.bridge.observation,
            allocations=SaverObservation(groups, 100, 409600, 409600, 0))
        server, client = socket.socketpair()
        server.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 1024)
        client.settimeout(1)
        self.addCleanup(client.close)
        outcomes = []
        thread = threading.Thread(target=lambda: outcomes.append(self.transport.serve(server)))
        thread.start()
        client.sendall(self.frame(timeout_ms=30))
        thread.join(1)
        self.assertFalse(thread.is_alive())
        self.assertEqual(outcomes, ["uncertain"])
        partial = bytearray()
        while chunk := client.recv(65540):
            partial.extend(chunk)
        self.assertGreaterEqual(len(partial), 4)
        self.assertLess(len(partial) - 4, struct.unpack("!I", partial[:4])[0])
        self.assertIsNone(self.bridge.pending)

    def test_zero_allocation_result_has_no_authority_or_future_fields(self):
        self.bridge.observation = dataclasses.replace(self.bridge.observation,
            allocations=SaverObservation((), 0, 0, 0, 0))
        object.__setattr__(self.bridge.observation, "future_path", "/secret/new-field")
        object.__setattr__(self.bridge.observation.library, "raw_address", "0xdeadbeef")
        client, thread, outcomes = self.connect()
        client.sendall(self.frame())
        response = self.read_response(client)
        thread.join(3)
        self.assertEqual(outcomes, ["observed"])
        self.assertEqual(response["observation"]["allocations"],
                         dict(groups=[], allocation_count=0, virtual_bytes=0, mapped_bytes=0, backup_bytes=0))
        encoded = json.dumps(response)
        for value in ("future_path", "secret", "raw_address", "deadbeef", "quiesced", "qualified", "released"):
            self.assertNotIn(value, encoded)

    def test_nonunix_and_nonstream_sockets_are_denied(self):
        for family, kind in ((socket.AF_INET, socket.SOCK_STREAM), (socket.AF_UNIX, socket.SOCK_DGRAM)):
            connection = socket.socket(family, kind)
            self.assertEqual(self.transport.serve(connection), "denied")
            self.assertEqual(connection.fileno(), -1)
        self.assertEqual(self.bridge.calls, [])

    # T33: key mode authenticates each request by the per-launch key's proof,
    # so a restarted host (any PID of the service UID) can observe; exactly one
    # of the enrolled peer or the key is accepted at construction.
    def test_key_mode_authenticates_requests_by_proof_not_peer_pid(self):
        # The same vectors crates/mllm-adapters/src/sglang/observation.rs computes.
        vector = self.module.observation_key("admin", "binding", "incarnation")
        self.assertEqual(vector.hex(),
                         "57fccc735da4dbf27df1429ffdd32592821c41c56fa440fe7ed32aa97411581e")
        self.assertEqual(self.module.request_proof(vector, "binding", "incarnation", "r-1"),
                         "ed01f094bf1dbca00b9ab32be46c29cc89768bbd5aed1cd0b4f567f1d89fadf3")
        key = self.module.observation_key("admin-key", "binding-1", "incarnation-1")
        self.assertEqual(len(key), 32)
        self.assertNotEqual(key, self.module.observation_key("admin-key", "binding-1", "other"))
        # Both, neither, or a malformed key.
        for changes in (dict(key=key), dict(expected_peer=None), dict(key=b"short", expected_peer=None)):
            with self.assertRaises(Exception):
                self.make_transport(**changes)
        keyed = self.make_transport(expected_peer=None, key=key)
        for proof, expected in ((self.module.request_proof(key, "binding-1", "incarnation-1", "r-1"),
                                 "observed"), ("0" * 64, None)):
            client, thread, outcomes = self.connect(keyed)
            request_id = "r-1" if expected else "r-2"
            data = json.dumps(dict(version=2, request_id=request_id, timeout_ms=1000,
                                   proof=proof)).encode()
            client.sendall(struct.pack("!I", len(data)) + data)
            response = self.read_response(client)
            thread.join(3)
            if expected is None:
                self.assertIsNone(response)
                self.assertEqual(outcomes, ["denied"])
            else:
                self.assertEqual((response["version"], response["status"]), (2, expected))

    def test_import_never_loads_engine_modules(self):
        result = subprocess.run(["python3", "-B", "-c",
            "import sys; import runtime.sglang_observation_transport; "
            "assert not any(name.split('.')[0] in ('sglang', 'torch', 'transformers', 'torch_memory_saver') "
            "for name in sys.modules)"], capture_output=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr.decode())


if __name__ == "__main__":
    unittest.main()
