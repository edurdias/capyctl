"""Launch-time capability probes for the engine internals capyctl hooks (ADR 0008).

Owner decision 2026-09-23: engine installation files get no hard-coded hashes
and no permission rule. Instead, each internal API capyctl depends on is probed at
launch by its shape: the module imports, the attribute or method exists and is
callable, the record declares the field, the router serves the route, the
metrics module names the gauge. A missing capability refuses only the feature
that depends on it, with one closed reason (`capability_missing:<name>`);
serving without that feature stays available. A custom or patched build that
keeps the shapes launches like a stock one.

Capabilities, per engine family, and the feature each gates:

- `core`: what every launch needs (the ServerArgs record and resolution the
  SGLang entry renders and rechecks, SGLang's `launch_server`; vLLM's `serve`
  parser destinations the vLLM entry reserves). Missing refuses the launch.
- `deep_park`: SGLang's memory saver hooks (the saver adapter and an importable
  torch_memory_saver), the memory saver and weights CPU backup ServerArgs
  fields and the release, resume, weight update from disk and flush routes;
  vLLM's sleep mode and middleware destinations and its sleep, wake,
  collective RPC and prefix cache routes. It is the parking capability of
  both parking tiers: missing refuses a deployment declared `deep` or
  `host_backed` (discrete GPU design §5; use `restart_only`) and any Park.
  `host_backed` drives a subset of these shapes (no disk reload, no
  collective RPC), so a build must carry the whole set to park either way.
- `metrics`: the gauges the host agent scrapes for load reports. Missing
  degrades load reporting only; it never refuses a launch.
- `observation` (SGLang): the scheduler and saver shapes the allocation
  observer binds to (sglang_saver_residency, sglang_observation_enrollment):
  the scheduler target `launch_server` takes, the Scheduler's event loop and
  input safe point, the saver's pools and the pool snapshot. Missing leaves the
  launch serving without an observation; SPEC §9.2 then refuses Park with no
  effect, since release cannot be observed.

A probe imports the engine's own modules, so it runs only where the engine
would run anyway: inside the protected entry after its startup gates, or as a
separate child of the installation's own interpreter at launch admission (this
file's command line). It never renders exception text, paths or values: the
result names capabilities and fixed probe labels only. Passing probes are not
evidence that a build serves a model or parks correctly (AGENTS.md: CPU and
fake tests are not qualification).
"""

import dataclasses
import importlib
import inspect
import json
import os
import sys


SCHEMA = "capyctl/engine-capabilities/v1"
ENGINES = {
    "sglang": ("core", "deep_park", "metrics", "observation"),
    "vllm": ("core", "deep_park", "metrics"),
}
_MAX_SOURCE = 4 * 1024 * 1024

# SGLang routes the adapter drives for a deep park and wake
# (crates/capyctl-adapters/src/sglang/http.rs).
SGLANG_DEEP_ROUTES = ("/release_memory_occupation", "/resume_memory_occupation",
                      "/update_weights_from_disk", "/flush_cache")
# The ServerArgs fields a deep launch renders (ADR 0010: residency decides).
SGLANG_DEEP_FIELDS = ("enable_memory_saver", "enable_weights_cpu_backup")
# Load gauges the host agent scrapes (crates/capyctl-agent/src/load.rs).
SGLANG_GAUGES = ("sglang:num_running_reqs", "sglang:num_queue_reqs", "sglang:token_usage")
VLLM_GAUGES = ("vllm:num_requests_running", "vllm:num_requests_waiting",
               "vllm:kv_cache_usage_perc")
# vLLM development routes the adapter drives (crates/capyctl-adapters/src/vllm/residency.rs).
VLLM_DEEP_ROUTES = (
    ("vllm.entrypoints.serve.dev.sleep.api_router", ("/sleep", "/wake_up", "/is_sleeping")),
    ("vllm.entrypoints.serve.dev.rpc.api_router", ("/collective_rpc",)),
    ("vllm.entrypoints.serve.dev.cache.api_router", ("/reset_prefix_cache",)),
)
# vLLM parser destinations deep parking renders: sleep mode and capyctl's guard
# middleware (capyctl_vllm_guard.RequireEngineKey), and the eager load strategy.
VLLM_DEEP_DESTINATIONS = ("enable_sleep_mode", "middleware", "safetensors_load_strategy")


