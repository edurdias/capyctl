"""Process-local, one-slot scheduler observation bridge. No remote transport.

Install on the enrolled scheduler thread after initialization and before its
event loop. The caller must verify the pinned immutable scheduler source before
installation. At fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1 the normal and overlap
loops call process_input_requests before selecting the next batch, including
while paused. Its successful return is a Python scheduling boundary, NOT a CUDA
synchronization or proof that any local or global work is idle. The hook neither
reads queue emptiness nor derives readiness, weights/cache validity or quiescence.

One bounded saver observation is attempted per requested tick. File/proc reads
inside the saver binding are size bounded, not hard wall-clock bounded; an OS
stall cannot be interrupted here. Deadlines discard late results. The scheduler
never waits for a bridge lock, transport, requester, or result consumer.
"""
from dataclasses import dataclass
import re
import threading
import time
import types

from . import sglang_saver_binding as saver
from .sglang_saver_binding import observe_scheduler_saver


class BridgeError(Exception):
    def __init__(self, code):
        self.code = code if code in {"invalid", "busy", "unknown", "owner", "topology"} else "invalid"
        super().__init__(self.code)


@dataclass(frozen=True)
class ObservationResult:
    binding_id: str
    incarnation_id: str
    request_id: str
    owner: saver.ProcessIdentity
    started_ns: int
    finished_ns: int
    status: str
    observation: saver.SchedulerSaverObservation | None


def _identifier(value):
    if type(value) is not str or re.fullmatch(r"[A-Za-z0-9_.:-]{1,128}", value) is None:
        raise BridgeError("invalid")
    return value


def _topology(scheduler):
    values = saver._fields(saver._fields(scheduler).get("server_args"))
    expected = {name: 1 for name in ("tp_size", "dp_size", "pp_size", "ep_size",
                                    "dcp_size", "attn_cp_size", "moe_dp_size")}
    expected.update(enable_dp_attention=False, enable_dp_lm_head=False,
                    enable_prefill_cp=False, speculative_algorithm=None,
                    disaggregation_mode="null")
    if any(name not in values or type(values[name]) is not type(value)
           or values[name] != value for name, value in expected.items()):
        raise BridgeError("topology")


class SchedulerObserverBridge:
    """Trusted in-process capability, not authentication or enrollment evidence.

request/poll/cancel are thread-safe. A request occupies the slot until poll
consumes its terminal result, including cancellation. Request IDs are correlation
labels, not durable replay fences; a future transport must enforce its own epochs.
"""
    def __init__(self, scheduler, binding_id, incarnation_id, owner, build):
        self._scheduler = scheduler
        self._binding_id = binding_id
        self._incarnation_id = incarnation_id
        self._owner = owner
        self._build = build
        self._thread = threading.get_ident()
        self._lock = threading.Lock()
        self._slot = None

    def request(self, request_id, *, timeout_ms):
        _identifier(request_id)
        if type(timeout_ms) is not int or not 1 <= timeout_ms <= 2000:
            raise BridgeError("invalid")
        with self._lock:
            if self._slot is not None:
                raise BridgeError("busy")
            self._slot = dict(id=request_id, deadline=time.monotonic_ns() + timeout_ms * 1_000_000,
                              result=None, running=False, uncertain=False)

    def _result(self, slot, started, finished, observation=None):
        return ObservationResult(self._binding_id, self._incarnation_id, slot["id"],
                                 self._owner, started, finished,
                                 "observed" if observation is not None else "uncertain", observation)

    def _matching(self, request_id):
        _identifier(request_id)
        slot = self._slot
        if slot is None or slot["id"] != request_id:
            raise BridgeError("unknown")
        return slot

    def poll(self, request_id):
        with self._lock:
            slot = self._matching(request_id)
            now = time.monotonic_ns()
            if now >= slot["deadline"]:
                slot["uncertain"] = True
            if slot["uncertain"]:
                slot["result"] = self._result(slot, now, now)
            result = slot["result"]
            if result is not None:
                self._slot = None
            return result

    def cancel(self, request_id):
        with self._lock:
            self._matching(request_id)["uncertain"] = True

    def _tick(self, *, dispatch_failed=False):
        if not self._lock.acquire(blocking=False):
            return
        try:
            slot = self._slot
            if slot is None or slot["running"] or slot["result"] is not None:
                return
            started = time.monotonic_ns()
            if (dispatch_failed or threading.get_ident() != self._thread
                    or started >= slot["deadline"] or slot["uncertain"]):
                slot["uncertain"] = True
                return
            slot["running"] = True
        finally:
            self._lock.release()
        observation = None
        try:
            _topology(self._scheduler)
            observation = observe_scheduler_saver(self._scheduler, expected_owner=self._owner,
                                                   build=self._build)
            _topology(self._scheduler)
            if type(observation) is not saver.SchedulerSaverObservation or observation.owner != self._owner:
                observation = None
        except Exception:
            # Never emit native exception text or change successful dispatch semantics.
            observation = None
        finally:
            finished = time.monotonic_ns()
            if self._lock.acquire(blocking=False):
                try:
                    if self._slot is slot:
                        if slot["uncertain"] or finished >= slot["deadline"]:
                            observation = None
                        slot["result"] = self._result(slot, started, finished, observation)
                finally:
                    self._lock.release()
            # If contended, the running slot expires uncertain; never retry a snapshot.


def install_scheduler_observer(scheduler, *, binding_id, incarnation_id, expected_owner, build):
    """Install once on an existing exact Scheduler; never import or create one.

Caller supplies service-enrolled binding/incarnation/process and reviewed build.
These labels cannot establish trust by themselves. Only this instance changes;
the upstream class, native controls, and engine entry gate remain untouched.
"""
    _identifier(binding_id)
    _identifier(incarnation_id)
    if (type(expected_owner) is not saver.ProcessIdentity
            or expected_owner != saver.current_process_identity()):
        raise BridgeError("owner")
    if type(build) is not saver.TrustedSaverBuild:
        raise BridgeError("invalid")
    try:
        saver._exact(scheduler, "sglang.srt.managers.scheduler", "Scheduler")
        _topology(scheduler)
        values = saver._fields(scheduler)
        if "process_input_requests" in values:
            raise BridgeError("invalid")
        original = scheduler.process_input_requests
        if type(original) is not types.MethodType or original.__self__ is not scheduler:
            raise BridgeError("invalid")
    except saver.SaverBindingError:
        raise BridgeError("topology") from None
    bridge = SchedulerObserverBridge(scheduler, binding_id, incarnation_id, expected_owner, build)

    def process_input_requests(instance, *args, **kwargs):
        try:
            result = original(*args, **kwargs)
        except BaseException:
            bridge._tick(dispatch_failed=True)
            raise
        bridge._tick()
        return result

    scheduler.process_input_requests = types.MethodType(process_input_requests, scheduler)
    return bridge
