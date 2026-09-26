"""Protected vLLM startup boundary (ADR 0014 §6, owner decision Q11).

mllm launches `python vllm_entry.py serve <model> <reserved> --mllm-user-args
<typed, host-fixed and extra arguments>`. This entry:

1. refuses, before any engine import, user tokens that could load a
   configuration file (SPEC §8.2: a file would hide values), print help or a
   version and exit, or impersonate the marker;
2. parses the reserved block alone and the complete vector with vLLM's own
   parser (the `vllm serve` subcommand parser of the installed build), so
   abbreviations, underscores, `=value`, dotted JSON keys, negations and
   aliases resolve exactly as vLLM resolves them;
3. compares every reserved destination of the two results; any difference,
   or a reserved destination the installed parser does not have, is a closed
   `effective_args_mismatch` (ADR 0014 open issue 4: drift fails closed);
4. validates and runs the server in this process from the very namespace it
   checked, so nothing is parsed twice with different rules.

The guard middleware and development-mode environment are unchanged (ADR 0012):
mllm renders `--middleware mllm_vllm_guard.RequireEngineKey` in the reserved
block whenever sleep mode is on, and the key rides `VLLM_API_KEY`.

Engine multiprocessing may re-import this file as `__mp_main__`; the module
body therefore has no effects. Passing tests here are CPU fakes, never evidence
that a vLLM build serves a model.
"""

import os
import sys

MARKER = "--mllm-user-args"
# ADR 0014 §8, SPEC §8.2: the deployment's own extra arguments follow this
# second marker (typed and host-fixed arguments precede it), so the entry can
# gate exactly the destinations they set, as vLLM's parser resolved them.
EXTRA_MARKER = "--mllm-extra-args"

# ADR 0014 §3: vLLM 0.29.0 parser destinations mllm owns. Names were read from
# the installed build (vllm/engine/arg_utils.py, vllm/entrypoints/launchers/
# cli_args.py). `--device`, `--swap-space` and `--disable-log-requests` no
# longer exist in 0.29.0; the parser itself refuses them as unknown.
RESERVED = (
    "model_tag", "model", "host", "port", "uds", "root_path", "api_key", "middleware",
    "served_model_name", "tensor_parallel_size", "pipeline_parallel_size",
    "gpu_memory_utilization", "cpu_offload_gb", "kv_cache_memory_bytes",
    "enable_sleep_mode", "enable_log_requests", "disable_log_stats",
    "log_config_file", "uvicorn_log_level", "disable_uvicorn_access_log",
    "distributed_executor_backend", "headless", "api_server_count", "revision",
    "code_revision", "enable_ssl_refresh", "config", "grpc",
)
# SPEC §8.2 / T21: multi-node rendezvous (0.26+ `ParallelConfig`). Compared
# whenever the installed parser defines them; unlike RESERVED, a build without
# them is not drift, since no argument can then reach them.
RESERVED_IF_PRESENT = ("nnodes", "node_rank", "master_addr", "master_port")
# ADR 0014 §2: destinations the typed fields render (engine_policy.rs
# VLLM_TYPED_OPTIONS); an extra argument may not restate or reverse them.
TYPED = ("dtype", "quantization", "kv_cache_dtype", "max_model_len", "max_num_seqs",
         "enforce_eager", "language_model_only", "trust_remote_code", "block_size",
         "max_num_batched_tokens")
# Whole families, matched on every destination the parser defines.
RESERVED_FAMILIES = ("ssl_", "data_parallel_")
# ADR 0014 §4: reserved only while sleep mode is on (mllm renders `eager`).
SLEEP_RESERVED = ("safetensors_load_strategy",)

_CODES = frozenset({"invalid_launch_arguments", "config_file_refused",
                    "invalid_arguments", "effective_args_mismatch", "startup_error",
                    # ADR 0008 (owner decision 2026-09-23): a launch with sleep
                    # mode on needs the development routes and destinations
                    # deep parking drives; an installation without them is
                    # refused deep parking only (engine_capabilities.py).
                    "capability_missing:deep_park",
                    # SPEC §8.2 / T21: loopback-only single-rank rendezvous
                    # (loopback_rendezvous.py).
                    "loopback_rendezvous_failed",
                    # ADR 0014 §8: an unapproved sensitive destination, or a
                    # malformed approvals document.
                    "sensitive_option_refused", "invalid_extra_approvals",
                    # ADR 0012 / T21: an installed plugin that is not vLLM's own.
                    "plugin_refused"})


class LaunchError(Exception):
    """Closed, public failure category; never retains argument values."""

    def __init__(self, code):
        self.code = code if code in _CODES else "startup_error"
        super().__init__(self.code)


def split_argv(argv):
    """The rendered reserved block and the user block, split at the marker."""
    if (type(argv) not in (list, tuple) or len(argv) < 3
            or any(type(token) is not str for token in argv)
            or argv[0] != "serve" or argv[1].startswith("-")
            or argv.count(MARKER) != 1):
        raise LaunchError("invalid_launch_arguments")
    index = argv.index(MARKER)
    return list(argv[:index]), list(argv[index + 1:])