class CapabilityMissing(Exception):
    """One closed reason; never paths, values or native exception text."""

    def __init__(self, capability):
        self.capability = capability
        self.code = "capability_missing:" + capability
        super().__init__(self.code)


@dataclasses.dataclass(frozen=True)
class Report:
    """Probe labels missing per capability; an empty tuple means available."""

    engine: str
    missing: tuple

    def available(self, capability):
        return not dict(self.missing).get(capability, ("unprobed",))

    def missing_labels(self, capability):
        return dict(self.missing).get(capability, ("unprobed",))

    def to_json(self):
        return {"schema": SCHEMA, "engine": self.engine,
                "capabilities": {name: list(labels) for name, labels in self.missing}}


def _import(importer, name):
    try:
        return importer(name)
    except Exception:
        return None


def _callable(owner, name):
    try:
        return callable(getattr(owner, name))
    except Exception:
        return False


def record_fields(cls):
    """The declared fields of a msgspec Struct or a dataclass, or None."""
    try:
        fields = cls.__struct_fields__
        if type(fields) is tuple and all(type(name) is str for name in fields):
            return frozenset(fields)
    except Exception:
        pass
    try:
        if dataclasses.is_dataclass(cls):
            return frozenset(field.name for field in dataclasses.fields(cls))
    except Exception:
        pass
    return None


def route_paths(owner):
    """The paths a FastAPI app or router serves, or an empty set."""
    try:
        return frozenset(path for path in (getattr(route, "path", None)
                                           for route in tuple(owner.routes))
                         if type(path) is str)
    except Exception:
        return frozenset()


def _names_in_source(module, names):
    """Which of `names` the module's own source does not mention."""
    try:
        path = module.__file__
        with open(path, "rb") as stream:
            text = stream.read(_MAX_SOURCE + 1)
        if len(text) > _MAX_SOURCE:
            return tuple(names)
        text = text.decode("utf-8", errors="replace")
    except Exception:
        return tuple(names)
    return tuple(name for name in names
                 if '"%s"' % name not in text and "'%s'" % name not in text)


def _missing(checks):
    return tuple(label for label, ok in checks if not ok)


def sglang_core(arguments, launch, reserved_fields=()):
    """ServerArgs record and resolution the entry renders and rechecks."""
    server_args = getattr(arguments, "ServerArgs", None)
    fields = record_fields(server_args) if server_args is not None else None
    checks = [
        ("server_args.ServerArgs", server_args is not None),
        ("ServerArgs.add_cli_args", _callable(server_args, "add_cli_args")),
        ("ServerArgs.resolve_once", _callable(server_args, "resolve_once")),
        ("ServerArgs.resolved_dict", _callable(server_args, "resolved_dict")),
        ("ServerArgs.fields", fields is not None),
        ("http_server.launch_server", _callable(launch, "launch_server")),
    ]
    if fields is not None:
        checks.extend(("ServerArgs." + name, name in fields) for name in reserved_fields)
    return _missing(checks)


