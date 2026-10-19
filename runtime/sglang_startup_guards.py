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
the trusted package/metadata paths without executing .pth files or site.main()
(trusted_search_path: the verified site-packages and one named data directory
inside it).
An empty metadata inventory under -S does not attest the installed environment.
Do not enable native file/request/crash logging. Spawned interpreters must run the
same preimport plugin check; the fd 1/2 pipe to the log writer is inherited by them.
CPython spawn hands each child the parent's sys.path before it runs the main
script, so the composed search path reaches the scheduler and detokenizer.
The protected sglang_entry script repeats these guards during CPython spawn's
__mp_main__ preparation, before native Process arguments are unpickled. Keeping
that exact protected main path and spawn method is a startup composition duty;
guarding only the scheduler target cannot protect argument-class imports.

Pinned plugin contract 94602c9c2b7cbdb8efd5c52802dac6a1c180089e:
plugins.load_plugins_by_group discovers importlib.metadata entry_points for both
sglang.srt.plugins and sglang.srt.platforms. platforms._resolve_platform also has
a direct entry_points(...).load() path when SGLANG_PLATFORM is selected. Empty
SGLANG_PLUGINS means unrestricted, NOT disabled. Reject the installed platforms
group and a nonempty SGLANG_PLATFORM without importing entry-point targets. Thus
the engine's load_plugins and platform defaults have no external plugin to
execute, provided the installed metadata is what the host registered. This does
not deny built-in hooks, arbitrary package imports, or unreviewed/new loader
paths. ADR 0008 (owner decision 2026-09-23): no pinned source audit backs this
any more; the installation's registered fingerprint and drift flag, and the
launch-time capability probes (engine_capabilities.py), take its place, and
neither attests the complete import graph.

ADR 0008 amendment 2026-10-17: SGLang 0.5.22 requires smg-grpc-servicer
(Requires-Dist smg-grpc-servicer>=0.9.0), whose 0.14.0 registers two
sglang.srt.plugins entry points (smg-servicer, smg-zmq), so every 0.5.22
installation has plugins. Read on an installed 0.5.22 that day,
sglang/srt/plugins/__init__.py::load_plugins_by_group (0.5.20 and 0.5.21 read
the same): a nonempty SGLANG_PLUGINS is a comma-separated whitelist of
entry-point names, and an entry point whose name is not in it is skipped
before ep.load(); the diffusion loader reads the same variable. CapyCTL renders
SGLANG_PLUGINS=NO_PLUGINS_SELECTION into every SGLang process it starts, a name
no entry point may carry, so the loader imports none. Installed general
plugins are accepted only under exactly that selection, and only when each
comes from a distribution SGLang itself requires (REQUIRED_PLUGIN_DISTRIBUTIONS);
an unknown distribution's plugin is still refused. Any SMG_* variable is
refused too: smg's zmq plugin activates on SMG_SGLANG_ZMQ_HANDSHAKE. The
distribution name is the installed metadata's claim, attested only as far as
the registered fingerprint attests the installation.
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
        if code not in ("native_already_imported",
                        "external_plugin_selection", "external_plugins_present",
                        "plugin_inventory_unavailable"):
            code = "startup_guard_failed"
        self.code = code
        super().__init__(code)


# ADR 0008 amendment 2026-10-17: the SGLANG_PLUGINS value CapyCTL renders
# (`capyctl_config::engine_env::SGLANG_NO_PLUGINS`); a whitelist naming no
# entry point, so SGLang's loader loads none.
NO_PLUGINS_SELECTION = "capyctl-loads-no-plugins"
# Distributions SGLang requires that register sglang.srt.plugins entry points
# (SGLang 0.5.22: Requires-Dist smg-grpc-servicer>=0.9.0). Normalized names.
REQUIRED_PLUGIN_DISTRIBUTIONS = frozenset({"smg-grpc-servicer"})
# smg's own settings; its zmq plugin activates on SMG_SGLANG_ZMQ_HANDSHAKE.
_PLUGIN_SETTING_PREFIX = "SMG_"
# ADR 0008 amendment 2026-10-19: nvidia-cutlass-dsl (SGLang 0.5.22 requires
# it) installs its `cutlass` package under
# <site-packages>/nvidia_cutlass_dsl/dsl_packages and reaches it only through
# a .pth code hook (`nvidia_cutlass_dsl_packages.pth`), which -IS never runs.
CUTLASS_DSL_PACKAGES = ("nvidia_cutlass_dsl", "dsl_packages")