def split_user(user):
    """The typed and host-fixed arguments, and the deployment's extras."""
    count = user.count(EXTRA_MARKER)
    if count > 1:
        raise LaunchError("invalid_launch_arguments")
    if count == 0:
        return list(user), []
    index = user.index(EXTRA_MARKER)
    return list(user[:index]), list(user[index + 1:])


def _option_name(token):
    return token.split("=", 1)[0].split(".", 1)[0].lower().replace("_", "-")


def check_user_tokens(user):
    """Refusals that must happen before vLLM's parser ever sees the tokens.

    vLLM's FlexibleArgumentParser reads a `--config` file inside parse_args,
    so it is refused here in any spelling that could reach it: the exact
    option, `=value`, underscores, or an abbreviation argparse would expand.
    Help and version would exit successfully without serving.
    """
    for token in user:
        if not token.startswith("-") or token == "-":
            continue
        name = _option_name(token)
        if name.startswith("--mllm-"):
            raise LaunchError("invalid_launch_arguments")
        if len(name) > 2 and "--config".startswith(name) or name == "--config":
            raise LaunchError("config_file_refused")
        if name in ("-h", "--help", "-v", "--version") or name.startswith("--help"):
            raise LaunchError("invalid_launch_arguments")


def _parse(parser, argv):
    try:
        return parser.parse_args(list(argv))
    except BaseException:
        # argparse exits through SystemExit; its usage text is the engine's
        # own and names options, not credentials (the key rides the env).
        raise LaunchError("invalid_arguments") from None


def reserved_destinations(namespace, sleep_mode):
    names = set(RESERVED)
    names.update(name for name in vars(namespace) if name.startswith(RESERVED_FAMILIES))
    names.update(name for name in RESERVED_IF_PRESENT if hasattr(namespace, name))
    if sleep_mode:
        names.update(SLEEP_RESERVED)
    return sorted(names)


def check_reserved(expected, actual):
    """Every reserved destination keeps the value mllm rendered, exactly."""
    sleep_mode = getattr(expected, "enable_sleep_mode", None)
    if type(sleep_mode) is not bool:
        raise LaunchError("effective_args_mismatch")
    names = set(reserved_destinations(expected, sleep_mode))
    names.update(reserved_destinations(actual, sleep_mode))
    for name in names:
        missing = object()
        want = getattr(expected, name, missing)
        have = getattr(actual, name, missing)
        if want is missing or have is missing or type(want) is not type(have) or want != have:
            raise LaunchError("effective_args_mismatch")


def _policy():
    try:
        from . import extra_args_policy
    except ImportError:
        # Run as a script from the runtime directory, which is then sys.path[0].
        import extra_args_policy
    return extra_args_policy


def check_extra(base, actual, approvals, checkpoint):
    """ADR 0014 §2, §8: what the extras changed, as vLLM resolved it.

    The destinations whose value differs between the typed-and-fixed parse and
    the full parse are exactly the ones the deployment's extras set. None may
    be a typed field; a sensitive one needs the host's named approval.
    """
    policy = _policy()
    missing = object()
    supplied = {}
    for name, value in vars(actual).items():
        before = getattr(base, name, missing)
        if before is missing or type(before) is not type(value) or before != value:
            supplied[name] = value
    if any(name in TYPED for name in supplied):
        raise LaunchError("effective_args_mismatch")
    try:
        policy.check("vllm", supplied, approvals, checkpoint)
    except policy.Refused:
        raise LaunchError("sensitive_option_refused") from None


def resolve(argv, parser, approvals=None):
    """Parse with the installed parser and return the checked namespace."""
    policy = _policy()
    if approvals is None:
        approvals = policy.parse_approvals(None)
    reserved, user = split_argv(argv)
    fixed, extra = split_user(user)
    check_user_tokens(fixed)
    check_user_tokens(extra)
    expected = _parse(parser, reserved)
    base = _parse(parser, reserved + fixed)
    actual = _parse(parser, reserved + fixed + extra)
    check_reserved(expected, base)
    check_reserved(expected, actual)
    check_extra(base, actual, approvals, argv[1])
    # ADR 0014 §8 / T21: trust_remote_code runs checkpoint code; the host's
    # approval is rechecked here, not only at deploy time.
    if getattr(actual, "trust_remote_code", False) is True and not approvals.trust_remote_code:
        raise LaunchError("sensitive_option_refused")
    return actual


def check_plugins(entry_points=None):
    """ADR 0012 / T21: refuse any installed vLLM plugin that is not vLLM's own.

    Plugins run code when vLLM imports them. mllm also pins VLLM_PLUGINS to the
    empty list so none loads; an installation carrying a foreign one is refused
    before any engine import rather than trusted to that pin alone.
    """
    if entry_points is None:
        import importlib.metadata
        entry_points = importlib.metadata.entry_points
    try:
        points = list(entry_points())
    except Exception:
        raise LaunchError("plugin_refused") from None
    for point in points:
        group = getattr(point, "group", "")
        if type(group) is not str or not group.startswith("vllm."):
            continue
        dist = getattr(point, "dist", None)
        try:
            name = dist.metadata["Name"] if dist is not None else None
        except Exception:
            name = None
        if type(name) is not str or name.lower().replace("_", "-") != "vllm":
            raise LaunchError("plugin_refused")


