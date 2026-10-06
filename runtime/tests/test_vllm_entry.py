"""CPU-only tests for the protected vLLM entry; never evidence a build serves.

The fake parser reproduces the argparse behaviour vLLM 0.29.0's serve parser
relies on (a `serve` subparser, abbreviations, `=value`, append and boolean
negation actions, short aliases). The real parser is exercised only live.
"""

import argparse
import io
import json
import os
from pathlib import Path
import unittest
from unittest import mock

from runtime import vllm_entry as entry


def fake_parser(drop=()):
    parser = argparse.ArgumentParser(prog="vllm")
    sub = parser.add_subparsers(dest="subparser")
    serve = sub.add_parser("serve")
    serve.add_argument("model_tag", nargs="?")
    add = serve.add_argument

    def option(*names, **kwargs):
        dest = kwargs.get("dest") or names[0].lstrip("-").replace("-", "_")
        if dest not in drop:
            add(*names, **kwargs)

    option("--model", default="Qwen/Qwen3-0.6B")
    option("--host", default=None)
    option("--port", type=int, default=8000)
    option("--uds", default=None)
    option("--root-path", default=None)
    option("--api-key", nargs="+", default=None)
    option("--middleware", action="append", default=[])
    option("--served-model-name", nargs="+", default=None)
    option("--tensor-parallel-size", "-tp", type=int, default=1)
    option("--pipeline-parallel-size", "-pp", type=int, default=1)
    option("--gpu-memory-utilization", type=float, default=0.9)
    option("--cpu-offload-gb", type=float, default=0)
    option("--kv-cache-memory-bytes", type=int, default=None)
    option("--enable-sleep-mode", action=argparse.BooleanOptionalAction, default=False)
    option("--enable-log-requests", action=argparse.BooleanOptionalAction, default=False)
    option("--disable-log-stats", action="store_true")
    option("--log-config-file", default=None)
    option("--uvicorn-log-level", default="info")
    option("--disable-uvicorn-access-log", action="store_true")
    option("--distributed-executor-backend", default=None)
    option("--headless", action="store_true")
    option("--api-server-count", "-asc", type=int, default=None)
    option("--revision", default=None)
    option("--code-revision", default=None)
    option("--enable-ssl-refresh", action="store_true")
    option("--config", default=None)
    option("--grpc", action="store_true")
    option("--ssl-keyfile", default=None)
    option("--ssl-ciphers", default=None)
    option("--data-parallel-size", "-dp", type=int, default=1)
    option("--data-parallel-rank", type=int, default=None)
    option("--safetensors-load-strategy", default=None)
    option("--dtype", default="auto")
    option("--max-model-len", type=int, default=None)
    option("--enforce-eager", action=argparse.BooleanOptionalAction, default=False)
    option("--reasoning-parser", default="")
    option("--language-model-only", action="store_true")
    option("--trust-remote-code", action="store_true")
    # Multi-node rendezvous and sensitive shapes vLLM's serve parser declares.
    option("--master-addr", default="127.0.0.1")
    option("--master-port", type=int, default=29501)
    option("--nnodes", "-n", type=int, default=1)
    option("--node-rank", "-r", type=int, default=0)
    # vLLM 0.30.0 scale-out routes and JSON configurations.
    option("--enable-scale-out", action=argparse.BooleanOptionalAction, default=False)
    option("--watermark-config", type=json.loads, default=None)
    option("--engram-config", type=json.loads, default=None)
    option("--worker-extension-cls", default="")
    option("--compilation-config", "-cc", type=json.loads, default=None)
    option("--download-dir", default=None)
    option("--chat-template", default=None)
    return parser


RESERVED = ["serve", "/models/qwen", "--host", "127.0.0.1", "--served-model-name", "route",
            "--tensor-parallel-size", "1", "--pipeline-parallel-size", "1",
            "--port", "20001", "--gpu-memory-utilization", "0.10",
            "--kv-cache-memory-bytes", "4294967296"]
