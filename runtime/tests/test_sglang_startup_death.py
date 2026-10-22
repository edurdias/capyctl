"""A scheduler that dies before readiness fails the launch promptly.

Found live on 2026-10-20: SGLang's scheduler failed at startup (a missing CUDA
library) and its parent stayed alive, so the host waited out the readiness
deadline and reported ownership uncertainty instead of a launch failure. SGLang
notices the death within seconds and raises out of `launch_server`, but the
processes it already started are non-daemon children that interpreter exit
joins forever. These run the protected entry as its own process group leader,
as the launcher does, against a synthetic SGLang tree whose `launch_server`
does what SGLang's does. CPU fixtures, never evidence that a build serves a
model (AGENTS.md: CPU and fake tests are not qualification).
"""

import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest

from test_engine_capabilities import REPO, TESTS, sglang_tree


# What SGLang 0.5.22 does on a scheduler that exits during startup without
# signalling its parent: `_set_envs_and_config` makes the prometheus
# multiprocess directory (a TemporaryDirectory the parent owns), the scheduler
# and detokenizer start as non-daemon spawn children, and
# `_wait_for_scheduler_ready` polls the scheduler's pipe and raises once the
# process is not alive (5 s polls there; shorter here).
HTTP_SERVER = '''
import multiprocessing as mp
import multiprocessing.util
import os
import subprocess
import sys
import tempfile
import time
import traceback

LAUNCHED = []
class _Route:
    def __init__(self, path):
        self.path = path
class _App:
    routes = [_Route("/generate"), _Route("/v1/models")]
app = _App()

def _scheduler():
    os._exit(1)

def _detokenizer(directory, record):
    helper = subprocess.Popen([sys.executable, "-I", "-S", "-c", "import time; time.sleep(600)"])
    with open(record + ".part", "w") as handle:
        handle.write("%d %d %s" % (os.getpid(), helper.pid, directory))
    os.rename(record + ".part", record)
    while True:
        try:
            open(os.path.join(directory, "gauge_all_%d.db" % os.getpid()), "a+b").close()
        except FileNotFoundError:
            traceback.print_exc()
        time.sleep(0.05)

_prometheus = None

def launch_server(server_args, run_scheduler_process_func=None):
    global _prometheus
    LAUNCHED.append(server_args)
    _prometheus = tempfile.TemporaryDirectory()
    os.environ["PROMETHEUS_MULTIPROC_DIR"] = _prometheus.name
    mp.set_start_method("spawn", force=True)
    record = os.environ["STUB_RECORD"]
    detokenizer = mp.Process(target=_detokenizer, args=(_prometheus.name, record))
    detokenizer.start()
    while not os.path.exists(record):
        time.sleep(0.05)
    reader, writer = mp.Pipe(duplex=False)
    scheduler = mp.Process(target=_scheduler)
    scheduler.start()
    while not reader.poll(timeout=0.5):
        if not scheduler.is_alive():
            raise RuntimeError("Rank 0 scheduler died during initialization")
'''

# The entry runs to interpreter exit, as `python -IS sglang_entry.py` does.
ENTRY_SCRIPT = r"""
import importlib, sys
repo, tests, tree = sys.argv[1], sys.argv[2], sys.argv[3]
sys.path[:0] = [repo, tests]
sys.path.append(tree)
from unittest import mock
from runtime import sglang_entry as entry
from runtime import sglang_native_composition as composition
from runtime import sglang_server_args as server_args
import test_sglang_entry as fixtures

fixture = fixtures.LaunchFixture()
fixture.setUp()
settings = fixture.public["settings"]
settings["memory_saver"] = False
settings["cpu_weight_backup"] = False
settings["weight_restore"] = "disk_reload"

def guarded_import():
    return (importlib.import_module("sglang.srt.server_args"),
            importlib.import_module("sglang.srt.entrypoints.http_server"))

with mock.patch.object(composition, "enforce_closed_plugins"), \
        mock.patch.object(entry, "_guarded_engine_import", side_effect=guarded_import), \
        mock.patch.object(server_args, "construct_server_args", return_value=mock.Mock()):
    status = entry.main(fixture.argv(), fixture.payloads().__getitem__)
# Whether multiprocessing's resource tracker still holds its pipe, and so has
# a process its finalizer will wait for at interpreter exit. Python 3.12's
# finalizer prints a ChildProcessError when the process was reaped by another.
from multiprocessing import resource_tracker
print("tracker_pipe:", "open" if resource_tracker._resource_tracker._fd is not None else "closed")
raise SystemExit(status)
"""


def running_group_members(group):
    """Processes in `group` that have not exited (zombies excluded)."""
    members = []
    for name in os.listdir("/proc"):
        if not name.isdigit():
            continue
        try:
            raw = Path("/proc", name, "stat").read_bytes()
        except OSError:
            continue
        fields = raw[raw.rindex(b")") + 2:].split()
        if int(fields[2]) == group and fields[0] not in (b"Z", b"X"):
            members.append(int(name))
    return members


