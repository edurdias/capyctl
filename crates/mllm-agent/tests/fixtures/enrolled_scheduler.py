"""CPU-only enrolled-scheduler fixture: the production key-mode listener and
enrollment record, over a synthetic saver map the test flips through stdin.

argv: repo-root observation-dir binding incarnation admin-key library-path
stdin: `release` (every allocation unmapped), `resume` (every allocation mapped),
`partial` (weights unmapped, cache mapped), `exit`; each is acknowledged `ok`.
No engine, GPU or saver library is loaded; never qualification.
"""
import hashlib
import json
import os
import sys
import threading
import time

sys.path.insert(0, sys.argv[1])
from runtime.memory_saver_observer import AllocationAggregate, SaverObservation
from runtime.sglang_observation_enrollment import _write_record
from runtime.sglang_observation_server import SchedulerObservationServer
from runtime.sglang_observation_transport import _process_identity, observation_key
from runtime.sglang_saver_binding import LoadedSaverLibrary, SchedulerSaverObservation
from runtime.sglang_scheduler_observer import ObservationResult

directory, binding, incarnation, admin, library = sys.argv[2:7]
owner = _process_identity(os.getpid())
with open(library, "rb") as stream:
    digest = hashlib.sha256(stream.read()).hexdigest()
state = {"weights": True, "kv_cache": True}
lock = threading.Lock()


def group(tag):
    mapped = state[tag]
    return AllocationAggregate(0, tag, 1, int(mapped), int(not mapped), 4096,
                               4096 if mapped else 0, 0, 0)


class Bridge:
    def request(self, request_id, *, timeout_ms):
        with lock:
            groups = (group("kv_cache"), group("weights"))
        now = time.monotonic_ns()
        observation = SchedulerSaverObservation(
            owner, LoadedSaverLibrary(digest, 1, 2, 3, 4, 5), "preload",
            SaverObservation(groups, 2, 8192, sum(g.mapped_bytes for g in groups), 0))
        self.result = ObservationResult(binding, incarnation, request_id, owner, now, now,
                                        "observed", observation)

    def poll(self, request_id):
        return self.result

    def cancel(self, request_id):
        pass


server = SchedulerObservationServer.start(
    path=os.path.join(directory, binding + ".sock"), bridge=Bridge(), binding_id=binding,
    incarnation_id=incarnation, expected_owner=owner,
    key=observation_key(admin, binding, incarnation))
_write_record(directory, binding + ".json", {
    "version": 1, "binding_id": binding, "incarnation_id": incarnation,
    "owner": {"pid": owner.pid, "start_ticks": owner.start_ticks, "boot_id": owner.boot_id},
    "library_path": library, "library_sha256": digest, "hook_mode": "preload",
    "socket": binding + ".sock"})
try:
    print(json.dumps(dict(pid=owner.pid, start_ticks=owner.start_ticks, boot_id=owner.boot_id)),
          flush=True)
    for line in sys.stdin:
        command = line.strip()
        if command == "exit":
            break
        with lock:
            if command == "release":
                state.update(weights=False, kv_cache=False)
            elif command == "resume":
                state.update(weights=True, kv_cache=True)
            elif command == "partial":
                state.update(weights=False, kv_cache=True)
        print("ok", flush=True)
finally:
    server.close()
