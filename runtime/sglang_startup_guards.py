"""Process-lifetime startup guards; stdlib only, never launch authority.

Call preimport_guard() in the fresh service-owned child BEFORE native imports or
ServerArgs construction. SPEC §13.3: engine output is kept in the launch's
private (0600) log at SGLang's default level. The launcher pipes descriptors 1
and 2 into capyctl's redacting log writer, which replaces the launch's keys by
value and credential shapes by rule, so native writes are covered too; the
scrubber installed here additionally scrubs Python-level records and streams.
Request logging stays off (log_requests=False, sglang_server_args.py). Explicit
operator --debug-engine-logs raises the level to debug and writes full private
output without the writer. Failure reporting must use fixed exit/status
categories, not native exception text or arguments.

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
same preimport plugin check; the fd 1/2 pipe to the log writer is inherited by them.
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
import sys


class StartupGuardError(Exception):
    """Only fixed public categories; never render intercepted native details."""

    def __init__(self, code):
        if code not in ("native_already_imported",
                        "external_plugin_selection", "external_plugins_present",
                        "plugin_inventory_unavailable"):
            code = "startup_guard_failed"
        self.code = code
        super().__init__(code)


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
    """SPEC §13.3 / T21: engine logs never carry a credential.

    The engine formats its arguments (keys included) into log messages, so every
    log record's message is scrubbed as it is created, and Python-level writes
    to stdout and stderr are scrubbed too. Native writes to the descriptors are
    redacted by capyctl's log writer on the other end of the pipe; under
    --debug-engine-logs they are not, and the log file is private (0600).
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

    Runs install_log_scrubber() then enforce_closed_plugins(), in that order,
    with or without --debug-engine-logs. This runs at the top of a spawned
    interpreter's main before native Process arguments are unpickled. Raises a
    closed StartupGuardError category; the caller exits without rendering
    native details. The selector rejection and installed entry-point inventory
    observation in enforce_closed_plugins() are the complete in-child posture:
    trusted immutable package/code/metadata/search paths (including children),
    the clean isolated interpreter and the closed environment remain launcher
    prerequisites that no child can self-attest.
    """
    install_log_scrubber()
    enforce_closed_plugins()
