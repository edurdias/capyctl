"""CPU-only interoperability fixture; synthetic saver facts, real transport."""
import json
import os
import socket
import sys
import time

sys.path.insert(0, sys.argv[1])
from runtime.sglang_observation_transport import ObservationTransport, _process_identity
from runtime.sglang_scheduler_observer import ObservationResult
from runtime.sglang_saver_binding import LoadedSaverLibrary, SchedulerSaverObservation
from runtime.memory_saver_observer import AllocationAggregate, SaverObservation

owner = _process_identity(os.getpid())
peer = _process_identity(int(sys.argv[3]))


class SyntheticBridge:
    def request(self, request_id, *, timeout_ms):
        now = time.monotonic_ns()
        group = AllocationAggregate(0, "kv_cache", 2, 0, 2, 8192, 0, 0, 0)
        observation = SchedulerSaverObservation(
            owner, LoadedSaverLibrary("a" * 64, 1, 2, 3, 4, 5), "preload",
            SaverObservation((group,), 2, 8192, 0, 0))
        self.result = ObservationResult("binding", "incarnation", request_id,
                                        owner, now, now, "observed", observation)

    def poll(self, request_id):
        return self.result


listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
listener.bind(sys.argv[2])
os.chmod(sys.argv[2], 0o600)
listener.listen(1)
print(json.dumps(dict(pid=owner.pid, start_ticks=owner.start_ticks, boot_id=owner.boot_id)), flush=True)
transport = ObservationTransport(bridge=SyntheticBridge(), binding_id="binding",
                                 incarnation_id="incarnation", expected_owner=owner,
                                 expected_peer=peer)
status = transport.serve(listener.accept()[0])
assert status == "observed", status
assert not any(name == prefix or name.startswith(prefix + ".")
               for name in sys.modules for prefix in ("sglang", "torch", "transformers"))
# Keep the authenticated owner alive until the Rust post-response identity check.
sys.stdin.read(1)
