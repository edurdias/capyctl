"""Process-local, one-slot scheduler observation bridge. No remote transport.

Install on the enrolled scheduler thread after initialization and before its
event loop. ADR 0008: the scheduler shape this hooks is probed at launch
(engine_capabilities.py `observation`), not audited against pinned sources. At 94602c9c2b7cbdb8efd5c52802dac6a1c180089e the normal and overlap
loops call process_input_requests before selecting the next batch, including
while paused. Its successful return is a Python scheduling boundary, NOT a CUDA
synchronization or proof that any local or global work is idle. The hook neither
reads queue emptiness nor derives readiness, weights/cache validity or quiescence.

One bounded saver observation is attempted per requested tick. File/proc reads
inside the saver binding are size bounded, not hard wall-clock bounded; an OS
stall cannot be interrupted here. Deadlines discard late results. The scheduler
never waits for a bridge lock or for transport/requester completion. While an
accepted observation connection is active, it yields for 1 ms per tick to let
the transport thread finish its checks and reply. Ordinary ticks do not sleep.
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


def _diagnose(stage, error):
    """One line on the engine's own stderr: a fixed stage name and the exception
class name, never its message, arguments or any native text."""
    try:
        import sys
        kind = type(error).__name__ if error is not None else None
        if kind is not None and re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]{0,63}", kind) is None:
            kind = "unnamed"
        # A binding error's code is from a closed set of fixed words.
        code = getattr(error, "code", None)
        if type(code) is not str or re.fullmatch(r"[a-z_]{1,32}", code) is None:
            code = None
        sys.stderr.write('{"event":"capyctl_saver_observation_failed","stage":"%s","error":%s,"code":%s}\n'
                         % (stage, '"%s"' % kind if kind else "null", '"%s"' % code if code else "null"))
        sys.stderr.flush()
    except Exception:
        pass


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
    def __init__(self, scheduler, binding_id, incarnation_id, owner, build, observe=None,
                 topology=None):
        self._scheduler = scheduler
        self._binding_id = binding_id
        self._incarnation_id = incarnation_id
        self._owner = owner
        self._build = build
        # The installation-specific snapshot and topology readers; None keeps
        # the patched-saver defaults (resolved at call time).
        self._observe = observe
        self._topology = topology
        self._thread = threading.get_ident()
        self._lock = threading.Lock()
        self._slot = None
        # Scheduling hint only; neither authentication nor observation evidence.
        self.transport_active = threading.Event()
        # Diagnostic timings of the latest request (monotonic ns and counts),
        # replaced whole, never read by the scheduler: see `timing`.
        self._timing = None

    def timing(self, request_id):
        """What the scheduler did for one request, for the transport's log line.

