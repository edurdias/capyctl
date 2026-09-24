"""Enrollment of an SGLang scheduler's saver observation at engine startup.

SPEC §9.2: parking SGLang is verified by the saver's actual mappings. The
protected entry (sglang_entry.py) hands `run_enrolled_scheduler` to SGLang's
`launch_server` as the scheduler process target. In that spawned scheduler
process, once the Scheduler is built and before its event loop runs, the
enrollment:

1. installs the safe-point observation bridge on that exact Scheduler with the
   installation's reader (sglang_saver_residency: SGLang 0.5.20 pools on
   torch-memory-saver 0.0.10, observed through the CUDA driver);
2. starts the protected Unix listener at `<dir>/<binding>.sock` in the host's
   private 0700 observation directory (owner-only ancestors, 0600 socket, the
   existing custody rules), authenticated in key mode by a per-launch key
   derived from the launch's admin credential, so a restarted host still
   observes the launch it owns and nothing without the key can;
3. writes `<dir>/<binding>.json` (0600, exclusive create): the binding,
   incarnation, the scheduler's process identity and the saver library's path
   and digest, which the host verifies itself before trusting any observation.

An enrollment that cannot complete leaves the engine serving without one: the
host then has no saver evidence and refuses Park with no effect (fail closed).
No exception text, path or credential is ever written anywhere. Nothing here
proves release, residency or readiness; the host fuses these facts with its own.
"""

import hashlib
import json
import os
import stat

# Environment the protected entry sets for its spawned children (validated there).
ENV_DIR = "MLLM_OBSERVATION_DIR"
ENV_SCOPE = "MLLM_OBSERVATION_SCOPE"

# The live listener, retained for the scheduler process lifetime.
_ENROLLED = []


def _scope():
    """(dir, binding, incarnation) from the entry's environment, or None."""
    directory = os.environ.get(ENV_DIR)
    scope = os.environ.get(ENV_SCOPE)
    if not directory or not scope or scope.count(":") != 1:
        return None
    binding, incarnation = scope.split(":")
    return directory, binding, incarnation


def _preload_library():
    """The one torch_memory_saver preload library in LD_PRELOAD, canonical.

    The saver loads exactly this path (its BinaryWrapper's CDLL name), so it is
    used as spelled; a path that is not already canonical is refused.
    """
    entries = [entry for entry in os.environ.get("LD_PRELOAD", "").split(":")
               if "torch_memory_saver" in entry]
    if (len(entries) != 1 or not entries[0].startswith("/")
            or os.path.realpath(entries[0]) != entries[0]):
        raise ValueError
    return entries[0]


def _digest(path):
    with open(path, "rb") as stream:
        digest = hashlib.sha256()
        while True:
            chunk = stream.read(1024 * 1024)
            if not chunk:
                return digest.hexdigest()
            digest.update(chunk)


def _write_record(directory, name, record):
    """Create the enrollment record exclusively, owner read/write only."""
    data = json.dumps(record, sort_keys=True, separators=(",", ":")).encode("ascii")
    parent = os.open(directory, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
    try:
        info = os.fstat(parent)
        if info.st_uid != os.geteuid() or stat.S_IMODE(info.st_mode) != 0o700:
            raise ValueError
        fd = os.open(name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC,
                     0o600, dir_fd=parent)
        try:
            os.fchmod(fd, 0o600)
            view = memoryview(data)
            while view:
                view = view[os.write(fd, view):]
            os.fsync(fd)
        finally:
            os.close(fd)
    finally:
        os.close(parent)


def enroll(scheduler):
    """Enroll this scheduler's saver observation; never raises, never logs."""
    try:
        scope = _scope()
        if scope is None:
            return False
        directory, binding, incarnation = scope
        from . import sglang_saver_binding as saver
        from . import sglang_saver_residency as residency
        from .sglang_observation_server import SchedulerObservationServer
        from .sglang_observation_transport import observation_key
        from .sglang_scheduler_observer import install_scheduler_observer
        args = saver._fields(scheduler).get("server_args")
        if residency.server_arg(args, "enable_memory_saver") is not True:
            return False
        admin = residency.server_arg(args, "admin_api_key")
        key = observation_key(admin, binding, incarnation)
        path = _preload_library()
        build = saver.TrustedSaverBuild(path, _digest(path), "preload")
        owner = saver.current_process_identity()
        bridge = install_scheduler_observer(
            scheduler, binding_id=binding, incarnation_id=incarnation, expected_owner=owner,
            build=build, observe=residency.observe_scheduler_saver, topology=residency.topology)
        server = SchedulerObservationServer.start(
            path=os.path.join(directory, binding + ".sock"), bridge=bridge, binding_id=binding,
            incarnation_id=incarnation, expected_owner=owner, key=key)
        _ENROLLED.append(server)
        _write_record(directory, binding + ".json", {
            "version": 1,
            "binding_id": binding,
            "incarnation_id": incarnation,
            "owner": {"pid": owner.pid, "start_ticks": owner.start_ticks,
                      "boot_id": owner.boot_id},
            "library_path": path,
            "library_sha256": build.sha256,
            "hook_mode": "preload",
            "socket": binding + ".sock",
        })
        return True
    except Exception:
        # Fail closed: without an enrollment the host refuses Park unchanged.
        return False


def _enrolling_event_loop(original):
    def run_event_loop(self, *args, **kwargs):
        enroll(self)
        return original(self, *args, **kwargs)
    run_event_loop.__wrapped__ = original
    return run_event_loop


def run_enrolled_scheduler(*args, **kwargs):
    """SGLang's scheduler process target with observation enrollment.

    Runs in the dedicated spawned scheduler process only: the Scheduler class of
    this process gains an enrolling `run_event_loop` (built, then enrolled, then
    served), and SGLang's own `run_scheduler_process` runs unchanged.
    """
    from sglang.srt.managers import scheduler as module
    if _scope() is not None:
        cls = module.Scheduler
        original = cls.__dict__.get("run_event_loop")
        if original is not None and getattr(original, "__wrapped__", None) is None:
            cls.run_event_loop = _enrolling_event_loop(original)
    return module.run_scheduler_process(*args, **kwargs)


def entry_environment(directory, binding, incarnation):
    """Validate the host's observation directory and publish the child scope.

    Called by the protected entry before `launch_server`; the directory must be
    an absolute, canonical, private (0700, service-owned) directory. Returns
    False (no enrollment) otherwise.
    """
    try:
        if (type(directory) is not str or not directory.startswith("/")
                or os.path.realpath(directory) != directory
                or len(os.fsencode(os.path.join(directory, binding + ".sock"))) > 107):
            return False
        info = os.stat(directory, follow_symlinks=False)
        if (not stat.S_ISDIR(info.st_mode) or info.st_uid != os.geteuid()
                or stat.S_IMODE(info.st_mode) != 0o700):
            return False
        os.environ[ENV_DIR] = directory
        os.environ[ENV_SCOPE] = binding + ":" + incarnation
        return True
    except Exception:
        return False


def clear_environment():
    os.environ.pop(ENV_DIR, None)
    os.environ.pop(ENV_SCOPE, None)
