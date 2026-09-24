"""Read-only transport for an enrolled scheduler's observation bridge.

The service supplies an already connected AF_UNIX stream, trusted bridge, and
frozen identities. No listener, enrollment, native startup, or controls live here.
serve() takes ownership of the connection and always closes it. Run it on a
transport thread while the scheduler runs its own safe-point ticks. Trusted
Linux procfs and a bridge with bounded request/poll/cancel operations are required.
The two-second deadline bounds socket waits, not a stalled kernel procfs read or
hostile in-process bridge. Same-UID peers are not inherently trusted: exact PID,
boot ID and start ticks must match the separately enrolled controller.

Key mode (version 2) replaces the enrolled controller identity with a per-launch
key: every request carries an HMAC-SHA256 proof over the binding, incarnation
and request ID, so a restarted host agent or controller (a new PID) can still
observe the launch it owns while any other same-UID process without the key
cannot. The key is derived from the launch's admin credential
(`observation_key`), which only the engine and its host hold.

Socket credential checks cannot prevent a trusted controller from handing its
connected FD to another process. Service FD custody is a precondition. Results
are point-in-time saver-map observations only, never readiness, global idleness,
residency, release proof, or qualification. Unknown allocation tags fail closed.
"""
from dataclasses import dataclass
import collections
import hashlib
import hmac
import json
import os
import re
import select
import socket
import struct
import threading
import time

from . import sglang_saver_binding as saver
from . import sglang_scheduler_observer as bridge_module
from .memory_saver_observer import AllocationAggregate, SaverObservation


class _Denied(Exception):
    pass


def _identifier(value):
    if type(value) is not str or re.fullmatch(r"[A-Za-z0-9_.:-]{1,128}", value) is None:
        raise _Denied()
    return value


def _integer(value, minimum=0, maximum=(1 << 64) - 1):
    if type(value) is not int or not minimum <= value <= maximum:
        raise _Denied()
    return value


def _identity(value):
    if type(value) is not saver.ProcessIdentity:
        raise _Denied()
    _integer(value.pid, 1, (1 << 31) - 1)
    _integer(value.start_ticks, 1)
    if type(value.boot_id) is not str or re.fullmatch(
            r"[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}", value.boot_id) is None:
        raise _Denied()
    return dict(pid=value.pid, start_ticks=value.start_ticks, boot_id=value.boot_id)


def _read_proc(path, limit):
    with open(path, "rb") as stream:
        data = stream.read(limit + 1)
    if len(data) > limit:
        raise _Denied()
    return data.decode("ascii", errors="strict")


def _process_identity(pid):
    _integer(pid, 1, (1 << 31) - 1)
    boot = _read_proc("/proc/sys/kernel/random/boot_id", 64).strip()
    head, tail = _read_proc(f"/proc/{pid}/stat", 16384).rsplit(") ", 1)
    fields = tail.split()
    if int(head.split(" (", 1)[0]) != pid or fields[0] in ("Z", "X", "x"):
        raise _Denied()
    identity = saver.ProcessIdentity(pid, int(fields[19]), boot)
    _identity(identity)
    if _read_proc("/proc/sys/kernel/random/boot_id", 64).strip() != boot:
        raise _Denied()
    return identity


_KEY_LABEL = b"mllm-sglang-observation-key-v1\0"
_PROOF_LABEL = b"mllm-sglang-observation-request-v2\0"


def observation_key(admin_key, binding_id, incarnation_id):
    """The per-launch observation key, derived from the launch's admin credential.

    Mirrors `observation_key` in crates/mllm-launchers/src/native_observation.rs.
    """
    if type(admin_key) is not str or not 0 < len(admin_key) <= 4096 or not admin_key.isascii():
        raise _Denied()
    message = (_KEY_LABEL + _identifier(binding_id).encode("ascii") + b"\0"
               + _identifier(incarnation_id).encode("ascii"))
    return hmac.new(admin_key.encode("ascii"), message, hashlib.sha256).digest()


def request_proof(key, binding_id, incarnation_id, request_id):
    """Hex HMAC-SHA256 proof a key-mode request carries."""
    message = (_PROOF_LABEL + binding_id.encode("ascii") + b"\0"
               + incarnation_id.encode("ascii") + b"\0" + request_id.encode("ascii"))
    return hmac.new(key, message, hashlib.sha256).hexdigest()


@dataclass(frozen=True)
class _Binding:
    binding_id: str
    incarnation_id: str
    owner: saver.ProcessIdentity
    peer: saver.ProcessIdentity | None
    uid: int
    key: bytes | None = None