def sglang_deep_park(arguments, launch, importer):
    """Memory saver hooks and the release/resume/reload routes deep parking drives."""
    fields = record_fields(getattr(arguments, "ServerArgs", None)) or frozenset()
    adapter = _import(importer, "sglang.srt.utils.torch_memory_saver_adapter")
    factory = getattr(adapter, "TorchMemorySaverAdapter", None)
    real = getattr(adapter, "_TorchMemorySaverAdapterReal", None)
    try:
        saver_imported = adapter is not None and vars(adapter).get("import_error", True) is None
    except Exception:
        saver_imported = False
    paths = route_paths(getattr(launch, "app", None))
    checks = [("ServerArgs." + name, name in fields) for name in SGLANG_DEEP_FIELDS]
    checks += [
        ("torch_memory_saver_adapter", adapter is not None),
        ("TorchMemorySaverAdapter.create", _callable(factory, "create")),
        ("TorchMemorySaverAdapterReal.pause", _callable(real, "pause")),
        ("TorchMemorySaverAdapterReal.resume", _callable(real, "resume")),
        ("TorchMemorySaverAdapterReal.region", _callable(real, "region")),
        ("torch_memory_saver", saver_imported),
    ]
    checks += [("route:" + path, path in paths) for path in SGLANG_DEEP_ROUTES]
    return _missing(checks)


def sglang_metrics(importer):
    module = _import(importer, "sglang.srt.observability.metrics_collector")
    if module is None:
        return ("metrics_collector",)
    return tuple("gauge:" + name for name in _names_in_source(module, SGLANG_GAUGES))


def accepts_scheduler_target(launch):
    """`launch_server` takes the scheduler process target by keyword."""
    try:
        parameters = inspect.signature(launch.launch_server).parameters
        return "run_scheduler_process_func" in parameters
    except Exception:
        return False


def sglang_observation(importer, launch=None):
    scheduler = _import(importer, "sglang.srt.managers.scheduler")
    entrypoint = _import(importer, "torch_memory_saver.entrypoint")
    preload = _import(importer, "torch_memory_saver.hooks.mode_preload")
    wrapper = _import(importer, "torch_memory_saver.binary_wrapper")
    memory = _import(importer, "torch.cuda.memory")
    cls = getattr(scheduler, "Scheduler", None)
    checks = [
        ("Scheduler.process_input_requests", _callable(cls, "process_input_requests")),
        ("Scheduler.run_event_loop", _callable(cls, "run_event_loop")),
        ("scheduler.run_scheduler_process", _callable(scheduler, "run_scheduler_process")),
        ("TorchMemorySaver", getattr(entrypoint, "TorchMemorySaver", None) is not None),
        ("TorchMemorySaverImpl", getattr(entrypoint, "_TorchMemorySaverImpl", None) is not None),
        ("HookUtilModePreload", getattr(preload, "HookUtilModePreload", None) is not None),
        ("BinaryWrapper", getattr(wrapper, "BinaryWrapper", None) is not None),
        ("MemPool.snapshot", _callable(getattr(memory, "MemPool", None), "snapshot")),
    ]
    if launch is not None:
        checks.append(("launch_server.run_scheduler_process_func",
                       accepts_scheduler_target(launch)))
    return _missing(checks)


def sglang_required(settings):
    """The capabilities an SGLang launch with these typed settings depends on.

    Either parking tier renders the memory saver (`deep`, and `host_backed`
    with its weights CPU backup), so both need `deep_park`.
    """
    return ("core", "deep_park") if settings.get("memory_saver") is True else ("core",)


def require_sglang(settings, arguments, launch, importer=importlib.import_module,
                   reserved_fields=()):
    """Refuse the launch when a capability its settings depend on is missing.

    Only the dependent feature is refused: a `restart_only` launch (no memory
    saver) needs `core` alone and serves on a build without the saver hooks.
    """
    for capability in sglang_required(settings):
        if capability == "core":
            missing = sglang_core(arguments, launch, reserved_fields)
        else:
            missing = sglang_deep_park(arguments, launch, importer)
        if missing:
            raise CapabilityMissing(capability)


def vllm_core(destinations, reserved):
    return tuple("destination:" + name for name in reserved if name not in destinations)


def vllm_deep_park(destinations, importer):
    checks = [("destination:" + name, name in destinations)
              for name in VLLM_DEEP_DESTINATIONS]
    for module_name, routes in VLLM_DEEP_ROUTES:
        module = _import(importer, module_name)
        paths = route_paths(getattr(module, "router", None))
        checks += [("route:" + path, path in paths) for path in routes]
    return _missing(checks)