SLEEP = ["--enable-sleep-mode", "--safetensors-load-strategy", "eager",
         "--middleware", "capyctl_vllm_guard.RequireEngineKey"]


class FakeRuntime:
    def __init__(self, drop=()):
        self.drop = drop
        self.events = []
        self.served = None

    def env_setup(self):
        self.events.append("env")

    def parser(self):
        self.events.append("parser")
        return fake_parser(self.drop)

    def validate(self, args):
        self.events.append("validate")

    def run(self, args):
        self.events.append("run")
        self.served = args

    def import_module(self, name):
        """Synthetic vLLM development routers; `self.missing` drops routes."""
        routes = {"vllm.entrypoints.serve.dev.sleep.api_router": ("/sleep", "/wake_up", "/is_sleeping"),
                  "vllm.entrypoints.serve.dev.rpc.api_router": ("/collective_rpc",),
                  "vllm.entrypoints.serve.dev.cache.api_router": ("/reset_prefix_cache",)}
        if name not in routes:
            raise ImportError(name)
        missing = getattr(self, "missing", ())
        router = type("Router", (), {})()
        router.routes = [type("Route", (), {"path": path})() for path in routes[name]
                         if path not in missing]
        module = type("Module", (), {})()
        module.router = router
        return module


def argv(user=(), sleep=False):
    return RESERVED + (SLEEP if sleep else []) + [entry.MARKER, *user]