def _pairs(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise _Denied()
        result[key] = value
    return result


def _remaining(deadline):
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise _Denied()
    return remaining


def _quiet(connection):
    try:
        # Any byte, or EOF, violates the one-request connected protocol.
        connection.recv(1, socket.MSG_PEEK | socket.MSG_DONTWAIT)
    except BlockingIOError:
        return
    raise _Denied()


def _receive(connection, count, deadline):
    result = bytearray()
    while len(result) < count:
        readable, _, _ = select.select([connection], [], [], _remaining(deadline))
        if not readable:
            raise _Denied()
        try:
            data = connection.recv(count - len(result))
        except BlockingIOError:
            continue
        if not data:
            raise _Denied()
        result.extend(data)
    return bytes(result)


def _allocations(value):
    if type(value) is not SaverObservation or type(value.groups) is not tuple or len(value.groups) > 4096:
        raise _Denied()
    groups = []
    seen = set()
    for group in value.groups:
        if type(group) is not AllocationAggregate or group.tag not in ("weights", "kv_cache"):
            raise _Denied()
        device = _integer(group.device, 0, (1 << 31) - 1)
        key = (device, group.tag)
        if key in seen:
            raise _Denied()
        seen.add(key)
        row = dict(device=device, tag=group.tag,
                   allocation_count=_integer(group.allocation_count, 1, 4096),
                   active_count=_integer(group.active_count, 0, 4096),
                   paused_count=_integer(group.paused_count, 0, 4096),
                   virtual_bytes=_integer(group.virtual_bytes, 1),
                   mapped_bytes=_integer(group.mapped_bytes),
                   backup_bytes=_integer(group.backup_bytes),
                   backup_enabled_count=_integer(group.backup_enabled_count, 0, 4096))
        if (row["active_count"] + row["paused_count"] != row["allocation_count"]
                or row["mapped_bytes"] > row["virtual_bytes"]
                or row["backup_bytes"] != 0 or row["backup_enabled_count"] != 0):
            raise _Denied()
        groups.append(row)
    result = dict(groups=groups, allocation_count=_integer(value.allocation_count, 0, 4096),
                  virtual_bytes=_integer(value.virtual_bytes), mapped_bytes=_integer(value.mapped_bytes),
                  backup_bytes=_integer(value.backup_bytes))
    if any(result[key] != sum(group[key] for group in groups)
           for key in ("allocation_count", "virtual_bytes", "mapped_bytes", "backup_bytes")):
        raise _Denied()
    return result


def _snapshot(value, owner):
    if type(value) is not saver.SchedulerSaverObservation or value.owner != owner or value.hook_mode != "preload":
        raise _Denied()
    library = value.library
    if type(library) is not saver.LoadedSaverLibrary or type(library.sha256) is not str or re.fullmatch(
            "[0-9a-f]{64}", library.sha256) is None:
        raise _Denied()
    # Deliberately omit backing-file identity details, paths and any future fields.
    return dict(owner=_identity(value.owner), library_sha256=library.sha256,
                hook_mode="preload", allocations=_allocations(value.allocations))


def _encode(value):
    data = json.dumps(value, ensure_ascii=True, separators=(",", ":"), allow_nan=False).encode("ascii")
    if len(data) > 65536:
        raise _Denied()
    return struct.pack("!I", len(data)) + data


# SPEC §9.2 / T21: the replay fence remembers this many recent correlation IDs.
_REPLAY_WINDOW = 4096


class ObservationTransport:
    """One active request; a sliding replay fence over the last 4096 IDs.

SPEC §9.2 / T21: a correlation ID seen among the most recent 4096 is refused.
The oldest is forgotten to admit a new one, so a long-lived launch keeps
answering; an ID that old is no longer fenced (every request is still
authenticated per connection and, in key mode, by its own proof).

Use one service-owned instance per enrolled bridge. Constructing another instance
does not create a durable replay epoch. Cleanup failure permanently disables this
instance; the service must reconcile the bridge before replacing it.
"""
    def __init__(self, *, bridge, binding_id, incarnation_id, expected_owner, expected_peer=None,
                 key=None):
        _identity(expected_owner)
        # Exactly one peer authentication: the enrolled controller identity
        # (version 1) or the per-launch key (version 2).
        if (expected_peer is None) == (key is None):
            raise _Denied()
        if expected_peer is not None:
            _identity(expected_peer)
        elif type(key) is not bytes or len(key) != 32:
            raise _Denied()
        self._binding = _Binding(_identifier(binding_id), _identifier(incarnation_id),
                                 expected_owner, expected_peer, os.getuid(), key)
        self._version = 1 if key is None else 2
        self._bridge = bridge
        self._lock = threading.Lock()
        self._seen = set()
        self._order = collections.deque()
        self._poisoned = False

    def _authenticate(self, connection):
        binding = self._binding
        if (connection.family != socket.AF_UNIX
                or connection.getsockopt(socket.SOL_SOCKET, socket.SO_TYPE) != socket.SOCK_STREAM
                or os.getuid() != binding.uid or os.geteuid() != binding.uid):
            raise _Denied()
        pid, uid, _ = struct.unpack("3i", connection.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
        if uid != binding.uid or pid <= 0 or os.getpid() != binding.owner.pid:
            raise _Denied()
        # Key mode authenticates each request by its proof instead of a peer PID.
        if binding.peer is not None and (pid != binding.peer.pid
                                         or _process_identity(pid) != binding.peer):
            raise _Denied()
        if _process_identity(os.getpid()) != binding.owner:
            raise _Denied()

    def _uncertain(self, request_id):
        now = time.monotonic_ns()
        return dict(version=self._version, binding_id=self._binding.binding_id,
                    incarnation_id=self._binding.incarnation_id, request_id=request_id,
                    owner=_identity(self._binding.owner), started_ns=now, finished_ns=now,
                    status="uncertain", observation=None)

    def _project(self, result, request_id, requested_ns):
        if (type(result) is not bridge_module.ObservationResult
                or result.binding_id != self._binding.binding_id
                or result.incarnation_id != self._binding.incarnation_id
                or result.request_id != request_id or result.owner != self._binding.owner
                or result.status not in ("observed", "uncertain")):
            raise _Denied()
        started = _integer(result.started_ns, requested_ns, time.monotonic_ns())
        finished = _integer(result.finished_ns, started, time.monotonic_ns())
        if result.status == "uncertain":
            if result.observation is not None:
                raise _Denied()
            observation = None
        else:
            observation = _snapshot(result.observation, self._binding.owner)
        value = self._uncertain(request_id)
        value.update(started_ns=started, finished_ns=finished, status=result.status, observation=observation)
        return value

    def _cancel(self, request_id):
        try:
            self._bridge.cancel(request_id)
            result = self._bridge.poll(request_id)
            if (type(result) is not bridge_module.ObservationResult
                    or result.request_id != request_id
                    or result.binding_id != self._binding.binding_id
                    or result.incarnation_id != self._binding.incarnation_id
                    or result.owner != self._binding.owner
                    or result.status != "uncertain" or result.observation is not None):
                self._poisoned = True
        except Exception:
            # Never guess whether a slot or late result is still live.
            self._poisoned = True

    def serve(self, connection):
        """Close one accepted socket; return only observed, uncertain, or denied.

Malformed/unauthenticated requests get no response. Valid observer failures may
receive a correlated uncertain frame. EOF, excess input and expired deadlines
close without a frame; no partial frame is a successful protocol result.
"""
        deadline = time.monotonic() + 2
        acquired = self._lock.acquire(blocking=False)
        request_id = None
        pending = False
        try:
            if not acquired or self._poisoned:
                return "denied"
            self._authenticate(connection)
            connection.setblocking(False)
            length = struct.unpack("!I", _receive(connection, 4, deadline))[0]
            if not 1 <= length <= 1024:
                raise _Denied()
            request = json.loads(_receive(connection, length, deadline).decode("utf-8", errors="strict"),
                                 object_pairs_hook=_pairs)
            fields = {"version", "request_id", "timeout_ms"}
            if self._version == 2:
                fields.add("proof")
            if type(request) is not dict or set(request) != fields:
                raise _Denied()
            if type(request["version"]) is not int or request["version"] != self._version:
                raise _Denied()
            candidate_id = _identifier(request["request_id"])
            if self._version == 2:
                proof = request["proof"]
                expected = request_proof(self._binding.key, self._binding.binding_id,
                                         self._binding.incarnation_id, candidate_id)
                if (type(proof) is not str or len(proof) != 64
                        or not hmac.compare_digest(proof.encode("ascii", "replace"),
                                                   expected.encode("ascii"))):
                    raise _Denied()
            timeout_ms = _integer(request["timeout_ms"], 1, 2000)
            if candidate_id in self._seen:
                raise _Denied()
            _quiet(connection)
            self._authenticate(connection)
            request_id = candidate_id
            while len(self._order) >= _REPLAY_WINDOW:
                self._seen.discard(self._order.popleft())
            self._seen.add(request_id)
            self._order.append(request_id)
            deadline = min(deadline, time.monotonic() + timeout_ms / 1000)
            requested_ns = time.monotonic_ns()
            response = self._uncertain(request_id)
            try:
                # Mark before invocation: a failing request may already own a slot.
                pending = True
                self._bridge.request(request_id, timeout_ms=max(1, int(_remaining(deadline) * 1000)))
                while True:
                    _remaining(deadline)
                    _quiet(connection)
                    result = self._bridge.poll(request_id)
                    if result is not None:
                        pending = False
                        response = self._project(result, request_id, requested_ns)
                        break
                    select.select([connection], [], [], min(0.005, _remaining(deadline)))
            except Exception:
                if pending:
                    self._cancel(request_id)
                    pending = False
                response = self._uncertain(request_id)
            try:
                encoded = _encode(response)
            except Exception:
                response = self._uncertain(request_id)
                encoded = _encode(response)
            self._authenticate(connection)
            _quiet(connection)
            view = memoryview(encoded)
            while view:
                readable, writable, _ = select.select([connection], [connection], [], _remaining(deadline))
                if readable:
                    raise _Denied()
                if writable:
                    try:
                        sent = connection.send(view)
                    except BlockingIOError:
                        continue
                    if not sent:
                        raise _Denied()
                    view = view[sent:]
            return response["status"]
        except Exception:
            return "uncertain" if request_id is not None else "denied"
        finally:
            if pending:
                self._cancel(request_id)
            try:
                connection.close()
            finally:
                if acquired:
                    self._lock.release()
