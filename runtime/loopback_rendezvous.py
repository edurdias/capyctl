"""Loopback-only engine rendezvous for single-rank launches.

SPEC §8.2: capyctl controls bind addresses, private ports, distributed ranks and
rendezvous data. SPEC §9.1 / T21 (ADR 0012): every engine listener stays on
loopback. Found live 2026-09-23 (M08): SGLang 0.5.20's scheduler initialises
torch.distributed with `tcp://127.0.0.1:<nccl_port>`, and torch 2.13's TCPStore
server binds that port on every interface regardless of the host in the URL
(checked on host-a: `TCPStore("127.0.0.1", ...)` listens on `*:<port>`). The
store accepted a connection from another host. No bind address or port choice
fixes that, so a single-rank launch uses a file rendezvous instead, the same
FileStore vLLM already uses on CUDA (`get_file_store_init_method`), and opens
no rendezvous listener at all. Gloo and NCCL socket interfaces are pinned to
loopback so any group transport a single rank still opens stays local.

The entry calls `pin` before any engine import and `verify` immediately before
handing control to the engine. `pin` overwrites every rendezvous input capyctl owns
and removes the ones it does not render, so an inherited value never wins; any
drift before `verify` refuses the launch.
This module imports nothing from an engine and opens no socket.
"""

import atexit
import os
import shutil
import tempfile

# Group transports: a single rank's gloo/NCCL groups bind loopback only.
_INTERFACE_ENV = {"GLOO_SOCKET_IFNAME": "lo", "NCCL_SOCKET_IFNAME": "lo"}
# SGLang 0.5.20 `_resolve_dist_init_method` (distributed/bootstrap.py) takes
# this override before `dist_init_addr` and the default TCP rendezvous.
SGLANG_OVERRIDE = "SGLANG_DISTRIBUTED_INIT_METHOD_OVERRIDE"
# Rendezvous inputs that would steer torch's `env://` store or vLLM's address
# resolution; capyctl renders none of them, so they are removed.
_REMOVED = ("MASTER_ADDR", "MASTER_PORT", "HOST_IP")
# SPEC §8.2 / T21 (found live 2026-09-23): the host names each launch's
# directory inside its private root and removes it once the group is gone,
# because a signalled stop never runs this interpreter's exit handlers.
HOST_DIR = "CAPYCTL_RENDEZVOUS_DIR"
# Every rendezvous input capyctl renders for either engine.
_OWNED = (*_INTERFACE_ENV, SGLANG_OVERRIDE, "VLLM_HOST_IP")


class RendezvousError(Exception):
    """Closed category; never carries environment values."""

    def __init__(self):
        super().__init__("loopback_rendezvous_failed")
        self.code = "loopback_rendezvous_failed"


def expected_environment(engine, store_dir=None):
    """The exact variables a single-rank launch of `engine` runs with."""
    expected = dict(_INTERFACE_ENV)
    if engine == "sglang":
        if type(store_dir) is not str or not os.path.isabs(store_dir):
            raise RendezvousError()
        expected[SGLANG_OVERRIDE] = "file://" + os.path.join(store_dir, "store")
    elif engine == "vllm":
        # vLLM's own TCP fallback (ROCm AITER, ray) resolves its address here.
        expected["VLLM_HOST_IP"] = "127.0.0.1"
    else:
        raise RendezvousError()
    return expected


def _host_named(path):
    """Create the host-named directory: absolute, fresh, 0700, in a private
    parent this user owns. Anything else refuses; nothing is reused."""
    if type(path) is not str or not os.path.isabs(path):
        raise OSError()
    parent = os.lstat(os.path.dirname(path))
    if (not os.path.isdir(os.path.dirname(path)) or os.path.islink(os.path.dirname(path))
            or parent.st_uid != os.geteuid() or parent.st_mode & 0o077):
        raise OSError()
    os.mkdir(path, 0o700)
    return path


def pin(engine, environ=None, make_dir=None, cleanup=True):
    """Set the loopback rendezvous for `engine`; returns what `verify` checks.

    SGLang gets a fresh owner-only (0700) directory for its file store: the
    one the host names in `CAPYCTL_RENDEZVOUS_DIR` (the host removes it once the
    group is gone), else a temporary one. Either is also removed at a normal
    interpreter exit. Inherited values of owned or removed inputs are
    replaced or dropped, never honoured.
    """
    environ = os.environ if environ is None else environ
    host_dir = environ.get(HOST_DIR)
    if make_dir is None and host_dir is not None:
        make_dir = lambda: _host_named(host_dir)  # noqa: E731
    for name in (*_OWNED, *_REMOVED):
        environ.pop(name, None)
    store_dir = None
    if engine == "sglang":
        try:
            store_dir = (make_dir or (lambda: tempfile.mkdtemp(prefix="capyctl-rdzv-")))()
            if os.stat(store_dir).st_mode & 0o077:
                raise OSError()
        except OSError:
            raise RendezvousError() from None
        if cleanup:
            atexit.register(shutil.rmtree, store_dir, True)
    expected = expected_environment(engine, store_dir)
    environ.update(expected)
    return expected


def verify(expected, environ=None):
    """Recheck, right before the engine starts, that nothing changed."""
    environ = os.environ if environ is None else environ
    try:
        if not expected or any(environ.get(name) != value
                                for name, value in expected.items()):
            raise ValueError()
        if any(environ.get(name) != expected.get(name)
               for name in (*_OWNED, *_REMOVED)):
            raise ValueError()
    except Exception:
        raise RendezvousError() from None