class ResolveTests(unittest.TestCase):
    def resolve(self, user=(), sleep=False, drop=()):
        # argparse writes its own usage on refusal; keep test output clean.
        with mock.patch("sys.stderr", io.StringIO()):
            return entry.resolve(argv(user, sleep), fake_parser(drop))

    def refused(self, user, code="effective_args_mismatch", sleep=False, drop=()):
        with self.assertRaises(entry.LaunchError) as caught:
            self.resolve(user, sleep, drop)
        self.assertEqual(caught.exception.code, code)
        for token in user:
            self.assertNotIn(token, str(caught.exception))

    # T14: typed and extra arguments reach vLLM's parser unchanged.
    def test_typed_and_extra_arguments_resolve_through_the_installed_parser(self):
        args = self.resolve(["--dtype", "bfloat16", "--max-model-len", "32768",
                             "--enforce-eager", "--language-model-only",
                             "--reasoning-parser=qwen3"])
        self.assertEqual((args.dtype, args.max_model_len, args.enforce_eager,
                          args.language_model_only, args.reasoning_parser),
                         ("bfloat16", 32768, True, True, "qwen3"))
        self.assertEqual(args.model_tag, "/models/qwen")
        self.assertEqual(args.kv_cache_memory_bytes, 4294967296)

    # T14 T21: reserved fields refused however spelled or supplied.
    def test_reserved_fields_are_refused_however_spelled(self):
        for user in (["--port", "1"], ["--port=1"], ["--po", "1"],
                     ["--host", "0.0.0.0"], ["--served-model-name", "other"],
                     ["-tp", "2"], ["--tensor-parallel", "2"],
                     ["-asc", "2"], ["--api-server-count", "1"], ["--headless"],
                     ["--api-key", "x"], ["--middleware", "evil.Middleware"],
                     ["--gpu-memory-utilization", "0.9"], ["--kv-cache-memory-bytes", "1"],
                     ["--enable-sleep-mode"], ["--disable-log-stats"],
                     ["--uds", "/tmp/s"], ["--root-path", "/x"], ["--grpc"],
                     ["--ssl-ciphers", "x"], ["--ssl-keyfile", "/k"],
                     ["-dp", "2"], ["--data-parallel-rank", "0"],
                     ["--revision", "main"], ["--distributed-executor-backend", "ray"],
                     ["--enable-log-requests"], ["--uvicorn-log-level", "debug"]):
            with self.subTest(user=user):
                self.refused(user)

    def test_a_second_model_is_refused(self):
        # `--model` replaces the positional or leaves two positionals.
        for user in (["--model", "/other"], ["/other"]):
            with self.subTest(user=user):
                with self.assertRaises(entry.LaunchError) as caught:
                    self.resolve(user)
                self.assertIn(caught.exception.code,
                              ("effective_args_mismatch", "invalid_arguments"))

    def test_sleep_mode_reserves_the_loader_and_the_guard(self):
        args = self.resolve(sleep=True)
        self.assertEqual(args.middleware, ["capyctl_vllm_guard.RequireEngineKey"])
        for user in (["--safetensors-load-strategy", "lazy"], ["--no-enable-sleep-mode"],
                     ["--middleware", "other.Middleware"]):
            with self.subTest(user=user):
                self.refused(user, sleep=True)
        # Without sleep mode the loader is an ordinary option.
        self.assertEqual(self.resolve(["--safetensors-load-strategy", "lazy"])
                         .safetensors_load_strategy, "lazy")

    # T14: configuration files refused before the parser can read one.
    def test_configuration_files_are_refused_in_every_spelling(self):
        for user in (["--config", "/tmp/c.yaml"], ["--config=/tmp/c.yaml"],
                     ["--conf", "/tmp/c.yaml"], ["--CONFIG", "x"], ["--con_fig"]):
            with self.subTest(user=user):
                with self.assertRaises(entry.LaunchError) as caught:
                    entry.check_user_tokens(user)
                    self.resolve(user)
                self.assertIn(caught.exception.code,
                              ("config_file_refused", "invalid_arguments"))
        with self.assertRaises(entry.LaunchError) as caught:
            entry.check_user_tokens(["--config", "x"])
        self.assertEqual(caught.exception.code, "config_file_refused")
        # A reserved `config` set any other way is still a mismatch.
        parser = fake_parser()
        expected = parser.parse_args(RESERVED)
        actual = parser.parse_args(RESERVED + ["--config", "x"])
        with self.assertRaises(entry.LaunchError):
            entry.check_reserved(expected, actual)

    def test_marker_help_and_version_are_closed(self):
        for vector in (RESERVED, RESERVED + [entry.MARKER, entry.MARKER],
                       ["run"] + RESERVED[1:] + [entry.MARKER],
                       ["serve", "--port", "1", entry.MARKER], "serve", [],
                       RESERVED + [entry.MARKER, 3]):
            with self.subTest(vector=vector), self.assertRaises(entry.LaunchError) as caught:
                entry.split_argv(vector)
            self.assertEqual(caught.exception.code, "invalid_launch_arguments")
        for user in (["--capyctl-user-args-x"], ["--help"], ["-h"], ["--version"], ["-v"],
                     ["--help=ModelConfig"]):
            with self.subTest(user=user), self.assertRaises(entry.LaunchError):
                entry.check_user_tokens(user)

    def test_parser_drift_fails_closed(self):
        # ADR 0014 open issue 4: a reserved destination the build lacks.
        self.refused([], drop=("grpc",))

    def test_unknown_or_malformed_arguments_are_invalid(self):
        for user in (["--no-such-flag"], ["--port"], ["--max-model-len", "x"]):
            with self.subTest(user=user):
                with self.assertRaises(entry.LaunchError) as caught:
                    self.resolve(user)
                self.assertIn(caught.exception.code, ("invalid_arguments",
                                                      "effective_args_mismatch"))


