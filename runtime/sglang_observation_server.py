"""Owned, protected Unix listener for an already enrolled scheduler.

No engine imports, process enrollment, lifecycle controls or qualification live
here. The trusted scheduler startup path supplies its existing bridge and exact
controller identity. One retained transport and one thread serialize connections;
no per-connection worker queue or replay-fence reset is introduced. Root and the
service UID are trusted against concurrent filesystem/FD manipulation.
"""
import os
import socket
import stat
import threading

from . import owner_only
from .sglang_observation_transport import ObservationTransport, _process_identity


class ObservationServerError(Exception):
    def __init__(self):
        super().__init__("observer_server_unavailable")


def _same(left, right):
    return (left.st_dev, left.st_ino, left.st_uid, left.st_mode) == (
        right.st_dev, right.st_ino, right.st_uid, right.st_mode)


class _Custody:
    def __init__(self, path):
        self.fds = []
        self.socket_info = None
        self.path = path
        try:
            if (type(path) is not str or not path.startswith("/")
                    or not path.isprintable() or len(os.fsencode(path)) > 107
                    or any(part in ("", ".", "..") for part in path[1:].split("/"))):
                raise ObservationServerError()
            parts = path[1:].split("/")
            self.leaf = parts[-1]
            self.names = parts[:-1]
            flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
            self.fds.append(os.open("/", flags))
            for name in self.names:
                self.fds.append(os.open(name, flags, dir_fd=self.fds[-1]))
            self.infos = [os.fstat(fd) for fd in self.fds]
            self.check_directories()
            if (self.infos[-1].st_uid != os.geteuid()
                    or stat.S_IMODE(self.infos[-1].st_mode) != 0o700):
                raise ObservationServerError()
            try:
                os.stat(self.leaf, dir_fd=self.fds[-1], follow_symlinks=False)
            except FileNotFoundError:
                pass
            else:
                raise ObservationServerError()
        except Exception:
            self.release()
            raise ObservationServerError() from None

    def check_directories(self):
        last = len(self.fds) - 1
        for index, fd in enumerate(self.fds):
            info = os.fstat(fd)
            linked = os.stat("/", follow_symlinks=False) if index == 0 else os.stat(
                self.names[index - 1], dir_fd=self.fds[index - 1], follow_symlinks=False)
            if (not stat.S_ISDIR(info.st_mode) or not _same(info, self.infos[index])
                    or not _same(info, linked)):
                raise ObservationServerError()
            if index == last:
                # capyctl private state: the socket's own 0700 directory stays
                # strict (no group write at all, owner decision 2026-09-23).
                if info.st_uid not in (0, os.geteuid()) or info.st_mode & 0o022:
                    raise ObservationServerError()
                continue
            # The directories on the way to it are the owner's: the owner-only
            # rule, where a private group adds no writer.
            try:
                owner_only.check(info)
            except owner_only.OwnerOnlyError:
                raise ObservationServerError() from None

    def capture_socket(self):
        self.check_directories()
        info = os.stat(self.leaf, dir_fd=self.fds[-1], follow_symlinks=False)
        if not stat.S_ISSOCK(info.st_mode) or info.st_uid != os.geteuid() or info.st_nlink != 1:
            raise ObservationServerError()
        # The parent is already private; other UIDs cannot reach the bind/chmod
        # interval. Never change process-wide umask in this threaded runtime.
        self.socket_info = info
        os.chmod(self.leaf, 0o600, dir_fd=self.fds[-1], follow_symlinks=False)
        protected = os.stat(self.leaf, dir_fd=self.fds[-1], follow_symlinks=False)
        if (protected.st_dev, protected.st_ino) != (info.st_dev, info.st_ino):
            raise ObservationServerError()
        self.socket_info = protected
        self.check()

    def check(self):
        self.check_directories()
        info = os.stat(self.leaf, dir_fd=self.fds[-1], follow_symlinks=False)
        if (self.socket_info is None or not _same(info, self.socket_info)
                or not stat.S_ISSOCK(info.st_mode) or info.st_nlink != 1
                or stat.S_IMODE(info.st_mode) != 0o600):
            raise ObservationServerError()

    def remove_socket(self):
        if self.socket_info is not None:
            self.check()
            os.unlink(self.leaf, dir_fd=self.fds[-1])
            self.socket_info = None

    def release(self):
        for fd in reversed(self.fds):
            os.close(fd)
        self.fds.clear()