Milliseconds from the request to the safe point that ran the snapshot (`wait_ms`),
the snapshot's own duration (`observe_ms`), how many safe points passed while the
request was pending (`ticks`), how many found the bridge lock held (`contended`),
and whether the result reached the slot (`stored`). None for another request.
Found live 2026-09-24 (rc.2, M28): a refused park could not say whether the
scheduler never reached a safe point or the snapshot itself was slow.
"""
        timing = self._timing
        if timing is None or timing.get("id") != request_id:
            return None
        def millis(start, end):
            if start is None or end is None:
                return None
            return (end - start) // 1_000_000
        return dict(wait_ms=millis(timing["requested"], timing["started"]),
                    observe_ms=millis(timing["started"], timing["finished"]),
                    ticks=timing["ticks"], contended=timing["contended"],
                    stored=timing["stored"])

    def request(self, request_id, *, timeout_ms):
        _identifier(request_id)
        if type(timeout_ms) is not int or not 1 <= timeout_ms <= 2000:
            raise BridgeError("invalid")
        with self._lock:
            if self._slot is not None:
                raise BridgeError("busy")
            now = time.monotonic_ns()
            self._slot = dict(id=request_id, deadline=now + timeout_ms * 1_000_000,
                              result=None, running=False, uncertain=False)
            self._timing = dict(id=request_id, requested=now, started=None, finished=None,
                                ticks=0, contended=0, stored=None)

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
        timing = self._timing
        if not self._lock.acquire(blocking=False):
            if timing is not None and timing["started"] is None:
                timing["contended"] += 1
            return
        try:
            slot = self._slot
            if slot is None or slot["running"] or slot["result"] is not None:
                return
            timing = self._timing if (self._timing or {}).get("id") == slot["id"] else None
            if timing is not None:
                timing["ticks"] += 1
            started = time.monotonic_ns()
            if (dispatch_failed or threading.get_ident() != self._thread
                    or started >= slot["deadline"] or slot["uncertain"]):
                slot["uncertain"] = True
                return
            slot["running"] = True
            if timing is not None:
                timing["started"] = started
        finally:
            self._lock.release()
        observation = None
        stage = "topology_before"
        try:
            topology = self._topology or _topology
            observe = self._observe or observe_scheduler_saver
            topology(self._scheduler)
            stage = "observe"
            observation = observe(self._scheduler, expected_owner=self._owner, build=self._build)
            stage = "topology_after"
            topology(self._scheduler)
            if type(observation) is not saver.SchedulerSaverObservation or observation.owner != self._owner:
                _diagnose("result_shape", None)
                observation = None
        except Exception as error:
            # Never emit native exception text or change successful dispatch
            # semantics; the stage and exception class only (found live
            # 2026-09-23, M28: an uncertain observation left no trace of why).
            _diagnose(stage, error)
            observation = None
        finally:
            finished = time.monotonic_ns()
            stored = False
            if self._lock.acquire(blocking=False):
                try:
                    if self._slot is slot:
                        if slot["uncertain"] or finished >= slot["deadline"]:
                            if observation is not None:
                                _diagnose("deadline", None)
                            observation = None
                        slot["result"] = self._result(slot, started, finished, observation)
                        stored = True
                finally:
                    self._lock.release()
            if timing is not None:
                timing["finished"] = finished
                timing["stored"] = stored
            # If contended, the running slot expires uncertain; never retry a snapshot.


def install_scheduler_observer(scheduler, *, binding_id, incarnation_id, expected_owner, build,
                               observe=None, topology=None):
    """Install once on an existing exact Scheduler; never import or create one.

Caller supplies service-enrolled binding/incarnation/process and reviewed build.
These labels cannot establish trust by themselves. Only this instance changes;
the upstream class, native controls, and engine entry gate remain untouched.
`observe` and `topology` select the installation's snapshot and topology readers
(sglang_saver_residency for SGLang 0.5.20 with torch-memory-saver 0.0.10); the
defaults read the patched saver's snapshot export.
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
        (topology or _topology)(scheduler)
        values = saver._fields(scheduler)
        if "process_input_requests" in values:
            raise BridgeError("invalid")
        original = scheduler.process_input_requests
        if type(original) is not types.MethodType or original.__self__ is not scheduler:
            raise BridgeError("invalid")
    except saver.SaverBindingError:
        raise BridgeError("topology") from None
    bridge = SchedulerObserverBridge(scheduler, binding_id, incarnation_id, expected_owner, build,
                                     observe, topology)

    def process_input_requests(instance, *args, **kwargs):
        try:
            result = original(*args, **kwargs)
        except BaseException:
            bridge._tick(dispatch_failed=True)
            raise
        bridge._tick()
        # SPEC §9.2 / T20 T22: a busy scheduler must let the observer perform
        # custody/authentication and return its snapshot within the deadline.
        # Cover the whole connection, including before and after the slot.
        if bridge.transport_active.is_set():
            time.sleep(0.001)
        return result

    scheduler.process_input_requests = types.MethodType(process_input_requests, scheduler)
    return bridge
