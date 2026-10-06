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
# ADR 0028 §12 (R12): a group member's topology, `<tp>:<pp>:<nnodes>`, which
# the saver topology check admits in place of the single-rank pin.
ENV_GROUP = "CAPYCTL_OBSERVATION_GROUP"

# The live listener, retained for the scheduler process lifetime.
_ENROLLED = []
# ADR 0028 §12 (R12): the observation credential a group worker's scheduler
# derives its key from, set by `run_enrolled_scheduler` in that process only.
_SECRET = []


class ObservationSecret:
    """A group worker's per-launch observation credential (ADR 0028 §12).

    The worker is handed no admin credential (ADR 0012), so its scheduler's
    observation key comes from this one instead. It reaches the spawned
    scheduler inside its pickled process target, never env or argv, and is
    never formatted.
    """

    __slots__ = ("_value",)

    def __init__(self, value):
        self._value = value

    def __repr__(self):
        return "ObservationSecret(<private>)"

    def __reduce__(self):
        return (ObservationSecret, (self._value,))


def _group(value):
    """(tp, pp, nnodes) from `ENV_GROUP`, or raise ValueError."""
    parts = value.split(":")
    if len(parts) != 3 or any(not part.isascii() or not part.isdigit() or part != str(int(part))
                              for part in parts):
        raise ValueError()
    tp, pp, nnodes = (int(part) for part in parts)
    if not (tp >= 1 and pp >= 1 and nnodes >= 2):
        raise ValueError()
    return tp, pp, nnodes


def _scope():
    """(dir, binding, incarnation, weight restore, group) from the entry's
    environment, or None. `group` is None for a single-rank launch."""
    directory = os.environ.get(ENV_DIR)
    scope = os.environ.get(ENV_SCOPE)
    restore = os.environ.get(ENV_RESTORE)
    if (not directory or not scope or scope.count(":") != 1
            or restore not in WEIGHT_RESTORES):
        return None
    group = os.environ.get(ENV_GROUP)
    try:
        group = None if group is None else _group(group)
    except ValueError:
        return None
    binding, incarnation = scope.split(":")
    return directory, binding, incarnation, restore, group


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
        directory, binding, incarnation, restore, group = scope
        from . import sglang_saver_binding as saver
        from . import sglang_saver_residency as residency
        from .sglang_observation_server import SchedulerObservationServer
        from .sglang_observation_transport import observation_key
        from .sglang_scheduler_observer import install_scheduler_observer
        stage = "arguments"
        args = saver._fields(scheduler).get("server_args")
        if residency.server_arg(args, "enable_memory_saver") is not True:
            return False
        # The key comes from the launch's admin credential; a group worker,
        # which holds none (ADR 0012), uses its own observation credential
        # (ADR 0028 §12).
        secret = (_SECRET[0]._value if _SECRET
                  else residency.server_arg(args, "admin_api_key"))
        key = observation_key(secret, binding, incarnation)
        stage = "library"
        path = _preload_library()
        build = saver.TrustedSaverBuild(path, _digest(path), "preload")
        owner = saver.current_process_identity()
        stage = "install"
        bridge = install_scheduler_observer(
            scheduler, binding_id=binding, incarnation_id=incarnation, expected_owner=owner,
            build=build,
            observe=functools.partial(residency.observe_scheduler_saver, weight_restore=restore),
            topology=(functools.partial(residency.topology, weight_restore=restore)
                      if group is None else
                      functools.partial(residency.topology, weight_restore=restore,
                                        group=group)))
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


def run_enrolled_scheduler(*args, observation_secret=None, **kwargs):
    """SGLang's scheduler process target with observation enrollment.

    Runs in the dedicated spawned scheduler process only: the Scheduler class of
    this process gains an enrolling `run_event_loop` (built, then enrolled, then
    served), and SGLang's own `run_scheduler_process` runs unchanged. A group
    worker's target carries its `ObservationSecret` (ADR 0028 §12), kept in
    this process for `enroll` and never passed to SGLang.
    """
    from sglang.srt.managers import scheduler as module
    if type(observation_secret) is ObservationSecret:
        _SECRET[:] = [observation_secret]
    if _scope() is not None:
        cls = module.Scheduler
        original = cls.__dict__.get("run_event_loop")
        if original is not None and getattr(original, "__wrapped__", None) is None:
            cls.run_event_loop = _enrolling_event_loop(original)
    return module.run_scheduler_process(*args, **kwargs)


def scheduler_target(secret=None):
    """The scheduler target the protected entry hands `launch_server`: the
    enrolling target itself, or, for a group worker, the same target bound to
    its observation credential (picklable for SGLang's spawned process)."""
    if secret is None:
        return run_enrolled_scheduler
    return functools.partial(run_enrolled_scheduler,
                             observation_secret=ObservationSecret(secret))


def entry_environment(directory, binding, incarnation, weight_restore="disk_reload",
                      group=None):
    """Validate the host's observation directory and publish the child scope.

    Called by the protected entry before `launch_server`; the directory must be
    an absolute, canonical, private (0700, service-owned) directory, and
    `weight_restore` the launch's declared restore. ADR 0028 §12: `group` is a
    group member's (tp, pp, nnodes), published for the topology check; None
    for a single rank. Returns False (no enrollment) otherwise.
    """
    try:
        if (weight_restore not in WEIGHT_RESTORES
                or type(directory) is not str or not directory.startswith("/")
                or os.path.realpath(directory) != directory
                or len(os.fsencode(os.path.join(directory, binding + ".sock"))) > 107):
            return False
        if group is not None:
            group = ":".join(str(value) for value in group)
            _group(group)
        info = os.stat(directory, follow_symlinks=False)
        if (not stat.S_ISDIR(info.st_mode) or info.st_uid != os.geteuid()
                or stat.S_IMODE(info.st_mode) != 0o700):
            return False
        os.environ[ENV_DIR] = directory
        os.environ[ENV_SCOPE] = binding + ":" + incarnation
        os.environ[ENV_RESTORE] = weight_restore
        if group is not None:
            os.environ[ENV_GROUP] = group
        return True
    except Exception:
        return False


def clear_environment():
    os.environ.pop(ENV_DIR, None)
    os.environ.pop(ENV_SCOPE, None)
    os.environ.pop(ENV_RESTORE, None)
    os.environ.pop(ENV_GROUP, None)
