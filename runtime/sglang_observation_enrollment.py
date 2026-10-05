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
No exception text, path or credential is ever written anywhere; a refused
enrollment logs its stage and the exception's class name only. Nothing here
proves release, residency or readiness; the host fuses these facts with its own.
"""

import functools
import hashlib
import json
import os
import stat

# Environment the protected entry sets for its spawned children (validated there).
ENV_DIR = "CAPYCTL_OBSERVATION_DIR"
ENV_SCOPE = "CAPYCTL_OBSERVATION_SCOPE"
# ADR 0019: the launch's declared weight restore, which decides whether the
# saver's weights region may hold a CPU backup (`cpu_backup`, host_backed) and
# whether a speculative scheduler is admitted (`resident`, ADR 0014 A17).
ENV_RESTORE = "CAPYCTL_OBSERVATION_WEIGHT_RESTORE"
WEIGHT_RESTORES = ("disk_reload", "cpu_backup", "resident")

# The live listener, retained for the scheduler process lifetime.
_ENROLLED = []


def _scope():
    """(dir, binding, incarnation, weight restore) from the entry's environment, or None."""
    directory = os.environ.get(ENV_DIR)
    scope = os.environ.get(ENV_SCOPE)
    restore = os.environ.get(ENV_RESTORE)
    if (not directory or not scope or scope.count(":") != 1
            or restore not in WEIGHT_RESTORES):
        return None
    binding, incarnation = scope.split(":")
    return directory, binding, incarnation, restore


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


def _refused(stage, error):
    """One line on the engine's own stderr naming where an enrollment stopped:
a fixed stage word, the exception class name and a binding error's closed code,
never its message, a path or a credential. Found live 2026-10-03 (ADR 0014
amendment A15): a speculative scheduler failed the topology check here and
left no trace, so its park was refused as not quiescent with nothing to say why.
"""
    try:
        import re
        import sys
        kind = type(error).__name__
        if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]{0,63}", kind) is None:
            kind = "unnamed"
        code = getattr(error, "code", None)
        if type(code) is not str or re.fullmatch(r"[a-z_]{1,32}", code) is None:
            code = None
        sys.stderr.write(json.dumps({"event": "capyctl_saver_enrollment_refused",
                                     "stage": stage, "error": kind, "code": code},
                                    separators=(",", ":")) + "\n")
        sys.stderr.flush()
    except Exception:
        pass


def enroll(scheduler):
    """Enroll this scheduler's saver observation; never raises.

    A launch outside any observation scope, or without the memory saver, is
    not an enrollment and returns False silently; an enrollment that stops
    part way logs its stage (`_refused`).
    """
    stage = "scope"
    try:
        scope = _scope()
        if scope is None:
            return False
        directory, binding, incarnation, restore = scope
        from . import sglang_saver_binding as saver
        from . import sglang_saver_residency as residency
        from .sglang_observation_server import SchedulerObservationServer
        from .sglang_observation_transport import observation_key
        from .sglang_scheduler_observer import install_scheduler_observer
        stage = "arguments"
        args = saver._fields(scheduler).get("server_args")
        if residency.server_arg(args, "enable_memory_saver") is not True:
            return False
        admin = residency.server_arg(args, "admin_api_key")
        key = observation_key(admin, binding, incarnation)
        stage = "library"
        path = _preload_library()
        build = saver.TrustedSaverBuild(path, _digest(path), "preload")
        owner = saver.current_process_identity()
        stage = "install"
        bridge = install_scheduler_observer(
            scheduler, binding_id=binding, incarnation_id=incarnation, expected_owner=owner,
            build=build,
            observe=functools.partial(residency.observe_scheduler_saver, weight_restore=restore),
            topology=functools.partial(residency.topology, weight_restore=restore))
        stage = "listen"
        server = SchedulerObservationServer.start(
            path=os.path.join(directory, binding + ".sock"), bridge=bridge, binding_id=binding,
            incarnation_id=incarnation, expected_owner=owner, key=key)
        _ENROLLED.append(server)
        stage = "record"
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
    except Exception as error:
        # Fail closed: without an enrollment the host refuses Park unchanged.
        _refused(stage, error)
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


def entry_environment(directory, binding, incarnation, weight_restore="disk_reload"):
    """Validate the host's observation directory and publish the child scope.

    Called by the protected entry before `launch_server`; the directory must be
    an absolute, canonical, private (0700, service-owned) directory, and
    `weight_restore` the launch's declared restore. Returns False (no
    enrollment) otherwise.
    """
    try:
        if (weight_restore not in WEIGHT_RESTORES
                or type(directory) is not str or not directory.startswith("/")
                or os.path.realpath(directory) != directory
                or len(os.fsencode(os.path.join(directory, binding + ".sock"))) > 107):
            return False
        info = os.stat(directory, follow_symlinks=False)
        if (not stat.S_ISDIR(info.st_mode) or info.st_uid != os.geteuid()
                or stat.S_IMODE(info.st_mode) != 0o700):
            return False
        os.environ[ENV_DIR] = directory
        os.environ[ENV_SCOPE] = binding + ":" + incarnation
        os.environ[ENV_RESTORE] = weight_restore
        return True
    except Exception:
        return False


def clear_environment():
    os.environ.pop(ENV_DIR, None)
    os.environ.pop(ENV_SCOPE, None)
    os.environ.pop(ENV_RESTORE, None)