class ExtraArgumentTests(unittest.TestCase):
    """ADR 0014 §8, SPEC §8.2: the deployment's own extra arguments follow the
    second marker and are gated on the destinations vLLM's parser resolved."""

    def resolve(self, extra, fixed=(), approvals=None):
        vector = RESERVED + [entry.MARKER, *fixed, entry.EXTRA_MARKER, *extra]
        with mock.patch("sys.stderr", io.StringIO()):
            return entry.resolve(vector, fake_parser(), approvals)

    def refused(self, extra, code, fixed=(), approvals=None):
        with self.assertRaises(entry.LaunchError) as caught:
            self.resolve(extra, fixed, approvals)
        self.assertEqual(caught.exception.code, code)
        for token in extra:
            self.assertNotIn(token, str(caught.exception))

    # T21 T22: abbreviations resolve to the reserved multi-node rendezvous.
    def test_multi_node_rendezvous_is_reserved_however_spelled(self):
        for extra in (["--master-ad", "10.0.0.1"], ["--master-addr=10.0.0.1"],
                      ["--master-port", "1"], ["--nnodes", "2"], ["-n", "2"],
                      ["--node-rank", "1"]):
            with self.subTest(extra=extra):
                self.refused(extra, "effective_args_mismatch")

    # T21: vLLM 0.30.0 scale-out registers extra routes; capyctl never enables it.
    # (The deploy-time check also refuses `--no-enable-scale-out`; here it
    # leaves the destination at capyctl's value, so it changes nothing.)
    def test_scale_out_is_reserved_however_spelled(self):
        for extra in (["--enable-scale-out"], ["--enable-scale"], ["--enable-scale-o"]):
            with self.subTest(extra=extra):
                self.refused(extra, "effective_args_mismatch")

    # T21: sensitive destinations need the host's named approval.
    def test_sensitive_destinations_need_named_approval(self):
        from runtime import extra_args_policy as policy
        for extra in (["--worker-extension", "pkg.Ext"], ["--compilation-config", "{}"],
                      ["-cc", '{"level": 3}'], ["--download-dir", "/srv/cache"],
                      ["--chat-template", "/tmp/t.jinja"], ["--watermark-config", "{}"],
                      ["--engram-config", "{}"]):
            with self.subTest(extra=extra):
                self.refused(extra, "sensitive_option_refused")
        approved = policy.parse_approvals(json.dumps({
            "options": ["--compilation-config", "--download-dir"],
            "paths": ["/srv/cache"], "trust_remote_code": False}))
        args = self.resolve(["--compilation-config", '{"level": 3}',
                             "--download-dir", "/srv/cache/hf"], approvals=approved)
        self.assertEqual(args.compilation_config, {"level": 3})
        self.refused(["--download-dir", "/elsewhere"], "sensitive_option_refused",
                     approvals=approved)
        # Inside the checkpoint a chat template is ordinary.
        self.resolve(["--chat-template", "/models/qwen/chat.jinja"])
        # Host-fixed arguments before the second marker are the host's own.
        self.resolve([], fixed=["--download-dir", "/srv/other"])

    # T14 T21: a typed field cannot be restated or reversed by an extra.
    def test_extras_cannot_change_typed_fields(self):
        for extra in (["--dtype", "float16"], ["--trust-remote-code"],
                      ["--no-enforce-eager"]):
            with self.subTest(extra=extra):
                self.refused(extra, "effective_args_mismatch",
                             fixed=["--dtype", "bfloat16", "--enforce-eager"])

    # T21: trust_remote_code runs checkpoint code; its host approval is
    # rechecked at launch.
    def test_trust_remote_code_needs_host_approval_at_launch(self):
        from runtime import extra_args_policy as policy
        self.refused([], "sensitive_option_refused", fixed=["--trust-remote-code"])
        approved = policy.parse_approvals(
            '{"options": [], "paths": [], "trust_remote_code": true}')
        self.assertIs(self.resolve([], fixed=["--trust-remote-code"],
                                   approvals=approved).trust_remote_code, True)

    def test_the_extra_marker_appears_at_most_once(self):
        with self.assertRaises(entry.LaunchError):
            self.resolve([entry.EXTRA_MARKER])