def vllm_metrics(importer):
    module = _import(importer, "vllm.v1.metrics.loggers")
    if module is None:
        return ("metrics_loggers",)
    return tuple("gauge:" + name for name in _names_in_source(module, VLLM_GAUGES))


def parser_destinations(parser):
    """Every destination a vLLM argparse tree defines, subparsers included."""
    seen = set()
    stack = [parser]
    while stack:
        current = stack.pop()
        for action in getattr(current, "_actions", ()):
            dest = getattr(action, "dest", None)
            if type(dest) is str:
                seen.add(dest)
            choices = getattr(action, "choices", None)
            if isinstance(choices, dict):
                stack.extend(value for value in choices.values() if hasattr(value, "_actions"))
    return frozenset(seen)


def probe_sglang(importer=importlib.import_module, reserved_fields=()):
    arguments = _import(importer, "sglang.srt.server_args")
    launch = _import(importer, "sglang.srt.entrypoints.http_server")
    return Report("sglang", (
        ("core", sglang_core(arguments, launch, reserved_fields)),
        ("deep_park", sglang_deep_park(arguments, launch, importer)),
        ("metrics", sglang_metrics(importer)),
        ("observation", sglang_observation(importer, launch)),
    ))


def probe_vllm(parser, reserved, importer=importlib.import_module):
    destinations = parser_destinations(parser) if parser is not None else frozenset()
    return Report("vllm", (
        ("core", vllm_core(destinations, reserved) if parser is not None else ("parser",)),
        ("deep_park", vllm_deep_park(destinations, importer)),
        ("metrics", vllm_metrics(importer)),
    ))


def _sglang_reserved_fields():
    try:
        from runtime import sglang_server_args
        return (tuple(sglang_server_args._RESERVED_CONSTANT)
                + tuple(sglang_server_args._RESERVED_BOUND)
                + ("trust_remote_code", "tokenizer_worker_num"))
    except Exception:
        return ()


def _run(engine):
    if engine == "sglang":
        # SPEC §9.2: the same plugin closure the entry enforces before any
        # engine import; a refused closure is a failed probe, not a report.
        from runtime.sglang_startup_guards import enforce_closed_plugins
        enforce_closed_plugins()
        return probe_sglang(reserved_fields=_sglang_reserved_fields())
    from runtime import vllm_entry
    try:
        runtime = vllm_entry.InstalledVllm()
        parser = runtime.parser()
    except Exception:
        parser = None
    return probe_vllm(parser, vllm_entry.RESERVED)


def main(argv=None):
    """`python -I -S engine_capabilities.py <sglang|vllm> <site-packages>`.

    Prints one JSON report on standard output. Engine imports may write to
    standard output or error themselves, so both are pointed at /dev/null for
    the probe and the report goes to the saved original descriptor.
    """
    argv = sys.argv[1:] if argv is None else argv
    if len(argv) != 2 or argv[0] not in ENGINES or not argv[1].startswith("/"):
        return 2
    engine, search = argv
    out = os.dup(1)
    devnull = os.open(os.devnull, os.O_WRONLY)
    try:
        os.dup2(devnull, 1)
        os.dup2(devnull, 2)
        if search not in sys.path:
            sys.path.append(search)
        try:
            report = _run(engine)
        except BaseException:
            return 1
        payload = json.dumps(report.to_json(), sort_keys=True, separators=(",", ":"))
        os.write(out, payload.encode("ascii") + b"\n")
        return 0
    finally:
        os.close(devnull)
        os.close(out)


if __name__ == "__main__":
    # SPEC §9.1 / T21: -I omits this directory; the runtime package resolves
    # from this verified directory alone, never its parent, the current
    # directory or PYTHONPATH.
    if "runtime" not in sys.modules:
        import types as _types
        _package = _types.ModuleType("runtime")
        _package.__path__ = [os.path.dirname(os.path.abspath(__file__))]
        _package.__package__ = "runtime"
        sys.modules["runtime"] = _package
    raise SystemExit(main())
