"""CPU-only recipe mapping tests; constructors below never import SGLang."""

from dataclasses import replace
import importlib
from types import SimpleNamespace
import unittest
from unittest import mock

from runtime import sglang_entry as entry
from runtime import sglang_server_args as mapping
from test_sglang_entry import LaunchFixture


def synthetic_constructor(**kwargs):
    # Native post-init resolves the graph object and the explicit unquant opt-out.
    kwargs["quantization"] = None
    kwargs["_quantization_explicitly_unset"] = True
    kwargs["cuda_graph_config"] = SimpleNamespace(
        prefill=SimpleNamespace(backend="disabled"),
        decode=SimpleNamespace(backend="disabled"))
    return SimpleNamespace(**kwargs)


class MappingTests(LaunchFixture, unittest.TestCase):
    def placement(self, **changes):
        observation = mapping.ObservedPlacement(
            binding_id=self.public["binding_id"], incarnation=self.public["incarnation"],
            host_id="host-a", hardware_fingerprint="hardware-v1",
            device_id="gpu0", memory_domain="uma", physical_gpu_uuid="GPU-selected",
            cuda_visible_uuids=("GPU-other", "GPU-selected"), cuda_index=1)
        return replace(observation, **changes)

    def test_exact_native_mapping_uses_observed_cuda_namespace(self):
        seen = {}
        def constructor(**kwargs):
            seen.update(kwargs)
            return synthetic_constructor(**kwargs)
        result = mapping.construct_server_args(self.build(), self.placement(), constructor)
        expected = {
            "model_path": self.root, "tokenizer_path": self.root,
            "served_model_name": self.public["served_name"],
            "api_key": self.inference.decode(), "admin_api_key": self.admin.decode(),
            "host": "127.0.0.1", "port": 20001, "device": "cuda", "base_gpu_id": 1,
            "tp_size": 1, "dp_size": 1, "pp_size": 1, "ep_size": 1,
            "nnodes": 1, "node_rank": 0, "gpu_id_step": 1,
            "tokenizer_worker_num": 1, "detokenizer_worker_num": 1,
            "dtype": "bfloat16", "kv_cache_dtype": "bfloat16",
            "context_length": 4096, "max_running_requests": 8,
            "max_total_tokens": 4096, "mem_fraction_static": 0.75,
            "disable_prefill_cuda_graph": True, "disable_decode_cuda_graph": True,
            "enable_memory_saver": True, "enable_weights_cpu_backup": False,
            "enable_draft_weights_cpu_backup": False, "trust_remote_code": False,
            "speculative_algorithm": None, "enable_lora": False,
            "disaggregation_mode": "null", "cpu_offload_gb": 0,
            "enable_hierarchical_cache": False, "enable_lmcache": False,
            "enable_flexkv": False, "grpc_port": None, "grpc_mode": False,
            "smg_grpc_mode": False, "sidecar": None, "use_ray": False,
            "skip_server_warmup": True, "warmups": None,
            "log_requests": False, "enable_torch_compile": False,
            "quantization": "unquant",
        }
        for key, value in expected.items():
            with self.subTest(key=key):
                self.assertEqual(seen[key], value)
                self.assertIs(type(seen[key]), type(value))
        for secret in (self.root, self.inference.decode(), self.admin.decode()):
            self.assertNotIn(secret, repr(result))
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
                mapping.construct_server_args(self.build(), self.placement(**changes),
                                              lambda **kw: self.fail("constructor reached"))

    def test_revalidates_launchspec_even_if_directly_constructed(self):
        spec = self.build()
        for bad in (replace(spec, _public_json="{}"),
                    replace(spec, _checkpoint_root="/private/../other"),
                    replace(spec, _admin_key=spec._inference_key)):
            with self.assertRaises(mapping.ServerArgsError):
                mapping.construct_server_args(bad, self.placement(),
                                              lambda **kw: self.fail("constructor reached"))

    def test_rejects_post_init_changes_to_every_explicit_value(self):
        captured = {}
        def capture(**kwargs):
            captured.update(kwargs)
            return synthetic_constructor(**kwargs)
        mapping.construct_server_args(self.build(), self.placement(), capture)
        for key in captured:
            def changed(**kwargs):
                effective = synthetic_constructor(**kwargs)
                setattr(effective, key, object())
                return effective
            with self.subTest(key=key), self.assertRaises(mapping.ServerArgsError):
                mapping.construct_server_args(self.build(), self.placement(), changed)

    def test_effective_graph_object_overrides_are_rejected(self):
        for phase in ("prefill", "decode"):
            def changed(**kwargs):
                effective = synthetic_constructor(**kwargs)
                getattr(effective.cuda_graph_config, phase).backend = "full"
                return effective
            with self.subTest(phase=phase), self.assertRaises(mapping.ServerArgsError):
                mapping.construct_server_args(self.build(), self.placement(), changed)

    def test_quantization_opt_out_must_survive_native_resolution(self):
        def changed(**kwargs):
            effective = synthetic_constructor(**kwargs)
            effective._quantization_explicitly_unset = False
            return effective
        with self.assertRaises(mapping.ServerArgsError):
            mapping.construct_server_args(self.build(), self.placement(), changed)

    def test_fraction_is_derived_from_frozen_basis_points(self):
        self.public["settings"]["requested_budget"]["static_memory_fraction_bps"] = 1234
        self.public["static_memory_fraction"] = "0.1234"
        def constructor(**kwargs):
            self.assertEqual(kwargs["mem_fraction_static"], 0.1234)
            return synthetic_constructor(**kwargs)
        mapping.construct_server_args(self.build(), self.placement(), constructor)

    def test_equal_but_wrong_effective_type_fails(self):
        for key, replacement in (("enable_memory_saver", 1), ("tp_size", True),
                                 ("mem_fraction_static", "0.75")):
            def changed(**kwargs):
                effective = synthetic_constructor(**kwargs)
                setattr(effective, key, replacement)
                return effective
            with self.subTest(key=key), self.assertRaises(mapping.ServerArgsError):
                mapping.construct_server_args(self.build(), self.placement(), changed)

    def test_delayed_mutation_and_missing_fields_fail_revalidation(self):
        native = []
        def constructor(**kwargs):
            result = synthetic_constructor(**kwargs)
            native.append(result)
            return result
        checked = mapping.construct_server_args(self.build(), self.placement(), constructor)
        del native[0].api_key
        with self.assertRaises(mapping.ServerArgsError):
            checked.revalidate()

    def test_constructor_errors_never_render_private_values(self):
        def broken(**kwargs):
            raise RuntimeError(str(kwargs))
        with self.assertRaises(mapping.ServerArgsError) as caught:
            mapping.construct_server_args(self.build(), self.placement(), broken)
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
            # The composition runs its gates without any native import and
            # denies through the gate's own closed category.
            with self.assertRaises(entry.LaunchError):
                entry._verified_native_contract(self.build(), object())
        # The guarded import seam is the only native import location, and the
        # audited argument mapper is never reached without a held contract.
        with mock.patch.object(entry, "_guarded_engine_import", side_effect=lambda: (object(), object())), \
                mock.patch.object(mapping, "construct_server_args",
                                  side_effect=AssertionError("constructed")):
            with self.assertRaises((AttributeError, TypeError)):
                entry._import_and_launch(self.build(), object(), object())


if __name__ == "__main__":
    unittest.main()