class PluginTests(unittest.TestCase):
    # T21: ADR 0012. Plugins run code at engine import; only vLLM's own
    # built-in entry points may be installed, and none is loaded.
    def test_foreign_plugins_are_refused_before_import(self):
        def point(group, name, dist):
            return mock.Mock(group=group, name=name,
                             dist=mock.Mock(metadata={"Name": dist}))
        own = [point("vllm.general_plugins", "lora_filesystem_resolver", "vllm")]
        entry.check_plugins(lambda: own)
        for foreign in (point("vllm.general_plugins", "x", "evil-plugin"),
                        point("vllm.platform_plugins", "p", "some-platform"),
                        point("vllm.stat_logger_plugins", "s", None)):
            with self.subTest(group=foreign.group):
                with self.assertRaises(entry.LaunchError) as caught:
                    entry.check_plugins(lambda: own + [foreign])
                self.assertEqual(caught.exception.code, "plugin_refused")
        # Other packages' groups are not vLLM's to load.
        entry.check_plugins(lambda: own + [point("console_scripts", "x", "evil")])


class MainTests(unittest.TestCase):
    def setUp(self):
        # main pins the process environment (loopback_rendezvous.py).
        environ = mock.patch.dict(os.environ)
        environ.start()
        self.addCleanup(environ.stop)

    # T21: ADR 0012. The served engine loads no plugin, and the approvals
    # document is consumed rather than inherited by engine children.
    def test_plugins_are_pinned_off_and_approvals_are_consumed(self):
        os.environ["VLLM_PLUGINS"] = "evil"
        os.environ["CAPYCTL_EXTRA_APPROVALS"] = (
            '{"options": [], "paths": [], "trust_remote_code": false}')
        runtime = FakeRuntime()
        self.assertEqual(entry.main(argv(), runtime, io.StringIO()), 0)
        self.assertEqual(os.environ["VLLM_PLUGINS"], "")
        self.assertNotIn("CAPYCTL_EXTRA_APPROVALS", os.environ)
        os.environ["CAPYCTL_EXTRA_APPROVALS"] = "{not json"
        error = io.StringIO()
        self.assertEqual(entry.main(argv(), FakeRuntime(), error), 1)
        self.assertIn("invalid_extra_approvals", error.getvalue())

    # T21: SPEC §8.2, found live 2026-09-23 (M08). The served engine inherits a
    # loopback-only rendezvous: vLLM's TCP fallback address and the gloo/NCCL
    # transports are pinned to loopback, and an inherited address is dropped.
    def test_served_engine_inherits_a_loopback_rendezvous(self):
        os.environ["HOST_IP"] = "10.1.2.3"
        os.environ["NCCL_SOCKET_IFNAME"] = "eth0"
        runtime = FakeRuntime()
        seen = {}
        run = runtime.run

        def recording_run(args):
            seen.update({name: os.environ.get(name) for name in (
                "VLLM_HOST_IP", "HOST_IP", "GLOO_SOCKET_IFNAME", "NCCL_SOCKET_IFNAME")})
            return run(args)

        runtime.run = recording_run
        self.assertEqual(entry.main(argv(), runtime, io.StringIO()), 0)
        self.assertEqual(seen, {"VLLM_HOST_IP": "127.0.0.1", "HOST_IP": None,
                                "GLOO_SOCKET_IFNAME": "lo", "NCCL_SOCKET_IFNAME": "lo"})

    # T21: a rendezvous input changed after pinning refuses before serving.
    def test_rendezvous_drift_before_serving_is_refused(self):
        runtime = FakeRuntime()

        def drifting_validate(args):
            os.environ["VLLM_HOST_IP"] = "0.0.0.0"

        runtime.validate = drifting_validate
        error = io.StringIO()
        self.assertEqual(entry.main(argv(), runtime, error), 1)
        self.assertEqual(error.getvalue(), "vllm_startup_failed: loopback_rendezvous_failed\n")
        self.assertNotIn("run", runtime.events)

    # T22: the namespace that was checked is the one that is served.
    def test_checked_namespace_is_served_in_process(self):
        runtime = FakeRuntime()
        error = io.StringIO()
        self.assertEqual(entry.main(argv(["--dtype", "float16"]), runtime, error), 0)
        self.assertEqual(runtime.events, ["env", "parser", "validate", "run"])
        self.assertEqual(runtime.served.dtype, "float16")
        self.assertEqual(error.getvalue(), "")

    def test_refusals_print_one_closed_line_and_never_serve(self):
        for user, code in ((["--port", "1"], "effective_args_mismatch"),
                           (["--config", "/secret/path.yaml"], "config_file_refused"),
                           (["--no-such-flag", "private-value"], "invalid_arguments")):
            with self.subTest(user=user):
                runtime = FakeRuntime()
                error = io.StringIO()
                with mock.patch("sys.stderr", io.StringIO()):
                    self.assertEqual(entry.main(argv(user), runtime, error), 1)
                self.assertEqual(error.getvalue(), "vllm_startup_failed: " + code + "\n")
                self.assertNotIn("run", runtime.events)

    def test_refused_vector_imports_no_engine(self):
        with mock.patch.object(entry, "InstalledVllm",
                               side_effect=AssertionError("engine imported")):
            error = io.StringIO()
            self.assertEqual(entry.main(argv(["--config", "x"]), None, error), 1)
            self.assertEqual(entry.main(RESERVED, None, io.StringIO()), 1)

    def test_validation_failure_is_closed(self):
        runtime = FakeRuntime()
        runtime.validate = mock.Mock(side_effect=TypeError("private"))
        error = io.StringIO()
        self.assertEqual(entry.main(argv(), runtime, error), 1)
        self.assertEqual(error.getvalue(), "vllm_startup_failed: invalid_arguments\n")

    # T21 T22: ADR 0008 (owner decision 2026-09-23). A sleep-mode launch on a
    # build that keeps the development routes and destinations serves.
    def test_deep_park_capabilities_present_serve_with_sleep_mode(self):
        runtime = FakeRuntime()
        error = io.StringIO()
        self.assertEqual(entry.main(argv(sleep=True), runtime, error), 0)
        self.assertEqual(runtime.events, ["env", "parser", "validate", "run"])
        self.assertIs(runtime.served.enable_sleep_mode, True)

    # T21 T22: a build missing a route deep parking drives refuses only the
    # sleep-mode (deep) launch, with the typed closed reason and a hint; the
    # same build serves a restart_only launch (no sleep mode rendered).
    def test_missing_deep_park_capability_refuses_only_sleep_mode(self):
        for missing in (("/sleep",), ("/collective_rpc",), ("/reset_prefix_cache",)):
            with self.subTest(missing=missing):
                runtime = FakeRuntime()
                runtime.missing = missing
                error = io.StringIO()
                self.assertEqual(entry.main(argv(sleep=True), runtime, error), 1)
                self.assertNotIn("run", runtime.events)
                self.assertTrue(error.getvalue().startswith(
                    "vllm_startup_failed: capability_missing:deep_park ("))
                self.assertIn("restart_only", error.getvalue())
                runtime = FakeRuntime()
                runtime.missing = missing
                self.assertEqual(entry.main(argv(), runtime, io.StringIO()), 0)
                self.assertIn("run", runtime.events)

    # T21: a parser without the guard's middleware destination cannot load
    # capyctl's guard; the reserved-block recheck already refuses it, closed.
    def test_parser_without_the_middleware_destination_is_refused(self):
        runtime = FakeRuntime(drop=("middleware",))
        error = io.StringIO()
        with mock.patch("sys.stderr", io.StringIO()):
            self.assertEqual(entry.main(argv(sleep=True), runtime, error), 1)
        self.assertNotIn("run", runtime.events)

    def test_spawned_reimport_has_no_effects(self):
        # vLLM worker processes may re-run the main script as __mp_main__.
        wrapper = Path(entry.__file__).resolve()
        namespace = {"__name__": "__mp_main__", "__file__": str(wrapper)}
        with mock.patch.object(entry, "main", side_effect=AssertionError("ran")):
            exec(compile(wrapper.read_bytes(), str(wrapper), "exec"), namespace)
        self.assertIn("resolve", namespace)

    def test_module_imports_nothing_from_the_engine(self):
        source = Path(entry.__file__).read_text()
        header = source[:source.index("class InstalledVllm")]
        self.assertNotIn("import vllm", header)
        self.assertNotIn("from vllm", header)
        self.assertTrue(os.path.basename(entry.__file__) == "vllm_entry.py")