def running(pid):
    try:
        raw = Path("/proc", str(pid), "stat").read_bytes()
    except OSError:
        return False
    return raw[raw.rindex(b")") + 2:].split()[0] not in (b"Z", b"X")


class SchedulerDeathBeforeReadinessTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="capyctl-sglang-death-")
        self.addCleanup(self.directory.cleanup)
        root = Path(self.directory.name)
        self.tree = root / "venv" / "site-packages"
        sglang_tree(self.tree, saver_hooks=False)
        (self.tree / "sglang/srt/entrypoints/http_server.py").write_text(
            textwrap.dedent(HTTP_SERVER))
        self.temp = root / "tmp"
        self.temp.mkdir()
        self.record = root / "detokenizer"

    def run_entry(self):
        environment = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"),
                       "TMPDIR": str(self.temp), "STUB_RECORD": str(self.record)}
        started = time.monotonic()
        # The launcher makes the entry its own process group's leader.
        entry = subprocess.Popen(
            [sys.executable, "-I", "-S", "-c", ENTRY_SCRIPT, str(REPO), str(TESTS),
             str(self.tree)],
            env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            start_new_session=True)
        try:
            stdout, stderr = entry.communicate(timeout=60)
        except subprocess.TimeoutExpired:
            os.killpg(entry.pid, signal.SIGKILL)
            entry.communicate()
            self.fail("the entry outlived its failed launch")
        return entry, stdout, stderr, time.monotonic() - started

    # T12: SPEC §§6.4, 13.2. A scheduler that exits before readiness,
    # without signalling its parent, ends the protected entry within seconds
    # with its closed category, and nothing it started is left running: the
    # live detokenizer and its own child are ended before the entry exits.
    def test_scheduler_death_before_readiness_fails_promptly_and_leaves_no_process(self):
        entry, stdout, stderr, elapsed = self.run_entry()
        self.assertEqual(entry.returncode, 1, stderr)
        self.assertLess(elapsed, 20, stderr)
        self.assertEqual(stderr.count("sglang_startup_failed"), 1, stderr)
        # Found live 2026-10-22: the failure line is the log's last; nothing
        # the interpreter does at exit follows it. multiprocessing's resource
        # tracker reaped its own process, so its finalizer, which on Python
        # 3.12 printed a ChildProcessError there, has nothing to wait for.
        self.assertTrue(stderr.endswith("\n"), stderr)
        self.assertTrue(stderr.splitlines()[-1].startswith(
            "sglang_startup_failed: engine_launch_failed ("), stderr)
        self.assertIn("tracker_pipe: closed", stdout, stdout)
        # The process group is gone at the moment its leader is.
        self.assertEqual(running_group_members(entry.pid), [])
        detokenizer, helper, prometheus = self.record.read_text().split(" ", 2)
        self.assertFalse(running(int(detokenizer)))
        self.assertFalse(running(int(helper)))
        # The parent removed its prometheus directory only after every child
        # that writes into it had exited, so none of them saw it missing.
        self.assertNotIn("FileNotFoundError", stderr)
        self.assertFalse(Path(prometheus).exists())


# A scheduler that, like SGLang 0.5.22's, makes its load snapshot file
# (`/dev/shm/sglang_loads_*.shm`, opened and mapped, never removed) and a
# multiprocessing segment (registered with the resource tracker), maps a
# segment that existed before the launch, and makes one more that a process
# outside the launch maps. It is then killed while it loads.
SHM_HTTP_SERVER = '''
import mmap
import multiprocessing as mp
import os
import subprocess
import sys
import time
from multiprocessing import shared_memory

class _Route:
    def __init__(self, path):
        self.path = path
class _App:
    routes = [_Route("/generate"), _Route("/v1/models")]
app = _App()

HOLDER = """
import mmap, os, sys, time
if os.fork():
    os._exit(0)
fd = os.open(sys.argv[1], os.O_RDWR)
mapped = mmap.mmap(fd, 4096)
with open(sys.argv[2] + ".part", "w") as handle:
    handle.write(str(os.getpid()))
os.rename(sys.argv[2] + ".part", sys.argv[2])
time.sleep(600)
"""

def _mapped(path, create):
    fd = os.open(path, (os.O_CREAT | os.O_RDWR) if create else os.O_RDWR, 0o600)
    os.ftruncate(fd, 4096)
    return fd, mmap.mmap(fd, 4096)

def _scheduler(token, scratch):
    kept = [_mapped("/dev/shm/sglang_loads_%s_0.shm" % token, True),
            shared_memory.SharedMemory(name="%s-psm" % token, create=True, size=4096),
            _mapped("/dev/shm/%s-old" % token, False),
            _mapped("/dev/shm/%s-shared" % token, True)]
    holder = os.path.join(scratch, "holder")
    subprocess.Popen([sys.executable, "-I", "-S", "-c", HOLDER,
                      "/dev/shm/%s-shared" % token, holder],
                     stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                     stderr=subprocess.DEVNULL, start_new_session=True,
                     close_fds=True).wait()
    while not os.path.exists(holder):
        time.sleep(0.05)
    record = os.path.join(scratch, "scheduler")
    with open(record + ".part", "w") as handle:
        handle.write(str(os.getpid()))
    os.rename(record + ".part", record)
    time.sleep(600)

def launch_server(server_args, run_scheduler_process_func=None):
    mp.set_start_method("spawn", force=True)
    reader, writer = mp.Pipe(duplex=False)
    scheduler = mp.Process(target=_scheduler,
                           args=(os.environ["STUB_TOKEN"], os.environ["STUB_SCRATCH"]))
    scheduler.start()
    while not reader.poll(timeout=0.2):
        if not scheduler.is_alive():
            raise RuntimeError("Rank 0 scheduler died during initialization")
'''