class InstalledVllm:
    """The installed build's own `vllm serve` parser and server (0.29.0).

    Mirrors vllm/entrypoints/cli/main.py for the serve subcommand only:
    environment setup, a FlexibleArgumentParser with the serve subparser,
    the subcommand's validation, then its dispatch in this process.
    """

    def __init__(self):
        from vllm.entrypoints.cli import serve
        from vllm.entrypoints.serve.utils.api_utils import cli_env_setup
        from vllm.utils.argparse_utils import FlexibleArgumentParser
        self._serve = serve
        self._env_setup = cli_env_setup
        self._parser_class = FlexibleArgumentParser
        self._command = None

    def env_setup(self):
        self._env_setup()

    def parser(self):
        parser = self._parser_class(description="vLLM CLI")
        subparsers = parser.add_subparsers(required=False, dest="subparser")
        self._command = self._serve.ServeSubcommand()
        self._command.subparser_init(subparsers).set_defaults(
            dispatch_function=self._command.cmd)
        return parser

    def validate(self, args):
        self._command.validate(args)

    def run(self, args):
        args.dispatch_function(args)

    def import_module(self, name):
        """The capability probes' importer (engine_capabilities.py)."""
        import importlib
        return importlib.import_module(name)


def _capabilities():
    try:
        from . import engine_capabilities
    except ImportError:
        # Run as a script from the runtime directory, which is then sys.path[0].
        import engine_capabilities
    return engine_capabilities


def check_deep_park(args, parser, runtime):
    """ADR 0008: a sleep-mode launch needs the shapes deep parking drives.

    Probed by shape on this installation, never by file hashes: the sleep and
    middleware destinations and the sleep, wake, collective RPC and prefix
    cache routes. Without them only deep parking is refused; the same
    deployment declared `restart_only` renders no sleep mode and serves.
    """
    if getattr(args, "enable_sleep_mode", None) is not True:
        return
    capabilities = _capabilities()
    missing = capabilities.vllm_deep_park(capabilities.parser_destinations(parser),
                                          runtime.import_module)
    if missing:
        raise LaunchError("capability_missing:deep_park")


def _rendezvous():
    try:
        from . import loopback_rendezvous
    except ImportError:
        # Run as a script from the runtime directory, which is then sys.path[0].
        import loopback_rendezvous
    return loopback_rendezvous


def _hint(code):
    if code.startswith("capability_missing:"):
        return (" (this vLLM installation lacks what parking (deep or host_backed)"
                " needs; declare residency restart_only, or use a build that provides it)")
    return ""


def main(argv=None, runtime=None, stderr=None):
    """Closed refusal status before serving; the engine owns its own failures."""
    stderr = sys.stderr if stderr is None else stderr
    argv = sys.argv[1:] if argv is None else argv
    rendezvous = _rendezvous()
    try:
        # Cheap refusals first: nothing is imported for a refused vector.
        _, user = split_argv(argv)
        fixed, extra = split_user(user)
        check_user_tokens(fixed)
        check_user_tokens(extra)
        # ADR 0014 §8: the host's approvals, read and removed before anything
        # else so no engine child inherits them.
        policy = _policy()
        try:
            approvals = policy.approvals_from_environment()
        except policy.Refused:
            raise LaunchError("invalid_extra_approvals") from None
        # ADR 0012 / T21: no plugin loads; a foreign one refuses the launch.
        os.environ["VLLM_PLUGINS"] = ""
        if runtime is None:
            check_plugins()
        # SPEC §8.2 / T21: vLLM already rendezvouses through a file store on
        # CUDA; its TCP fallback address and group transports are pinned to
        # loopback before any engine import reads them.
        try:
            pinned = rendezvous.pin("vllm")
        except rendezvous.RendezvousError:
            raise LaunchError("loopback_rendezvous_failed") from None
        runtime = InstalledVllm() if runtime is None else runtime
        runtime.env_setup()
        parser = runtime.parser()
        args = resolve(argv, parser, approvals)
        try:
            runtime.validate(args)
        except Exception:
            raise LaunchError("invalid_arguments") from None
        check_deep_park(args, parser, runtime)
        try:
            rendezvous.verify(pinned)
        except rendezvous.RendezvousError:
            raise LaunchError("loopback_rendezvous_failed") from None
    except LaunchError as error:
        stderr.write("vllm_startup_failed: " + error.code + _hint(error.code) + "\n")
        return 1
    except Exception:
        stderr.write("vllm_startup_failed: startup_error\n")
        return 1
    runtime.run(args)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