class SchedulerObservationServer:
    """Retain for the scheduler lifetime; explicitly close before discarding.

close interrupts an active socket and waits at most three seconds for transport
completion. An uninterruptible provider/kernel stall raises a generic error and
retains custody for a later close. It is not proof of runtime cleanup or release.
The listener never removes a replaced path and never adopts an existing socket.
"""
    def __repr__(self):
        return "SchedulerObservationServer(<private>)"

    @classmethod
    def start(cls, *, path, bridge, binding_id, incarnation_id, expected_owner, expected_peer=None,
              key=None):
        custody = None
        listener = None
        try:
            # Exactly one of the enrolled controller identity or the per-launch
            # key (version 2) authenticates requests; the transport enforces it.
            transport = ObservationTransport(bridge=bridge, binding_id=binding_id,
                incarnation_id=incarnation_id, expected_owner=expected_owner,
                expected_peer=expected_peer, key=key)
            if (os.getuid() != os.geteuid() or _process_identity(os.getpid()) != expected_owner
                    or (expected_peer is not None
                        and _process_identity(expected_peer.pid) != expected_peer)):
                raise ObservationServerError()
            custody = _Custody(path)
            listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            listener.set_inheritable(False)
            listener.bind(path)
            custody.capture_socket()
            listener.listen(1)
            listener.settimeout(0.1)
            server = cls()
            server._custody = custody
            server._listener = listener
            server._transport = transport
            server._bridge = bridge
            server._stop = threading.Event()
            server._lock = threading.Lock()
            server._active = None
            server._closed = False
            server._owner = expected_owner
            server._thread = threading.Thread(target=server._run, name="capyctl-saver-observer", daemon=True)
            server._thread.start()
            return server
        except Exception:
            if listener is not None:
                listener.close()
            if custody is not None:
                try:
                    custody.remove_socket()
                except Exception:
                    pass  # Changed/uncertain path is retained, never overwritten.
                custody.release()
            raise ObservationServerError() from None

    def _run(self):
        try:
            while not self._stop.is_set():
                try:
                    connection, _ = self._listener.accept()
                except socket.timeout:
                    continue
                with self._lock:
                    if self._stop.is_set():
                        connection.close()
                        break
                    self._active = connection
                try:
                    # SPEC §9.2: allow the scheduler to yield during all of the
                    # transport's work, not just the safe-point snapshot.
                    self._bridge.transport_active.set()
                    connection.set_inheritable(False)
                    self._custody.check()
                    if _process_identity(os.getpid()) != self._owner:
                        raise ObservationServerError()
                    self._transport.serve(connection)
                finally:
                    self._bridge.transport_active.clear()
                    connection.close()
                    with self._lock:
                        self._active = None
        except Exception:
            self._stop.set()  # No exception/path/native text reaches logs.
        finally:
            self._listener.close()

    def close(self):
        try:
            with self._lock:
                if self._closed:
                    return
                self._stop.set()
                if self._active is not None:
                    try:
                        self._active.shutdown(socket.SHUT_RDWR)
                    except OSError:
                        pass
                self._listener.close()
            self._thread.join(3)
            if self._thread.is_alive():
                raise ObservationServerError()
            with self._lock:
                if self._closed:
                    return
                self._custody.remove_socket()
                self._custody.release()
                self._closed = True
        except Exception:
            raise ObservationServerError() from None