class SharedMemoryAfterFailedLaunchTests(unittest.TestCase):
    def setUp(self):
        if not os.path.isdir("/dev/shm"):
            self.skipTest("no /dev/shm")
        self.directory = tempfile.TemporaryDirectory(prefix="capyctl-sglang-shm-")
        self.addCleanup(self.directory.cleanup)
        root = Path(self.directory.name)
        self.tree = root / "venv" / "site-packages"
        sglang_tree(self.tree, saver_hooks=False)
        (self.tree / "sglang/srt/entrypoints/http_server.py").write_text(
            textwrap.dedent(SHM_HTTP_SERVER))
        self.temp = root / "tmp"
        self.temp.mkdir()
        self.scratch = root / "scratch"
        self.scratch.mkdir()
        self.token = "capyctl-test-%d-%d" % (os.getpid(), time.monotonic_ns())
        self.addCleanup(self.remove_segments)

    def segment(self, suffix):
        return Path("/dev/shm", self.token + suffix)

    def remove_segments(self):
        holder = self.scratch / "holder"
        if holder.exists():
            try:
                os.kill(int(holder.read_text()), signal.SIGKILL)
            except ProcessLookupError:
                pass
        for path in Path("/dev/shm").glob("*" + self.token + "*"):
            path.unlink(missing_ok=True)

    def wait_for(self, path, entry):
        deadline = time.monotonic() + 30
        while not path.exists():
            if entry.poll() is not None or time.monotonic() > deadline:
                os.killpg(entry.pid, signal.SIGKILL)
                self.fail("the stub scheduler never started: %s" % (entry.communicate(),))
            time.sleep(0.05)

    # T12: SPEC §13.2 (found live 2026-10-22). A scheduler killed while it
    # loads fails the launch, and the shared memory the launch made, which
    # no live process maps, is removed with it: SGLang's load snapshot file
    # and a segment the killed resource tracker would have removed. A segment
    # that existed before the launch, one a process outside the launch maps,
    # and one another process made during the launch all stay.
    def test_a_failed_launch_removes_the_shared_memory_it_left(self):
        old = self.segment("-old")
        old.write_bytes(b"\0" * 4096)
        old.chmod(0o600)
        environment = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"),
                       "TMPDIR": str(self.temp), "STUB_TOKEN": self.token,
                       "STUB_SCRATCH": str(self.scratch)}
        entry = subprocess.Popen(
            [sys.executable, "-I", "-S", "-c", ENTRY_SCRIPT, str(REPO), str(TESTS),
             str(self.tree)],
            env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            start_new_session=True)
        self.wait_for(self.scratch / "scheduler", entry)
        foreign = self.segment("-foreign")
        foreign.write_bytes(b"\0" * 4096)
        foreign.chmod(0o600)
        # Long enough for the watch to list the directory several times.
        time.sleep(1.0)
        os.kill(int((self.scratch / "scheduler").read_text()), signal.SIGKILL)
        try:
            _, stderr = entry.communicate(timeout=60)
        except subprocess.TimeoutExpired:
            os.killpg(entry.pid, signal.SIGKILL)
            entry.communicate()
            self.fail("the entry outlived its failed launch")
        self.assertEqual(entry.returncode, 1, stderr)
        self.assertTrue(stderr.splitlines()[-1].startswith(
            "sglang_startup_failed: engine_launch_failed ("), stderr)
        self.assertEqual(running_group_members(entry.pid), [])
        self.assertFalse(Path("/dev/shm/sglang_loads_%s_0.shm" % self.token).exists())
        self.assertFalse(self.segment("-psm").exists())
        self.assertTrue(old.exists())
        self.assertTrue(self.segment("-shared").exists())
        self.assertTrue(foreign.exists())


if __name__ == "__main__":
    unittest.main()
