"""CPU-only safe-point bridge contracts; native saver boundary is replaced."""
import importlib
import sys
import subprocess
import threading
import types
import unittest
from unittest import mock

from runtime import sglang_saver_binding as saver
from runtime.memory_saver_observer import SaverObservation


class SchedulerBridgeTests(unittest.TestCase):
    def setUp(self):
        self.assertIsNotNone(importlib.util.find_spec("runtime.sglang_scheduler_observer"),
                             "safe-point bridge is not implemented")
        self.module = importlib.import_module("runtime.sglang_scheduler_observer")
        self.calls = []
        calls = self.calls
        class Scheduler:
            def process_input_requests(self, requests):
                calls.append(("dispatch", requests))
                if isinstance(requests, BaseException):
                    raise requests
                return requests
        self.scheduler = Scheduler()
        self.scheduler.server_args = types.SimpleNamespace(
            tp_size=1, pp_size=1, dp_size=1, ep_size=1, dcp_size=1,
            attn_cp_size=1, moe_dp_size=1, enable_dp_attention=False,
            enable_dp_lm_head=False, enable_prefill_cp=False,
            speculative_algorithm=None, disaggregation_mode="null")
        self.owner = saver.current_process_identity()
        self.build = saver.TrustedSaverBuild("/private/library.so", "a" * 64, "preload")
        self.observation = saver.SchedulerSaverObservation(self.owner,
            saver.LoadedSaverLibrary("a" * 64, 1, 2, 3, 4, 5), "preload",
            SaverObservation((), 0, 0, 0, 0))
        self.loaded = types.ModuleType("sglang.srt.managers.scheduler")
        self.loaded.Scheduler = Scheduler
        self.addCleanup(mock.patch.stopall)
        mock.patch.dict(sys.modules, {self.loaded.__name__: self.loaded}).start()
        mock.patch.object(self.module, "observe_scheduler_saver", self.observe).start()

    def observe(self, scheduler, *, expected_owner, build):
        self.assertIs(scheduler, self.scheduler)
        self.assertEqual(expected_owner, self.owner)
        self.assertEqual(build, self.build)
        self.calls.append(("observe", None))
        return self.observation

    def install(self, **changes):
        values = dict(binding_id="binding-1", incarnation_id="incarnation-1",
                      expected_owner=self.owner, build=self.build)
        values.update(changes)
        return self.module.install_scheduler_observer(self.scheduler, **values)

    def test_request_runs_only_after_dispatch_and_preserves_return(self):
        bridge = self.install()
        bridge.request("request-1", timeout_ms=2000)
        self.assertIsNone(bridge.poll("request-1"))
        marker = []
        self.assertIs(self.scheduler.process_input_requests(marker), marker)
        result = bridge.poll("request-1")
        self.assertEqual(self.calls, [("dispatch", marker), ("observe", None)])
        self.assertEqual((result.binding_id, result.incarnation_id, result.request_id),
                         ("binding-1", "incarnation-1", "request-1"))
        self.assertEqual(result.status, "observed")
        self.assertEqual(result.observation, self.observation)
        self.assertLessEqual(result.started_ns, result.finished_ns)
        self.assertFalse(hasattr(result, "quiesced"))

    def test_yields_only_while_transport_is_active_including_after_snapshot(self):
        # T20/T22: service the observer without slowing ordinary scheduler ticks.
        bridge = self.install()
        with mock.patch.object(self.module.time, "sleep") as sleep:
            self.scheduler.process_input_requests([])
            sleep.assert_not_called()
            bridge.transport_active.set()
            self.scheduler.process_input_requests([])  # Before authentication/request.
            sleep.assert_called_once_with(0.001)
            bridge.request("one", timeout_ms=1000)
            self.scheduler.process_input_requests([])
            self.assertEqual(bridge.poll("one").status, "observed")
            self.scheduler.process_input_requests([])  # Reply still in flight.
            self.assertEqual(sleep.call_count, 3)
            bridge.transport_active.clear()
            self.scheduler.process_input_requests([])
            self.assertEqual(sleep.call_count, 3)

    def test_no_request_has_no_observation_and_one_slot_is_bounded(self):
        bridge = self.install()
        self.scheduler.process_input_requests([])
        self.assertEqual(self.calls, [("dispatch", [])])
        bridge.request("one", timeout_ms=1000)
        with self.assertRaisesRegex(self.module.BridgeError, "busy"):
            bridge.request("two", timeout_ms=1000)
        self.scheduler.process_input_requests([])
        with self.assertRaisesRegex(self.module.BridgeError, "busy"):
            bridge.request("two", timeout_ms=1000)
        bridge.poll("one")
        bridge.request("two", timeout_ms=1000)

    def test_original_exception_preserved_and_observation_uncertain(self):
        bridge = self.install()
        bridge.request("one", timeout_ms=1000)
        error = ValueError("private native error")
        with self.assertRaises(ValueError) as caught:
            self.scheduler.process_input_requests(error)
        self.assertIs(caught.exception, error)
        result = bridge.poll("one")
        self.assertEqual(result.status, "uncertain")
        self.assertIsNone(result.observation)
        self.assertNotIn("private native error", repr(result))

    def test_observer_failure_is_closed_without_changing_dispatch_return(self):
        bridge = self.install()
        bridge.request("one", timeout_ms=1000)
        with mock.patch.object(self.module, "observe_scheduler_saver",
                               side_effect=RuntimeError("secret")):
            self.assertEqual(self.scheduler.process_input_requests(7), 7)
        result = bridge.poll("one")
        self.assertEqual(result.status, "uncertain")
        self.assertIsNone(result.observation)
        self.assertNotIn("secret", repr(result))

    def test_cancel_and_deadline_discard_late_observation(self):
        bridge = self.install()
        bridge.request("one", timeout_ms=1000)
        bridge.cancel("one")
        self.scheduler.process_input_requests([])
        self.assertEqual(bridge.poll("one").status, "uncertain")
        with mock.patch.object(self.module.time, "monotonic_ns", return_value=100):
            bridge.request("two", timeout_ms=1)
        self.scheduler.process_input_requests([])
        self.assertEqual(bridge.poll("two").status, "uncertain")
        self.assertEqual(self.calls, [("dispatch", []), ("dispatch", [])])

    def test_cancel_during_observation_does_not_publish_late_success(self):
        bridge = self.install()
        bridge.request("one", timeout_ms=1000)
        def cancel(*args, **kwargs):
            bridge.cancel("one")
            return self.observation
        with mock.patch.object(self.module, "observe_scheduler_saver", cancel):
            self.scheduler.process_input_requests([])
        self.assertEqual(bridge.poll("one").status, "uncertain")

    def test_wrong_thread_cannot_observe_but_dispatch_result_survives(self):
        bridge = self.install()
        bridge.request("one", timeout_ms=1000)
        results = []
        thread = threading.Thread(target=lambda: results.append(
            self.scheduler.process_input_requests(9)))
        thread.start()
        thread.join()
        self.assertEqual(results, [9])
        self.assertEqual(bridge.poll("one").status, "uncertain")
        self.assertEqual(self.calls, [("dispatch", 9)])

    def test_invalid_ids_deadlines_topology_and_duplicate_install_rejected(self):
        for bad in ("", "a" * 129, "secret\nvalue", 1):
            with self.assertRaises(self.module.BridgeError):
                self.install(binding_id=bad)
        self.scheduler.server_args.tp_size = 2
        with self.assertRaisesRegex(self.module.BridgeError, "topology"):
            self.install()
        self.scheduler.server_args.tp_size = 1
        bridge = self.install()
        with self.assertRaises(self.module.BridgeError):
            self.install()
        for bad in (0, 2001, True, 1.2):
            with self.assertRaises(self.module.BridgeError):
                bridge.request("one", timeout_ms=bad)
        with self.assertRaises(self.module.BridgeError):
            bridge.poll("unknown")

    def test_changed_topology_or_owner_never_returns_snapshot(self):
        bridge = self.install()
        bridge.request("one", timeout_ms=1000)
        del self.scheduler.server_args.dp_size
        self.scheduler.process_input_requests([])
        self.assertEqual(bridge.poll("one").status, "uncertain")

    def test_contended_tick_does_not_block_or_observe(self):
        bridge = self.install()
        bridge.request("one", timeout_ms=1000)
        with bridge._lock:
            self.assertEqual(self.scheduler.process_input_requests(12), 12)
        self.assertIsNone(bridge.poll("one"))
        self.assertEqual(self.calls, [("dispatch", 12)])
        self.scheduler.process_input_requests([])
        self.assertEqual(bridge.poll("one").status, "observed")

    def test_cancel_consume_new_request_cannot_receive_old_success(self):
        bridge = self.install()
        bridge.request("one", timeout_ms=1000)
        def supersede(*args, **kwargs):
            bridge.cancel("one")
            self.assertEqual(bridge.poll("one").status, "uncertain")
            bridge.request("two", timeout_ms=1000)
            return self.observation
        with mock.patch.object(self.module, "observe_scheduler_saver", supersede):
            self.scheduler.process_input_requests([])
        self.assertIsNone(bridge.poll("two"))
        self.scheduler.process_input_requests([])
        self.assertEqual(bridge.poll("two").request_id, "two")

    def test_timeout_during_snapshot_and_wrong_owner_fail_closed(self):
        bridge = self.install()
        with mock.patch.object(self.module.time, "monotonic_ns", return_value=100):
            bridge.request("one", timeout_ms=1)
        with mock.patch.object(self.module.time, "monotonic_ns", side_effect=[101, 1000100]):
            self.scheduler.process_input_requests([])
        self.assertEqual(bridge.poll("one").status, "uncertain")
        bridge.request("two", timeout_ms=1000)
        from dataclasses import replace
        changed = replace(self.observation, owner=replace(self.owner, pid=self.owner.pid + 1))
        with mock.patch.object(self.module, "observe_scheduler_saver", return_value=changed):
            self.scheduler.process_input_requests([])
        self.assertEqual(bridge.poll("two").status, "uncertain")

    def test_import_has_no_native_import_or_library_construction(self):
        result = subprocess.run([sys.executable, "-B", "-c", '''
import ctypes, sys
def deny(*args, **kwargs): raise AssertionError("library construction")
ctypes.CDLL = deny
import runtime.sglang_scheduler_observer
assert not any(name == "torch" or name.startswith("sglang.") for name in sys.modules)
'''], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()