if __name__ == "__main__":
    unittest.main()


GROUP_WORKER = ["serve", "/models/qwen", "--tensor-parallel-size", "2",
                "--pipeline-parallel-size", "1", "--distributed-executor-backend", "mp",
                "--nnodes", "2", "--node-rank", "1", "--master-addr", "192.0.2.10",
                "--master-port", "25000", "--headless", entry.MARKER]


class GroupModeTests(unittest.TestCase):
    EXPECTED = {"nnodes": 2, "node_rank": 1, "master_addr": "192.0.2.10",
                "master_port": 25000, "headless": True,
                "distributed_executor_backend": "mp",
                "tensor_parallel_size": 2, "pipeline_parallel_size": 1}

    def setUp(self):
        environ = mock.patch.dict(os.environ)
        environ.start()
        self.addCleanup(environ.stop)

    # T14, T21: every rendered multi-node destination must match the parse.
    def test_group_drift_is_refused(self):
        entry.check_group(self.EXPECTED, argparse.Namespace(**self.EXPECTED))
        for dest, bad in [("node_rank", 0), ("master_port", 25001), ("headless", False),
                          ("tensor_parallel_size", 1)]:
            drifted = argparse.Namespace(**{**self.EXPECTED, dest: bad})
            with self.assertRaises(entry.LaunchError) as ctx:
                entry.check_group(self.EXPECTED, drifted)
            self.assertEqual(ctx.exception.code, "group_drift:" + dest)
        missing = {k: v for k, v in self.EXPECTED.items() if k != "nnodes"}
        with self.assertRaises(entry.LaunchError) as ctx:
            entry.check_group(missing, argparse.Namespace(**self.EXPECTED))
        self.assertEqual(ctx.exception.code, "group_drift:nnodes")

    # T21, R11: a headless worker serves with its own address and rendered gloo
    # interface, and no inherited transport input.
    def test_worker_serves_with_group_environment(self):
        os.environ.update({"CAPYCTL_GROUP_MODE": "1", "VLLM_HOST_IP": "192.0.2.11",
                           "GLOO_SOCKET_IFNAME": "eth9", "NCCL_SOCKET_IFNAME": "eth0",
                           "MASTER_ADDR": "198.51.100.1",
                           "CAPYCTL_GROUP_EXPECTED": json.dumps(
                               {**self.EXPECTED, "gloo_socket_ifname": "eth9"})})
        runtime = FakeRuntime()
        seen = {}
        run = runtime.run

        def recording_run(args):
            seen.update({name: os.environ.get(name) for name in (
                "VLLM_HOST_IP", "GLOO_SOCKET_IFNAME", "NCCL_SOCKET_IFNAME", "MASTER_ADDR")})
            return run(args)

        runtime.run = recording_run
        error = io.StringIO()
        self.assertEqual(entry.main(GROUP_WORKER, runtime, error), 0, error.getvalue())
        self.assertEqual(seen, {"VLLM_HOST_IP": "192.0.2.11", "GLOO_SOCKET_IFNAME": "eth9",
                                "NCCL_SOCKET_IFNAME": None, "MASTER_ADDR": None})
        self.assertNotIn("CAPYCTL_GROUP_EXPECTED", os.environ)

    # T14: a rendered vector that disagrees with the expected payload refuses.
    def test_worker_drift_refuses_before_serving(self):
        os.environ.update({"CAPYCTL_GROUP_MODE": "1", "VLLM_HOST_IP": "192.0.2.11",
                           "CAPYCTL_GROUP_EXPECTED": json.dumps(
                               {**self.EXPECTED, "node_rank": 0})})
        runtime = FakeRuntime()
        error = io.StringIO()
        self.assertEqual(entry.main(GROUP_WORKER, runtime, error), 1)
        self.assertEqual(error.getvalue(), "vllm_startup_failed: group_drift:node_rank\n")
        self.assertNotIn("run", runtime.events)
