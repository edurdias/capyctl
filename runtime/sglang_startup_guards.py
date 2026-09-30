"""Process-lifetime startup guards; stdlib only, never launch authority.

Call contain_startup_output() in the fresh service-owned child BEFORE native
imports or ServerArgs construction, then enforce_closed_plugins(). Never restore
stdout/stderr: native buffers, threads and atexit handlers can outlive startup.
Output is discarded by default, not heuristically redacted. Explicit operator
--debug-engine-logs retains full private output for development. Failure reporting
must use fixed exit/status categories, not native exception text or arguments.

These are narrow controls, not a sandbox. The launcher must supply a clean isolated
interpreter, no inherited alternate output FDs/handlers, a closed environment, and
trusted immutable package/code/metadata/search paths (including children). No file,
network, syslog or explicitly reopened terminal channel is intercepted here.
The renderer disables automatic site initialization with -IS: -I alone still
executes installed .pth and sitecustomize hooks before this module can guard them.
Before plugin inventory and native imports, startup must explicitly establish
the trusted package/metadata paths without executing .pth files or site.main().
An empty metadata inventory under -S does not attest the installed environment.
Do not enable native file/request/crash logging. Spawned interpreters must run the
same preimport plugin check; fd 1/2 suppression survives fork and exec by itself.
The protected sglang_entry script repeats these guards during CPython spawn's
__mp_main__ preparation, before native Process arguments are unpickled. Keeping
that exact protected main path and spawn method is a startup composition duty;
guarding only the scheduler target cannot protect argument-class imports.

Pinned plugin contract 94602c9c2b7cbdb8efd5c52802dac6a1c180089e:
plugins.load_plugins_by_group discovers importlib.metadata entry_points for both
sglang.srt.plugins and sglang.srt.platforms. platforms._resolve_platform also has
a direct entry_points(...).load() path when SGLANG_PLATFORM is selected. Empty
SGLANG_PLUGINS means unrestricted, NOT disabled. Reject BOTH installed groups and
nonempty selectors without importing entry-point targets. Thus the engine's
load_plugins and platform defaults have no external plugin to execute, provided
the installed metadata is what the host registered. This does not deny built-in
hooks, arbitrary package imports, or unreviewed/new loader paths. ADR 0008
(owner decision 2026-09-23): no pinned source audit backs this any more; the
installation's registered fingerprint and drift flag, and the launch-time
capability probes (engine_capabilities.py), take its place, and neither attests
the complete import graph.
"""

import importlib.metadata
import logging
import os
import re
import stat
import sys


class StartupGuardError(Exception):
    """Only fixed public categories; never render intercepted native details."""

    def __init__(self, code):
        if code not in ("output_containment_failed", "native_already_imported",
                        "external_plugin_selection", "external_plugins_present",
                        "plugin_inventory_unavailable"):
            code = "startup_guard_failed"
        self.code = code
        super().__init__(code)


def contain_startup_output():
    """Irreversibly redirect stdout/stderr at OS descriptor level on Linux.

    Call once in a fresh, single-threaded launcher process. No original descriptor
    is retained. Do not flush first: buffered secret output must also be discarded.
    Failures require immediate process termination, never proceeding to imports.
    This function intentionally is not a context manager.
    """
    sink = None
    try:
        if not sys.platform.startswith("linux"):
            raise OSError()
        sink = os.open("/dev/null", os.O_WRONLY | os.O_CLOEXEC | os.O_NOFOLLOW)
        info = os.fstat(sink)
        if not stat.S_ISCHR(info.st_mode) or info.st_rdev != os.makedev(1, 3):
            raise OSError()
        # dup2 clears CLOEXEC: spawned native children inherit suppression.
        os.dup2(sink, 1, inheritable=True)
        os.dup2(sink, 2, inheritable=True)
    except OSError:
        raise StartupGuardError("output_containment_failed") from None
    finally:
        if sink is not None and sink not in (1, 2):
            os.close(sink)


