"""Process-lifetime startup guards; stdlib only, never launch authority.

Call contain_startup_output() in the fresh service-owned child BEFORE native
imports or ServerArgs construction, then enforce_closed_plugins(). Never restore
stdout/stderr: native buffers, threads and atexit handlers can outlive startup.
Output is deliberately discarded, not heuristically redacted. Failure reporting
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

Pinned plugin contract fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1:
plugins.load_plugins_by_group discovers importlib.metadata entry_points for both
sglang.srt.plugins and sglang.srt.platforms. platforms._resolve_platform also has
a direct entry_points(...).load() path when SGLANG_PLATFORM is selected. Empty
SGLANG_PLUGINS means unrestricted, NOT disabled. Reject BOTH installed groups and
nonempty selectors without importing entry-point targets. Thus the pinned engine's
load_plugins and platform defaults have no external plugin to execute, provided
the immutable metadata/source preconditions hold. This does not deny built-in
hooks, arbitrary package imports, or unreviewed/new loader paths. Source preflight
must include the pinned plugins/platforms initializers, and be revalidated before
imports. Source-file checks alone do not attest installed entry-point metadata or
the complete import graph and cannot satisfy the immutable-package prerequisite.
"""

import importlib.metadata
import os
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