def trusted_search_path(site_packages):
    """The search entries composed for a verified site-packages, in append order.

    SPEC §9.2, ADR 0008 amendment 2026-10-19: the tree itself, then its
    nvidia-cutlass-dsl package directory, the one entry a vendor .pth hook
    would add that SGLang needs (0.5.22's dual GEMM imports `cutlass`). It is
    added only when both components are real directories (lstat: neither is a
    symlink, so the entry cannot leave the verified tree), and it is a path,
    never code: no .pth file is read or run and nothing is imported here.
    Appended after the tree, it shadows neither the standard library, the
    protected runtime package nor the tree's own modules; the plugin closure
    enumerates its metadata like any other entry. No other .pth path is
    honoured.
    """
    entries = [site_packages]
    path = site_packages
    for part in CUTLASS_DSL_PACKAGES:
        path = os.path.join(path, part)
        try:
            if not stat.S_ISDIR(os.lstat(path).st_mode):
                return entries
        except OSError:
            return entries
    entries.append(path)
    return entries



def _required_plugin(point):
    """Whether an installed general plugin is one SGLang itself requires."""
    if point.name.strip() == NO_PLUGINS_SELECTION or point.dist is None:
        return False
    name = re.sub(r"[-_.]+", "-", point.dist.name).lower()
    return name in REQUIRED_PLUGIN_DISTRIBUTIONS


def enforce_closed_plugins():
    """Preimport observation of no loadable external plugin, not an enduring capability.

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
    selection = os.environ.get("SGLANG_PLUGINS")
    if (os.environ.get("SGLANG_PLATFORM")
            or (selection and selection != NO_PLUGINS_SELECTION)
            or any(name.upper().startswith(_PLUGIN_SETTING_PREFIX) for name in os.environ)):
        raise StartupGuardError("external_plugin_selection") from None
    try:
        if tuple(importlib.metadata.entry_points(group="sglang.srt.platforms")):
            raise StartupGuardError("external_plugins_present")
        plugins = tuple(importlib.metadata.entry_points(group="sglang.srt.plugins"))
        if plugins and (selection != NO_PLUGINS_SELECTION
                        or not all(_required_plugin(point) for point in plugins)):
            raise StartupGuardError("external_plugins_present")
    except StartupGuardError:
        raise
    except Exception:
        raise StartupGuardError("plugin_inventory_unavailable") from None


# SPEC §13.3 / T21: a bearer value, or a run of credential-shaped characters as
# long as capyctl's keys (64 hex) or longer than any ordinary token. Mirrors
# `crates/capyctl-adapters/src/vllm/args.rs::redact_text`: a run that begins
# with `/` is a path, and so is the rest of its word past any `.`, `@` or `:`;
# a path is kept segment by segment, so only a credential-length segment inside
# it is redacted. Found live
# 2026-10-14: SGLang's `server_args` line showed
# `/home/u/.cache/huggingface/hub/models--RadixArk--Qwen3.<redacted>`.
_BEARER = re.compile(r"(?i)(bearer\s+)[^\s'\",;]+")
_WORD_END = re.compile(r"([\s\x00-\x1f\x7f\"'`,;()\[\]{}<>])")
_RUN = re.compile(r"[A-Za-z0-9+/=_-]+")
_SECRET_RUN_MIN = 48


def _scrub_word(word):
    in_path = False

    def run(match):
        nonlocal in_path
        text = match.group(0)
        if in_path or text.startswith("/"):
            in_path = True
            return "/".join("<redacted>" if len(segment) >= _SECRET_RUN_MIN else segment
                            for segment in text.split("/"))
        return "<redacted>" if len(text) >= _SECRET_RUN_MIN else text

    return _RUN.sub(run, word)


def scrub(text):
    """The text with bearer values and credential-shaped runs redacted."""
    text = _BEARER.sub(lambda match: match.group(1) + "<redacted>", text)
    return "".join(_scrub_word(part) for part in _WORD_END.split(text))


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