def enforce_closed_plugins():
    """Preimport observation of no external plugins, not an enduring capability.

    Metadata enumeration loads no entry-point targets. Failure to enumerate is a
    denial. Caller must independently attest immutable packages, source and search
    paths and prevent mutations between this observation and all native imports.
    No caller-supplied inventory or configurable allowlist can weaken this policy.
    """
    # Native dependencies can execute effects before SGLang itself is imported.
    # Refuse a late guard; inspecting module names imports none of these packages.
    native_roots = ("sglang", "torch", "transformers", "torch_memory_saver")
    if any(name == root or name.startswith(root + ".")
           for name in sys.modules for root in native_roots):
        raise StartupGuardError("native_already_imported") from None
    if any(os.environ.get(name) for name in ("SGLANG_PLUGINS", "SGLANG_PLATFORM")):
        raise StartupGuardError("external_plugin_selection") from None
    try:
        for group in ("sglang.srt.platforms", "sglang.srt.plugins"):
            if tuple(importlib.metadata.entry_points(group=group)):
                raise StartupGuardError("external_plugins_present")
    except StartupGuardError:
        raise
    except Exception:
        raise StartupGuardError("plugin_inventory_unavailable") from None


# SPEC §13.3 / T21: a bearer value, or a run of credential-shaped characters as
# long as capyctl's keys (64 hex) or longer than any ordinary token. Mirrors
# `crates/capyctl-adapters/src/vllm/args.rs::redact_text`.
_CREDENTIAL = re.compile(r"(?i)(bearer\s+)[^\s'\",;]+|[A-Za-z0-9+/=_-]{48,}")


def scrub(text):
    """The text with bearer values and credential-shaped runs redacted."""
    return _CREDENTIAL.sub(lambda match: (match.group(1) or "") + "<redacted>", text)


class _ScrubbingStream:
    """A text stream whose Python-level writes are scrubbed (SPEC §13.3)."""

    def __init__(self, inner):
        self._inner = inner

    def write(self, text):
        self._inner.write(scrub(text) if type(text) is str else text)
        return len(text)

    def __getattr__(self, name):
        return getattr(self._inner, name)


def install_log_scrubber():
    """SPEC §13.3 / T21: debug engine logs never carry a credential.

    The engine formats its arguments (keys included) into log messages, so every
    log record's message is scrubbed as it is created, and Python-level writes
    to stdout and stderr are scrubbed too. Native writes to the descriptors are
    not intercepted; the log file is private (0600) either way.
    """
    factory = logging.getLogRecordFactory()

    def scrubbed(*args, **kwargs):
        record = factory(*args, **kwargs)
        try:
            message = scrub(record.getMessage())
        except Exception:
            message = "<unformattable log message>"
        record.msg, record.args = message, None
        return record

    logging.setLogRecordFactory(scrubbed)
    sys.stdout = _ScrubbingStream(sys.stdout)
    sys.stderr = _ScrubbingStream(sys.stderr)


def preimport_guard():
    """One-call preimport safety for a freshly spawned interpreter.

    Normally runs contain_startup_output() then enforce_closed_plugins(), in that order.
    Explicit debug opt-in skips output containment only. This runs
    at the top of a spawned interpreter's main before native Process arguments
    are unpickled. Raises a closed StartupGuardError category; the caller exits
    without rendering native details. The selector rejection and installed
    entry-point inventory observation in enforce_closed_plugins() are the
    complete in-child posture: trusted immutable package/code/metadata/search
    paths (including children), the clean isolated interpreter and the closed
    environment remain launcher prerequisites that no child can self-attest.
    """
    # Explicit operator development opt-in; plugin checks remain mandatory, and
    # the retained output is scrubbed of credentials (SPEC §13.3).
    if os.environ.get("CAPYCTL_DEBUG_ENGINE_LOGS") != "1":
        contain_startup_output()
    else:
        install_log_scrubber()
    enforce_closed_plugins()
