"""CPU-only ServerArgs mapping tests; constructors below never import SGLang."""

import argparse
from dataclasses import replace
import importlib
import os
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock

from runtime import sglang_entry as entry
from runtime import sglang_server_args as mapping
from test_sglang_entry import LaunchFixture

GIB = 1 << 30
# A 128 GiB unified-memory baseline: the fixture's 8 GiB static share is 0.0625.
AVAILABLE = 128 * GIB


class SyntheticArgs(SimpleNamespace):
    """Stands in for 0.5.20 ServerArgs: raw until resolve_once, then resolved."""

    __struct_fields__ = ()  # replaced per class below

    def resolve_once(self):
        disabled = getattr(self, "disable_decode_cuda_graph", False)
        backend = "disabled" if disabled else "full"
        self.cuda_graph_config = SimpleNamespace(
            prefill=SimpleNamespace(backend=backend),
            decode=SimpleNamespace(backend=backend))

    def resolved_dict(self):
        result = vars(self).copy()
        result["cuda_graph_config"] = {
            phase: {"backend": getattr(self.cuda_graph_config, phase).backend}
            for phase in ("prefill", "decode")}
        return result

    @staticmethod
    def add_cli_args(parser):
        # A small slice of the installed parser: field-derived long options,
        # one alias, one deprecated non-field switch and the config option.
        parser.add_argument("--model-path", "--model", dest="model_path", required=True)
        parser.add_argument("--host", default="127.0.0.1")
        parser.add_argument("--port", type=int, default=30000)
        parser.add_argument("--tp-size", "--tensor-parallel-size", dest="tp_size",
                            type=int, default=1)
        parser.add_argument("--mem-fraction-static", type=float, default=None)
        parser.add_argument("--enable-metrics", action="store_true")
        parser.add_argument("--ssl-keyfile", default=None)
        parser.add_argument("--dtype", default="auto")
        parser.add_argument("--context-length", type=int, default=None)
        parser.add_argument("--reasoning-parser", default=None)
        parser.add_argument("--detokenizer-worker-num", type=int, default=1)
        parser.add_argument("--schedule-policy", default="fcfs")
        parser.add_argument("--config", type=str)
        parser.add_argument("--disable-cuda-graph", action="store_true")
        parser.add_argument("--enable-flashinfer-allreduce-fusion", action="store_true")
        # Sensitive shapes the installed 0.5.20 parser declares.
        parser.add_argument("--engine-info-bootstrap-port", type=int, default=None)
        parser.add_argument("--decoupled-spec-bind-endpoint", default=None)
        parser.add_argument("--chat-template", default=None)


SyntheticArgs.__struct_fields__ = (
    "model_path", "host", "port", "tp_size", "mem_fraction_static", "enable_metrics",
    "ssl_keyfile", "dtype", "context_length", "reasoning_parser",
    "detokenizer_worker_num", "schedule_policy", "disable_cuda_graph",
    "engine_info_bootstrap_port", "decoupled_spec_bind_endpoint", "chat_template")


def synthetic_constructor(**kwargs):
    # 0.5.20 keeps constructor inputs raw until resolve_once is called.
    return SyntheticArgs(**kwargs)


synthetic_constructor.add_cli_args = SyntheticArgs.add_cli_args
synthetic_constructor.__struct_fields__ = SyntheticArgs.__struct_fields__


def construct(spec, placement, constructor=synthetic_constructor):
    return mapping.construct_server_args(spec, placement, constructor, AVAILABLE)


