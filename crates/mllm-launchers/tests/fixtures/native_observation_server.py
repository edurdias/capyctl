"""CPU-only interoperability fixture; synthetic saver facts, real transport."""
import json
import os
import sys
import time
import threading

sys.path.insert(0, sys.argv[1])
from runtime.sglang_observation_transport import _process_identity, observation_key
from runtime.sglang_observation_server import SchedulerObservationServer
from runtime.sglang_scheduler_observer import ObservationResult
from runtime.sglang_saver_binding import LoadedSaverLibrary, SchedulerSaverObservation
from runtime.memory_saver_observer import AllocationAggregate, SaverObservation

owner = _process_identity(os.getpid())
# Key mode (version 2) when an admin credential follows: no enrolled peer PID.
keyed = len(sys.argv) > 4
peer = None if keyed else _process_identity(int(sys.argv[3]))
key = observation_key(sys.argv[4], "binding", "incarnation") if keyed else None


class SyntheticBridge:
    def __init__(self):
        self.calls = 0
        self.transport_active = threading.Event()

    def request(self, request_id, *, timeout_ms):
        self.calls += 1
        now = time.monotonic_ns()
        group = AllocationAggregate(0, "kv_cache", 2, 0, 2, 8192, 0, 0, 0)
        observation = SchedulerSaverObservation(
            owner, LoadedSaverLibrary("a" * 64, 1, 2, 3, 4, 5), "preload",
            SaverObservation((group,), 2, 8192, 0, 0))
        self.result = ObservationResult("binding", "incarnation", request_id,
                                        owner, now, now, "observed", observation)

    def poll(self, request_id):
        return self.result


bridge = SyntheticBridge()
server = SchedulerObservationServer.start(path=sys.argv[2], bridge=bridge,
    binding_id="binding", incarnation_id="incarnation", expected_owner=owner, expected_peer=peer,
    key=key)
try:
    print(json.dumps(dict(pid=owner.pid, start_ticks=owner.start_ticks, boot_id=owner.boot_id)), flush=True)
    # Keep the authenticated owner alive until the Rust post-response identity check.
    sys.stdin.read(1)
    assert bridge.calls == 1
    assert not any(name == prefix or name.startswith(prefix + ".")
                   for name in sys.modules for prefix in ("sglang", "torch", "transformers"))
finally:
    server.close()
