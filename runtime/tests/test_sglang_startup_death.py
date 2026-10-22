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
            _, stderr = entry.communicate(timeout=60)
        except subprocess.TimeoutExpired:
            os.killpg(entry.pid, signal.SIGKILL)
            entry.communicate()
            self.fail("the entry outlived its failed launch")
        return entry, stderr, time.monotonic() - started

    # T12: SPEC §§6.4, 13.2. A scheduler that exits before readiness,
    # without signalling its parent, ends the protected entry within seconds
    # with its closed category, and nothing it started is left running: the
    # live detokenizer and its own child are ended before the entry exits.
    def test_scheduler_death_before_readiness_fails_promptly_and_leaves_no_process(self):
        entry, stderr, elapsed = self.run_entry()
        self.assertEqual(entry.returncode, 1, stderr)
        self.assertLess(elapsed, 20, stderr)
        self.assertIn("sglang_startup_failed: engine_launch_failed (", stderr)
        self.assertEqual(stderr.count("sglang_startup_failed"), 1, stderr)
        # The process group is gone at the moment its leader is.
        self.assertEqual(running_group_members(entry.pid), [])
        detokenizer, helper, prometheus = self.record.read_text().split(" ", 2)
        self.assertFalse(running(int(detokenizer)))
        self.assertFalse(running(int(helper)))
        # The parent removed its prometheus directory only after every child
        # that writes into it had exited, so none of them saw it missing.
        self.assertNotIn("FileNotFoundError", stderr)
        self.assertFalse(Path(prometheus).exists())


if __name__ == "__main__":
    unittest.main()