class MappingTests(LaunchFixture, unittest.TestCase):
    def placement(self, **changes):
        observation = mapping.ObservedPlacement(
            binding_id=self.public["binding_id"], incarnation=self.public["incarnation"],
            host_id="host-a", hardware_fingerprint="hardware-v1",
            device_id="gpu0", memory_domain="uma", physical_gpu_uuid="GPU-selected",
            cuda_visible_uuids=("GPU-other", "GPU-selected"), cuda_index=1)
        return replace(observation, **changes)

    def spec_with(self, **settings):
        self.public["settings"].update(settings)
        return self.build()

    def seen(self, spec):
        captured = {}

        def constructor(**kwargs):
            captured.update(kwargs)
            return synthetic_constructor(**kwargs)
        constructor.add_cli_args = SyntheticArgs.add_cli_args
        constructor.__struct_fields__ = SyntheticArgs.__struct_fields__
        construct(spec, self.placement(), constructor).revalidate()
        return captured

    # T14 T22: reserved settings are rendered by mllm from binding and grant.
    def test_reserved_subset_is_rendered_from_binding_placement_and_grant(self):
        kwargs = self.seen(self.build())
        expected = {
            "model_path": self.root, "tokenizer_path": self.root,
            "served_model_name": self.public["served_name"],
            "api_key": self.inference.decode(), "admin_api_key": self.admin.decode(),
            "host": "127.0.0.1", "port": 20001, "device": "cuda", "base_gpu_id": 1,
            "revision": None, "tp_size": 1, "dp_size": 1, "pp_size": 1, "ep_size": 1,
            "nnodes": 1, "node_rank": 0, "gpu_id_step": 1,
            "mem_fraction_static": 0.0625, "enable_memory_saver": True,
            "enable_weights_cpu_backup": False, "enable_draft_weights_cpu_backup": False,
            "cpu_offload_gb": 0, "disaggregation_mode": "null",
            "enable_hierarchical_cache": False, "enable_lmcache": False,
            "enable_flexkv": False, "grpc_port": None, "grpc_mode": False,
            "smg_grpc_mode": False, "sidecar": None, "use_ray": False,
            "skip_server_warmup": True, "log_requests": False,
            "enable_metrics": True, "quantize_and_serve": False,
            "trust_remote_code": False, "tokenizer_worker_num": 1,
            "detokenizer_worker_num": 1,
            "disable_prefill_cuda_graph": True, "disable_decode_cuda_graph": True,
            "cuda_graph_backend_prefill": "disabled", "cuda_graph_backend_decode": "disabled",
        }
        for key, value in expected.items():
            with self.subTest(key=key):
                self.assertEqual(kwargs[key], value)
                self.assertIs(type(kwargs[key]), type(value))
        # ADR 0014 §9: nothing from the retired single-checkpoint pin remains.
        for retired in ("context_length", "max_running_requests", "max_total_tokens",
                        "dtype", "kv_cache_dtype", "quantization", "enable_multimodal",
                        "load_format", "language_only"):
            self.assertNotIn(retired, kwargs)

    # T14: typed fields pass as the engine spells them, for any model.
    def test_typed_fields_render_for_any_model(self):
        spec = self.spec_with(
            dtype="bfloat16", quantization="modelopt_fp4", kv_cache_dtype="fp8_e4m3",
            context_length=32768, max_running_requests=16, cuda_graphs=True,
            language_model_only=True, max_total_tokens=65536, chunked_prefill_size=4096,
            tokenizer_workers=2)
        kwargs = self.seen(spec)
        for key, value in (("dtype", "bfloat16"), ("quantization", "modelopt_fp4"),
                           ("kv_cache_dtype", "fp8_e4m3"), ("context_length", 32768),
                           ("max_running_requests", 16), ("language_model_only", True),
                           ("max_total_tokens", 65536), ("chunked_prefill_size", 4096),
                           ("tokenizer_worker_num", 2)):
            with self.subTest(key=key):
                self.assertEqual(kwargs[key], value)
        # cuda_graphs: true leaves the engine's own graph defaults in place.
        for graph in ("disable_prefill_cuda_graph", "disable_decode_cuda_graph",
                      "cuda_graph_backend_prefill", "cuda_graph_backend_decode"):
            self.assertNotIn(graph, kwargs)

    def test_residency_decides_saver_and_weight_backup(self):
        kwargs = self.seen(self.spec_with(memory_saver=True, cpu_weight_backup=True,
                                          weight_restore="cpu_backup"))
        self.assertIs(kwargs["enable_weights_cpu_backup"], True)
        kwargs = self.seen(self.spec_with(memory_saver=False, cpu_weight_backup=False,
                                          weight_restore="disk_reload", cuda_graphs=None))
        self.assertIs(kwargs["enable_memory_saver"], False)

    # T14: extra arguments flow in through the installed parser.
    def test_extra_arguments_reach_the_constructor_under_their_field_names(self):
        kwargs = self.seen(self.spec_with(extra_args=[
            "--reasoning-parser", "qwen3", "--schedule-policy=lpm",
            "--detokenizer-worker-num", "2", "--enable-flashinfer-allreduce-fusion"]))
        self.assertEqual(kwargs["reasoning_parser"], "qwen3")
        self.assertEqual(kwargs["schedule_policy"], "lpm")
        # ADR 0014 §4: a safe default the deployment may override.
        self.assertEqual(kwargs["detokenizer_worker_num"], 2)
        # Mirrors from_cli_args: a non-field parser destination is dropped.
        self.assertNotIn("enable_flashinfer_allreduce_fusion", kwargs)
        # Unset options keep the engine's defaults: nothing defaulted leaks in.
        self.assertNotIn("model_path_extra", kwargs)
        self.assertEqual(kwargs["host"], "127.0.0.1")

    # T14 T21: reserved fields refused however spelled (abbreviation, alias,
    # `=value`, config file), before construction.
    def test_reserved_extra_arguments_are_refused_however_spelled(self):
        for extra in (["--port", "1"], ["--po", "1"], ["--port=1"],
                      ["--model", "/other"], ["--model-path", "/other"],
                      ["--tensor-parallel-size", "2"], ["--tp-size=2"],
                      ["--mem-fraction-static", "0.9"], ["--mem-frac", "0.9"],
                      ["--host", "0.0.0.0"], ["--ssl-keyfile", "/k"],
                      ["--config", "/tmp/c.yaml"], ["--conf", "/tmp/c.yaml"],
                      ["--enable-metrics"],
                      # Typed fields have one spelling: the typed field.
                      ["--dtype", "float16"], ["--context-length", "8"],
                      ["--disable-cuda-graph"]):
            with self.subTest(extra=extra):
                with self.assertRaises(mapping.ServerArgsError) as caught:
                    construct(self.spec_with(extra_args=extra), self.placement(),
                              lambda **kw: self.fail("constructor reached"))
                self.assertIn(caught.exception.code,
                              ("effective_args_mismatch", "invalid_extra_args"))

    # T21 T22: ADR 0014 §8, SPEC §8.2. A sensitive destination is refused on
    # what the installed parser resolved, so an abbreviation the deploy-time
    # spelling check could not know is still caught, before construction.
    def test_sensitive_destinations_need_host_approval_however_spelled(self):
        from runtime import extra_args_policy as policy
        for extra in (["--engine-info", "29500"], ["--engine-info-bootstrap-port=1"],
                      ["--decoupled-spec-bind", "tcp://0.0.0.0:1"],
                      ["--chat-template", "/tmp/chat.jinja"]):
            with self.subTest(extra=extra):
                constructor = mock.Mock(side_effect=AssertionError("constructed"))
                constructor.add_cli_args = SyntheticArgs.add_cli_args
                constructor.__struct_fields__ = SyntheticArgs.__struct_fields__
                with self.assertRaises(mapping.ServerArgsError) as caught:
                    mapping.construct_server_args(
                        self.spec_with(extra_args=extra), self.placement(),
                        constructor, AVAILABLE)
                self.assertEqual(caught.exception.code, "sensitive_option_refused")
        approved = policy.parse_approvals(
            '{"options": ["--engine-info-bootstrap-port"], "paths": [],'
            ' "trust_remote_code": false}')
        checked = mapping.construct_server_args(
            self.spec_with(extra_args=["--engine-info", "29500"]), self.placement(),
            synthetic_constructor, AVAILABLE, approvals=approved)
        self.assertEqual(checked._native.engine_info_bootstrap_port, 29500)
        # A chat template inside the checkpoint needs no approval.
        mapping.construct_server_args(
            self.spec_with(extra_args=["--chat-template", self.root + "/chat.jinja"]),
            self.placement(), synthetic_constructor, AVAILABLE)

    # T21: trust_remote_code runs checkpoint code; the host's approval is
    # rechecked at launch, not only at deploy time.
    def test_trust_remote_code_needs_host_approval_at_launch(self):
        from runtime import extra_args_policy as policy
        spec = self.spec_with(trust_remote_code=True)
        with self.assertRaises(mapping.ServerArgsError) as caught:
            mapping.construct_server_args(spec, self.placement(), synthetic_constructor,
                                          AVAILABLE)
        self.assertEqual(caught.exception.code, "sensitive_option_refused")
        approved = policy.parse_approvals(
            '{"options": [], "paths": [], "trust_remote_code": true}')
        mapping.construct_server_args(spec, self.placement(), synthetic_constructor,
                                      AVAILABLE, approvals=approved).revalidate()

    def test_malformed_or_unknown_extra_arguments_are_closed_failures(self):
        for extra in (["--no-such-option", "1"], ["--port"], ["positional"], ["--help"],
                      ["--context-length", "not-a-number"]):
            with self.subTest(extra=extra):
                constructor = mock.Mock(side_effect=AssertionError("constructed"))
                constructor.add_cli_args = SyntheticArgs.add_cli_args
                with self.assertRaises(mapping.ServerArgsError) as caught:
                    construct(self.spec_with(extra_args=extra), self.placement(), constructor)
                self.assertIn(caught.exception.code,
                              ("invalid_extra_args", "effective_args_mismatch"))
                self.assertNotIn("not-a-number", str(caught.exception))

    # T22: SGLang's own resolution runs, then only the reserved subset is rechecked.
    def test_resolution_changing_a_reserved_field_is_a_closed_mismatch(self):
        for key, replacement in (("port", 1), ("mem_fraction_static", 0.9),
                                 ("enable_metrics", False), ("host", "0.0.0.0"),
                                 ("enable_memory_saver", 1), ("tp_size", True),
                                 ("revision", "main"), ("trust_remote_code", True)):
            def changed(**kwargs):
                effective = synthetic_constructor(**kwargs)
                effective.resolve_once()
                effective.resolve_once = lambda: None
                setattr(effective, key, replacement)
                return effective
            changed.add_cli_args = SyntheticArgs.add_cli_args
            with self.subTest(key=key), self.assertRaises(mapping.ServerArgsError) as caught:
                construct(self.build(), self.placement(), changed)
            self.assertEqual(caught.exception.code, "effective_args_mismatch")

    def test_resolution_may_change_ordinary_fields(self):
        # ADR 0014 §6: only the reserved subset is revalidated; the engine's
        # resolution of ordinary and typed values is the engine's business.
        def changed(**kwargs):
            effective = synthetic_constructor(**kwargs)
            effective.resolve_once()
            effective.resolve_once = lambda: None
            effective.max_running_requests = 3
            effective.chunked_prefill_size = 2048
            return effective
        construct(self.spec_with(max_running_requests=16), self.placement(), changed)

    def test_missing_reserved_field_after_resolution_fails_closed(self):
        # ADR 0014 open issue 4: parser drift is visible, never silent.
        native = []

        def constructor(**kwargs):
            result = synthetic_constructor(**kwargs)
            native.append(result)
            return result
        checked = construct(self.build(), self.placement(), constructor)
        del native[0].enable_metrics
        with self.assertRaises(mapping.ServerArgsError):
            checked.revalidate()

    def test_disabled_graphs_must_survive_resolution(self):
        for phase in ("prefill", "decode"):
            def changed(**kwargs):
                effective = synthetic_constructor(**kwargs)
                effective.resolve_once()
                effective.resolve_once = lambda: None
                getattr(effective.cuda_graph_config, phase).backend = "full"
                return effective
            with self.subTest(phase=phase), self.assertRaises(mapping.ServerArgsError):
                construct(self.build(), self.placement(), changed)

    def test_debug_logging_requires_exact_operator_opt_in(self):
        for selected, expected in ((None, "error"), ("0", "error"), ("true", "error"), ("1", "debug")):
            with self.subTest(selected=selected), mock.patch.dict(os.environ, {}, clear=True):
                if selected is not None:
                    os.environ["MLLM_DEBUG_ENGINE_LOGS"] = selected
                result = construct(self.build(), self.placement())
                self.assertEqual(result._native.log_level, expected)
                self.assertEqual(result._native.log_level_http, expected)
                self.assertFalse(result._native.log_requests)
                result.revalidate()

    def test_placement_must_match_binding_and_physical_namespace(self):
        for changes in ({"binding_id": "other"}, {"incarnation": "other"},
                        {"host_id": "other"}, {"hardware_fingerprint": "other"},
                        {"device_id": "other"}, {"memory_domain": "other"},
                        {"physical_gpu_uuid": "GPU-other"}, {"cuda_index": 0},
                        {"cuda_index": True}, {"cuda_index": -1}, {"cuda_index": 2},
                        {"cuda_visible_uuids": ("GPU-selected", "GPU-selected")},
                        {"cuda_visible_uuids": []}, {"physical_gpu_uuid": ""}):
            with self.subTest(changes=changes), self.assertRaises(mapping.ServerArgsError):
                construct(self.build(), self.placement(**changes),
                          lambda **kw: self.fail("constructor reached"))

    def test_revalidates_launchspec_even_if_directly_constructed(self):
        spec = self.build()
        for bad in (replace(spec, _public_json="{}"),
                    replace(spec, _checkpoint_root="/private/../other"),
                    replace(spec, _admin_key=spec._inference_key)):
            with self.assertRaises(mapping.ServerArgsError):
                construct(bad, self.placement(), lambda **kw: self.fail("constructor reached"))

    # T14: mem_fraction_static is rendered from the grant, never a pinned 0.75.
    def test_static_fraction_is_derived_from_the_grant_and_rounds_down(self):
        self.assertEqual(mapping.static_fraction(8 * GIB, 128 * GIB), 0.0625)
        self.assertEqual(mapping.static_fraction(40 * GIB, 119 * GIB), 0.3361)
        for static, available in ((0, GIB), (GIB, 0), (GIB, GIB), (2 * GIB, GIB),
                                  (1, 1 << 40), (True, GIB), (GIB, 1.0)):
            with self.subTest(static=static, available=available):
                with self.assertRaises(mapping.ServerArgsError) as caught:
                    mapping.static_fraction(static, available)
                self.assertEqual(caught.exception.code, "memory_grant_unavailable")

    def test_available_memory_reads_memavailable_only(self):
        with tempfile.NamedTemporaryFile("w") as meminfo:
            meminfo.write("MemTotal:       131072000 kB\nMemAvailable:   125000000 kB\n")
            meminfo.flush()
            self.assertEqual(mapping.available_memory_bytes(meminfo.name), 125000000 * 1024)
        with tempfile.NamedTemporaryFile("w") as meminfo:
            meminfo.write("MemTotal:       131072000 kB\n")
            meminfo.flush()
            with self.assertRaises(mapping.ServerArgsError):
                mapping.available_memory_bytes(meminfo.name)
        with self.assertRaises(mapping.ServerArgsError):
            mapping.available_memory_bytes("/nonexistent/meminfo")

    def test_grant_too_large_for_the_baseline_is_refused_before_construction(self):
        with self.assertRaises(mapping.ServerArgsError) as caught:
            mapping.construct_server_args(self.build(), self.placement(),
                                          lambda **kw: self.fail("constructed"), 4 * GIB)
        self.assertEqual(caught.exception.code, "memory_grant_unavailable")

    def test_resolution_failure_is_closed(self):
        def constructor(**kwargs):
            obj = synthetic_constructor(**kwargs)

            def fail():
                raise ValueError("private engine details")
            obj.resolve_once = fail
            return obj
        with self.assertRaises(mapping.ServerArgsError) as caught:
            construct(self.build(), self.placement(), constructor)
        self.assertEqual(str(caught.exception), "server_args_construction_failed")

    def test_constructor_errors_never_render_private_values(self):
        def broken(**kwargs):
            raise RuntimeError(str(kwargs))
        with self.assertRaises(mapping.ServerArgsError) as caught:
            construct(self.build(), self.placement(), broken)
        self.assertEqual(str(caught.exception), "server_args_construction_failed")
        for secret in (self.root, self.inference.decode(), self.admin.decode()):
            self.assertNotIn(secret, repr(caught.exception))

    def test_import_is_cpu_only_and_the_boundary_requires_a_held_contract(self):
        original = __import__

        def guard(name, *args, **kwargs):
            if name.split(".")[0] in ("sglang", "torch", "torch_memory_saver"):
                self.fail("native import")
            return original(name, *args, **kwargs)
        with mock.patch("builtins.__import__", side_effect=guard):
            importlib.reload(mapping)
        # The guarded import seam is the only native import location, and the
        # audited argument mapper is never reached without a held contract.
        with mock.patch.object(entry, "_guarded_engine_import", side_effect=lambda: (object(), object())), \
                mock.patch.object(entry, "_require_capabilities"), \
                mock.patch.object(mapping, "construct_server_args",
                                  side_effect=AssertionError("constructed")):
            with self.assertRaises((AttributeError, TypeError)):
                entry._import_and_launch(self.build(), object())

    def test_closed_parser_never_exits_or_prints(self):
        parser = mapping._ClosedParser(add_help=False)
        parser.add_argument("--x", type=int)
        with self.assertRaises(mapping.ServerArgsError):
            parser.parse_args(["--x", "y"])
        self.assertIsInstance(parser, argparse.ArgumentParser)



class DiscreteBaselineTest(unittest.TestCase):
    # T26 / ADR 0014 open issue 2: on a discrete device the fraction is of the card.
    def test_device_total_is_the_baseline(self):
        memory = {"device_total_bytes": 16376 * 2**20}
        self.assertEqual(mapping.available_bytes_for(memory), 16376 * 2**20)
        self.assertEqual(mapping.static_fraction(8 * 2**30, 16 * 2**30), 0.5)

    def test_unified_keeps_memavailable(self):
        with mock.patch.object(mapping, "available_memory_bytes", return_value=100):
            self.assertEqual(mapping.available_bytes_for({}), 100)

    def test_bad_device_total_is_refused(self):
        for bad in (0, -1, "16", 2**63, True, 16.0):
            with self.subTest(bad=bad):
                with self.assertRaises(mapping.ServerArgsError):
                    mapping.available_bytes_for({"device_total_bytes": bad})


if __name__ == "__main__":
    unittest.main()
