"""Scheduler observation enrollment, end to end over the real listener, with fakes.

The scheduler, saver chain and CUDA driver are the stand-ins of
test_sglang_saver_residency; the enrollment, bridge, key-mode transport,
protected listener and record are real. CPU fixtures, never qualification.
"""
import json
import os
from pathlib import Path
import socket
import stat
import struct
import sys
import tempfile
import threading
import types
import unittest
from unittest import mock

from runtime import sglang_entry as entry
from runtime import sglang_observation_enrollment as enrollment
from runtime import sglang_saver_residency as residency
from runtime.sglang_observation_transport import observation_key, request_proof
from test_sglang_saver_residency import Fakes

BINDING = "01K00000000000000000000001"
INCARNATION = "01K00000000000000000000002"


class EnrollmentTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="obs-", dir=Path.home())
        self.addCleanup(self.directory.cleanup)
        os.chmod(self.directory.name, 0o700)
        library = Path(self.directory.name) / "torch_memory_saver_hook_mode_preload_cu13.abi3.so"
        library.write_bytes(b"preload library bytes")
        self.library = str(library)
        self.fakes = Fakes(self)
        self.fakes.impl._binary_wrapper.cdll._name = self.library
        mock.patch.object(residency, "CudaDriver", return_value=self.fakes.driver).start()
        mock.patch.dict(os.environ, {"LD_PRELOAD": self.library}).start()
        mock.patch.dict(os.environ).start()
        enrollment.clear_environment()
        self.addCleanup(self.close_all)

    def close_all(self):
        while enrollment._ENROLLED:
            enrollment._ENROLLED.pop().close()

    def scope(self):
        self.assertTrue(enrollment.entry_environment(self.directory.name, BINDING, INCARNATION))

    def ask(self, proof=None, request_id="read-1", version=2):
        """One request from a client thread while this thread, the one that
        enrolled (the scheduler thread), ticks the safe point."""
        answers = []

        def client():
            connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            connection.settimeout(3)
            connection.connect(os.path.join(self.directory.name, BINDING + ".sock"))
            key = observation_key("a" * 64, BINDING, INCARNATION)
            body = dict(version=version, request_id=request_id, timeout_ms=1000,
                        proof=proof or request_proof(key, BINDING, INCARNATION, request_id))
            raw = json.dumps(body).encode()
            connection.sendall(struct.pack("!I", len(raw)) + raw)
            data = bytearray()
            while True:
                try:
                    chunk = connection.recv(65540)
                except (ConnectionResetError, socket.timeout):
                    break
                if not chunk:
                    break
                data.extend(chunk)
            connection.close()
            answers.append(json.loads(data[4:]) if data else None)
        thread = threading.Thread(target=client)
        thread.start()
        while thread.is_alive():
            self.fakes.scheduler.process_input_requests([])
            thread.join(0.005)
        return answers[0]

    # T22 T16: the enrolled scheduler answers key-mode observations of its
    # saver's real mappings, and a release the driver confirms reads as zero.
    def test_enrolled_scheduler_serves_keyed_observations_and_writes_its_record(self):
        self.scope()
        self.assertTrue(enrollment.enroll(self.fakes.scheduler))
        record_path = Path(self.directory.name) / (BINDING + ".json")
        self.assertEqual(stat.S_IMODE(record_path.stat().st_mode), 0o600)
        record = json.loads(record_path.read_text())
        self.assertEqual(record["binding_id"], BINDING)
        self.assertEqual(record["incarnation_id"], INCARNATION)
        self.assertEqual(record["owner"]["pid"], os.getpid())
        self.assertEqual(record["library_path"], self.library)
        self.assertEqual(record["socket"], BINDING + ".sock")
        resident = self.ask()
        self.assertEqual(resident["version"], 2)
        self.assertEqual(resident["status"], "observed")
        self.assertEqual(resident["observation"]["allocations"]["mapped_bytes"], 28672)
        self.fakes.driver.unmapped |= {0x1000_0000, 0x2000_0000, 0x3000_0000}
        released = self.ask(request_id="read-2")
        self.assertEqual(released["observation"]["allocations"]["mapped_bytes"], 0)
        self.assertEqual(released["observation"]["allocations"]["virtual_bytes"], 28672)

    # T20: a request without the launch's key proof, or in the enrolled-peer
    # protocol, gets no answer at all.
    def test_requests_without_the_launch_key_are_denied(self):
        self.scope()
        self.assertTrue(enrollment.enroll(self.fakes.scheduler))
        self.assertIsNone(self.ask(proof="0" * 64))
        self.assertIsNone(self.ask(request_id="read-3", version=1))
        self.assertEqual(self.ask(request_id="read-4")["status"], "observed")

    # T22: anything short of a validated private scope enrolls nothing, and the
    # engine keeps serving (the host then refuses Park unchanged).
    def test_enrollment_fails_closed_without_a_valid_scope(self):
        self.assertFalse(enrollment.enroll(self.fakes.scheduler))
        os.chmod(self.directory.name, 0o750)
        self.assertFalse(enrollment.entry_environment(self.directory.name, BINDING, INCARNATION))
        os.chmod(self.directory.name, 0o700)
        self.assertFalse(enrollment.entry_environment("relative", BINDING, INCARNATION))
        self.scope()
        self.fakes.scheduler.server_args = type(self.fakes.scheduler.server_args)(tp_size=2)
        self.assertFalse(enrollment.enroll(self.fakes.scheduler))
        self.assertEqual(os.listdir(self.directory.name), [Path(self.library).name])

    def test_scheduler_target_enrolls_after_construction_before_the_event_loop(self):
        events = []

        class Scheduler:
            def run_event_loop(self):
                events.append("loop")

        def run_scheduler_process(*args):
            events.append(("run", args))
            Scheduler().run_event_loop()

        loaded = types.ModuleType("sglang.srt.managers.scheduler")
        loaded.Scheduler = Scheduler
        loaded.run_scheduler_process = run_scheduler_process
        mock.patch.dict(sys.modules, {loaded.__name__: loaded,
                                      "sglang.srt.managers": types.ModuleType("m")}).start()
        sys.modules["sglang.srt.managers"].scheduler = loaded
        mock.patch.object(enrollment, "enroll", side_effect=lambda s: events.append("enroll")).start()
        self.scope()
        enrollment.run_enrolled_scheduler(1, 2)
        self.assertEqual(events, [("run", (1, 2)), "enroll", "loop"])

    def test_entry_hands_launch_server_the_target_only_for_an_enrollable_launch(self):
        class Spec:
            def __init__(self, saver):
                self._public_json = json.dumps(dict(binding_id=BINDING, incarnation=INCARNATION,
                                                    settings=dict(memory_saver=saver)))

        class Launch:
            def launch_server(self, server_args, run_scheduler_process_func=None):
                pass

        class OldLaunch:
            def launch_server(self, server_args):
                pass

        os.environ[enrollment.ENV_DIR] = self.directory.name
        self.assertIs(entry._observation_target(Spec(True), Launch()),
                      enrollment.run_enrolled_scheduler)
        self.assertEqual(os.environ[enrollment.ENV_SCOPE], BINDING + ":" + INCARNATION)
        for spec, launch in ((Spec(False), Launch()), (Spec(True), OldLaunch())):
            os.environ[enrollment.ENV_DIR] = self.directory.name
            self.assertIsNone(entry._observation_target(spec, launch))
            self.assertNotIn(enrollment.ENV_SCOPE, os.environ)
            self.assertNotIn(enrollment.ENV_DIR, os.environ)


if __name__ == "__main__":
    unittest.main()
