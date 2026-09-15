"""Synthetic CPU fixtures: these tests are not native checkpoint evidence."""

import builtins
from dataclasses import FrozenInstanceError, replace
import hashlib
import json
import os
from pathlib import Path
import tempfile
import importlib
import unittest
from unittest import mock

from runtime import checkpoint_preflight as preflight
from runtime.checkpoint_manifest import (
    _Artifact, _Geometry, _Manifest, _PINNED_GEOMETRY, _PINNED_MANIFEST,
)


class ArtifactTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="mllm-checkpoint-test-")
        self.addCleanup(self.temp.cleanup)
        self.parent = Path(self.temp.name) / "parent"
        self.root = self.parent / "checkpoint"
        self.root.mkdir(parents=True)
        self.data = b"synthetic fixture only"
        (self.root / "config.json").write_bytes(self.data)
        self.manifest = _Manifest((
            _Artifact("config.json", hashlib.sha256(self.data).hexdigest(), None),
        ))

    def verify(self):
        return preflight._observe(str(self.root), self.manifest)

    def rejects(self, code, operation=None):
        with self.assertRaises(preflight.CheckpointPreflightError) as caught:
            (operation or self.verify)()
        self.assertEqual(caught.exception.code, code)
        self.assertNotIn(str(self.root), str(caught.exception))
        self.assertNotIn(str(self.root), repr(caught.exception))

    def test_missing_artifact(self):
        (self.root / "config.json").unlink()
        self.rejects("artifact_missing")

    def test_changed_artifact_bytes(self):
        (self.root / "config.json").write_bytes(b"x" * len(self.data))
        self.rejects("artifact_mismatch")

    def test_replacement_during_read(self):
        original = os.read
        replaced = False

        def read(fd, size):
            nonlocal replaced
            result = original(fd, size)
            if not replaced:
                replaced = True
                leaf = self.root / "config.json"
                leaf.rename(self.root / "old")
                leaf.write_bytes(self.data)
            return result

        with mock.patch.object(preflight.os, "read", side_effect=read):
            self.rejects("artifact_changed")

    def test_public_verifier_does_not_accept_fixture_hashes(self):
        self.rejects("artifact_mismatch", lambda: preflight.verify_checkpoint(str(self.root)))

    def test_valid_inventory_and_extra_file_exclusion(self):
        (self.root / "extra").write_bytes(b"not attested")
        observed = self.verify()
        self.assertIsNotNone(observed)
        self.assertEqual(tuple(a.name for a in observed.artifacts), ("config.json",))
        self.assertEqual(observed.artifacts[0].size, 22)

    def test_invalid_roots(self):
        for root in ("relative", str(self.root) + "/", str(self.root) + "/.",
                     str(self.root) + "/../checkpoint", "//tmp", "/tmp//x", "/x\0y"):
            with self.subTest(root=root):
                self.rejects("invalid_root", lambda: preflight._observe(root, self.manifest))

    def test_symlink_and_special_leaves(self):
        leaf = self.root / "config.json"
        for kind in ("symlink", "fifo", "directory"):
            with self.subTest(kind=kind):
                leaf.unlink()
                if kind == "symlink":
                    leaf.symlink_to(self.root / "missing")
                elif kind == "fifo":
                    os.mkfifo(leaf)
                else:
                    leaf.mkdir()
                self.rejects("unsafe_file")
                if kind == "directory":
                    leaf.rmdir()
                else:
                    leaf.unlink()
                leaf.write_bytes(self.data)

    def test_root_and_ancestor_symlinks(self):
        for directory in (self.root, self.parent):
            with self.subTest(directory=directory):
                moved = directory.with_name(directory.name + "-moved")
                directory.rename(moved)
                directory.symlink_to(moved, target_is_directory=True)
                self.rejects("unsafe_file")
                directory.unlink()
                moved.rename(directory)

    def test_size_and_ancillary_limit(self):
        self.manifest = replace(self.manifest, artifacts=(replace(self.manifest.artifacts[0], size=1),))
        self.rejects("artifact_mismatch")
        self.manifest = replace(self.manifest, artifacts=(replace(self.manifest.artifacts[0], size=None),))
        with (self.root / "config.json").open("wb") as stream:
            stream.truncate(16 * 1024 * 1024 + 1)
        self.rejects("artifact_mismatch")

    def test_parent_root_and_in_place_mutation(self):
        for target in ("parent", "root", "contents"):
            with self.subTest(target=target):
                original = os.read
                mutated = False
                moved = None

                def read(fd, size):
                    nonlocal mutated, moved
                    result = original(fd, size)
                    if not mutated:
                        mutated = True
                        if target == "contents":
                            (self.root / "config.json").write_bytes(self.data)
                        else:
                            directory = self.parent if target == "parent" else self.root
                            moved = directory.with_name(directory.name + "-old")
                            directory.rename(moved)
                            self.root.mkdir(parents=True)
                            (self.root / "config.json").write_bytes(self.data)
                    return result

                with mock.patch.object(preflight.os, "read", side_effect=read):
                    self.rejects("artifact_changed")
                if moved is not None:
                    (self.root / "config.json").unlink()
                    self.root.rmdir()
                    directory = self.parent if target == "parent" else self.root
                    if target == "parent":
                        self.parent.rmdir()
                    moved.rename(directory)

    def test_descriptor_cleanup_and_sanitized_io(self):
        before = set(os.listdir("/proc/self/fd"))
        self.verify()
        self.assertEqual(set(os.listdir("/proc/self/fd")), before)
        with mock.patch.object(preflight.os, "read", side_effect=OSError("SECRET " + str(self.root))):
            self.rejects("io_error")
        self.assertEqual(set(os.listdir("/proc/self/fd")), before)

    def test_read_chunk_limit(self):
        data = b"x" * (2 * 1024 * 1024 + 13)
        (self.root / "config.json").write_bytes(data)
        self.manifest = _Manifest((_Artifact("config.json", hashlib.sha256(data).hexdigest(), None),))
        original = os.read
        sizes = []

        def read(fd, size):
            sizes.append(size)
            self.assertLessEqual(size, 1024 * 1024)
            return original(fd, size)

        with mock.patch.object(preflight.os, "read", side_effect=read):
            result = self.verify()
        self.assertIsNotNone(result)
        self.assertEqual(sum(sizes), len(data))

    def test_unsupported_platform(self):
        with mock.patch.object(preflight.sys, "platform", "win32"):
            self.rejects("unsupported_platform")

    def test_short_reads_and_truncation(self):
        original = os.read
        with mock.patch.object(preflight.os, "read", side_effect=lambda fd, size: original(fd, min(size, 3))):
            self.assertEqual(self.verify().artifacts[0].size, 22)
        (self.root / "config.json").write_bytes(self.data)

        def truncate(fd, size):
            (self.root / "config.json").write_bytes(b"")
            return original(fd, size)

        with mock.patch.object(preflight.os, "read", side_effect=truncate):
            self.rejects("artifact_changed")

    def test_final_entry_disappearance_and_symlink_replacement(self):
        before = set(os.listdir("/proc/self/fd"))
        for symlink in (False, True):
            with self.subTest(symlink=symlink):
                def inspect(contents):
                    leaf = self.root / "config.json"
                    leaf.unlink()
                    if symlink:
                        leaf.symlink_to(self.root / "missing")

                self.rejects("artifact_changed", lambda: preflight._observe(
                    str(self.root), self.manifest, inspect))
                if symlink:
                    (self.root / "config.json").unlink()
                (self.root / "config.json").write_bytes(self.data)
                self.assertEqual(set(os.listdir("/proc/self/fd")), before)

    def test_sanitized_public_io_error_suppresses_cause(self):
        with mock.patch.object(preflight.os, "open", side_effect=PermissionError("PRIVATE " + str(self.root))):
            with self.assertRaises(preflight.CheckpointPreflightError) as caught:
                preflight.verify_checkpoint(str(self.root))
        self.assertEqual(caught.exception.code, "io_error")
        self.assertEqual(str(caught.exception), "io_error")
        self.assertTrue(caught.exception.__suppress_context__)
        self.assertIsNone(caught.exception.__cause__)


_TINY_SHAPES = {
    "model.embed_tokens.weight": (4, 2),
    "model.norm.weight": (2,),
    "model.layers.0.input_layernorm.weight": (2,),
    "model.layers.0.post_attention_layernorm.weight": (2,),
    "model.layers.0.mlp.down_proj.weight": (2, 3),
    "model.layers.0.mlp.gate_proj.weight": (3, 2),
    "model.layers.0.mlp.up_proj.weight": (3, 2),
    "model.layers.0.self_attn.k_norm.weight": (2,),
    "model.layers.0.self_attn.q_norm.weight": (2,),
    "model.layers.0.self_attn.k_proj.weight": (2, 2),
    "model.layers.0.self_attn.v_proj.weight": (2, 2),
    "model.layers.0.self_attn.q_proj.weight": (4, 2),
    "model.layers.0.self_attn.o_proj.weight": (2, 4),
}
_TINY_GEOMETRY = _Geometry(1, 2, 3, 2, 1, 2, 4, 16)


def _json_bytes(value):
    return json.dumps(value, separators=(",", ":")).encode()


class StorageTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="mllm-checkpoint-storage-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        # Complete structure from the observed pinned config, with tiny geometry.
        self.config = {
            "architectures": ["Qwen3ForCausalLM"], "attention_bias": False,
            "attention_dropout": 0.0, "bos_token_id": 151643, "eos_token_id": 151645,
            "head_dim": 2, "hidden_act": "silu", "hidden_size": 2,
            "initializer_range": 0.02, "intermediate_size": 3,
            "max_position_embeddings": 16, "max_window_layers": 1,
            "model_type": "qwen3", "num_attention_heads": 2, "num_hidden_layers": 1,
            "num_key_value_heads": 1, "rms_norm_eps": 1e-6, "rope_scaling": None,
            "rope_theta": 5000000, "sliding_window": None, "tie_word_embeddings": True,
            "torch_dtype": "bfloat16", "transformers_version": "4.51.0",
            "use_cache": True, "use_sliding_window": False, "vocab_size": 4,
        }
        self.headers = ({"__metadata__": {"format": "pt"}}, {})
        self.payloads = [bytearray(), bytearray()]
        self.shards = ("model-00001-of-00002.safetensors", "model-00002-of-00002.safetensors")
        self.index = {"metadata": {"total_size": 655480}, "weight_map": {}}
        for i, (name, shape) in enumerate(_TINY_SHAPES.items()):
            shard = i % 2
            count = 2
            for dimension in shape:
                count *= dimension
            start = len(self.payloads[shard])
            self.payloads[shard].extend(b"\0" * count)
            self.headers[shard][name] = {"dtype": "BF16", "shape": list(shape),
                                         "data_offsets": [start, start + count]}
            self.index["weight_map"][name] = self.shards[shard]
        self.overrides = {}

    def manifest(self):
        data = {"config.json": _json_bytes(self.config),
                "model.safetensors.index.json": _json_bytes(self.index)}
        for name, header, payload in zip(self.shards, self.headers, self.payloads):
            encoded = _json_bytes(header)
            data[name] = len(encoded).to_bytes(8, "little") + encoded + payload
        for name in ("tokenizer_config.json", "tokenizer.json", "vocab.json",
                     "merges.txt", "generation_config.json"):
            data[name] = b"synthetic opaque artifact"
        data.update(self.overrides)
        artifacts = []
        for name, content in data.items():
            (self.root / name).write_bytes(content)
            artifacts.append(_Artifact(name, hashlib.sha256(content).hexdigest(),
                                       len(content) if name.endswith(".safetensors") else None))
        return _Manifest(tuple(artifacts), _TINY_GEOMETRY)

    def verify(self):
        return preflight._verify(str(self.root), self.manifest())

    def rejects(self, code):
        with self.assertRaises(preflight.CheckpointPreflightError) as caught:
            self.verify()
        self.assertEqual(caught.exception.code, code)

    def tensor(self):
        return self.headers[0]["model.embed_tokens.weight"]

    def test_synthetic_geometry_and_advisory_discrepancy(self):
        result = self.verify()
        self.assertEqual(getattr(result, "tensor_count", None), 13)
        self.assertEqual(result.payload_bytes, 120)
        self.assertEqual(result.index_total_size, 655480)
        self.assertEqual(result.index_discrepancy, 655360)
        self.assertEqual(result.model_maximum_context, 16)

    def test_generated_tiny_tensor_names_and_shapes(self):
        self.assertEqual(preflight._expected_tensors(_TINY_GEOMETRY), _TINY_SHAPES)

    def test_selected_geometry_exact_payload(self):
        expected = preflight._expected_tensors(_PINNED_GEOMETRY)
        self.assertEqual(len(expected), 398)
        self.assertEqual(sum(preflight._tensor_bytes(shape) for shape in expected.values()), 8044936192)
        self.assertEqual(expected["model.embed_tokens.weight"], (151936, 2560))
        self.assertEqual(expected["model.layers.35.self_attn.q_proj.weight"], (4096, 2560))
        self.assertNotIn("lm_head.weight", expected)

    def test_config_recipe_mismatches(self):
        cases = {"architectures": ["Other"], "torch_dtype": "float32", "num_hidden_layers": 2,
                 "hidden_size": True, "intermediate_size": 4, "num_attention_heads": 3,
                 "num_key_value_heads": 2, "head_dim": 4, "vocab_size": 5,
                 "max_position_embeddings": 4096, "tie_word_embeddings": False,
                 "use_sliding_window": True, "sliding_window": 16, "rope_scaling": {},
                 "quantization_config": {}, "num_experts": 1, "layer_types": [],
                 "attention_bias": True, "model_type": "qwen3_moe"}
        for key, value in cases.items():
            with self.subTest(key=key):
                old = dict(self.config)
                self.config[key] = value
                self.rejects("unsupported_geometry")
                self.config = old

    def test_missing_config_field(self):
        del self.config["head_dim"]
        self.rejects("unsupported_geometry")

    def test_strict_json(self):
        for content in (b'{"x":1,"x":2}', b'{"x":NaN}', b'{"x":Infinity}',
                        b'{"x":1e999}', b'{"x":\xff}', b'{"x":"\\ud800"}',
                        b"[" * 33 + b"0" + b"]" * 33, b'{"x":18446744073709551616}'):
            with self.subTest(content=content[:40]):
                self.overrides["config.json"] = content
                self.rejects("invalid_json")

    def test_header_length_bounds(self):
        for content in (b"1234567", (1048577).to_bytes(8, "little"),
                        (1000).to_bytes(8, "little") + b"{}", b"\0" * 8):
            with self.subTest(content=content[:10]):
                self.overrides[self.shards[0]] = content
                self.rejects("invalid_storage")

    def test_invalid_tensor_records(self):
        changes = ({"dtype": "F16"}, {"shape": [2, 4]}, {"shape": [True, 2]},
                   {"shape": [1] * 9}, {"shape": [1 << 63, 2]},
                   {"data_offsets": [-1, 15]}, {"data_offsets": [True, 16]},
                   {"data_offsets": [16, 0]}, {"data_offsets": [0, 15]},
                   {"data_offsets": [0, 16, 32]}, {"surprise": 1})
        for change in changes:
            with self.subTest(change=change):
                old = dict(self.tensor())
                self.tensor().update(change)
                self.rejects("invalid_storage")
                self.headers[0]["model.embed_tokens.weight"] = old

    def test_tensor_name_coverage(self):
        for name in ("lm_head.weight", "model.layers.1.input_layernorm.weight", "x" * 257):
            with self.subTest(name=name):
                record = self.headers[0].pop("model.embed_tokens.weight")
                self.headers[0][name] = record
                try:
                    self.rejects("invalid_storage")
                finally:
                    del self.headers[0][name]
                    self.headers[0]["model.embed_tokens.weight"] = record

    def test_missing_tensor(self):
        del self.headers[0]["model.embed_tokens.weight"]
        self.rejects("invalid_storage")

    def test_duplicate_tensor_across_shards(self):
        self.headers[1]["model.embed_tokens.weight"] = dict(self.tensor())
        self.rejects("invalid_storage")

    def test_wrong_index_assignment_and_index_escape(self):
        for name in (self.shards[1], "../escape", "/absolute", "missing.safetensors"):
            with self.subTest(name=name):
                self.index["weight_map"]["model.embed_tokens.weight"] = name
                self.rejects("invalid_storage")

    def test_index_structure_and_advisory_value(self):
        original = self.index
        for index in ({"weight_map": original["weight_map"]},
                      {**original, "extra": 1}, {**original, "metadata": {"total_size": True}},
                      {**original, "metadata": {"total_size": 120}},
                      {**original, "metadata": {"total_size": 655480, "extra": 1}},
                      {**original, "weight_map": []}):
            with self.subTest(index=index.keys()):
                self.index = index
                self.rejects("invalid_storage")
        self.index = original

    def test_gaps_overlaps_and_trailing_payload(self):
        for shift in (-2, 2):
            with self.subTest(shift=shift):
                record = self.headers[0]["model.layers.0.input_layernorm.weight"]
                old = record["data_offsets"]
                record["data_offsets"] = [value + shift for value in old]
                self.rejects("invalid_storage")
                record["data_offsets"] = old
        self.payloads[0].extend(b"xx")
        self.rejects("invalid_storage")

    def test_unknown_header_metadata(self):
        self.headers[0]["__metadata__"] = {"format": 1}
        self.rejects("invalid_storage")

    def test_container_limits(self):
        self.headers[0].update({f"extra.{i}": {} for i in range(4097)})
        self.rejects("invalid_storage")

    def test_shard_payload_is_streamed_without_retention(self):
        content = (2).to_bytes(8, "little") + b"{}" + b"x" * (2 * 1024 * 1024)
        self.overrides[self.shards[0]] = content
        manifest = self.manifest()
        captured = {}
        preflight._observe(str(self.root), manifest, lambda values: captured.update(values))
        self.assertLessEqual(len(captured[self.shards[0]]), 1024 * 1024 + 8)

    def test_duplicate_index_and_header_keys(self):
        for name, content in (("model.safetensors.index.json", b'{"metadata":{},"metadata":{}}'),
                              (self.shards[0], (13).to_bytes(8, "little") + b'{"x":1,"x":2}')):
            with self.subTest(name=name):
                self.overrides = {name: content}
                self.rejects("invalid_json")

    def test_deep_header_and_index_container_bounds(self):
        content = b"[" * 33 + b"0" + b"]" * 33
        self.overrides[self.shards[0]] = len(content).to_bytes(8, "little") + content
        self.rejects("invalid_json")
        self.overrides.clear()
        self.index["weight_map"] = {f"extra.{i}": self.shards[0] for i in range(4097)}
        self.rejects("invalid_storage")

    def test_shape_product_and_unsigned_bounds(self):
        for shape in ([True], [0], [-1], [1 << 64], [1 << 63], [1] * 9):
            with self.subTest(shape=shape):
                with self.assertRaises(preflight.CheckpointPreflightError) as caught:
                    preflight._tensor_bytes(shape)
                self.assertEqual(caught.exception.code, "invalid_storage")

    def test_all_fixture_files_are_hashed_and_required(self):
        manifest = self.manifest()
        for artifact in manifest.artifacts:
            with self.subTest(name=artifact.name):
                leaf = self.root / artifact.name
                original = leaf.read_bytes()
                changed = bytearray(original)
                changed[-1] ^= 1
                leaf.write_bytes(changed)
                with self.assertRaises(preflight.CheckpointPreflightError) as caught:
                    preflight._verify(str(self.root), manifest)
                self.assertEqual(caught.exception.code, "artifact_mismatch")
                leaf.unlink()
                with self.assertRaises(preflight.CheckpointPreflightError) as caught:
                    preflight._verify(str(self.root), manifest)
                self.assertEqual(caught.exception.code, "artifact_missing")
                leaf.write_bytes(original)

    def test_shard_file_size_mismatch(self):
        manifest = self.manifest()
        with (self.root / self.shards[0]).open("ab") as stream:
            stream.write(b"x")
        with self.assertRaises(preflight.CheckpointPreflightError) as caught:
            preflight._verify(str(self.root), manifest)
        self.assertEqual(caught.exception.code, "artifact_mismatch")

    def test_json_nesting_boundary_and_escaped_brackets(self):
        value = preflight._strict_json(b"[" * 32 + b'"\\\"[}"' + b"]" * 32)
        for _ in range(32):
            value = value[0]
        self.assertEqual(value, '"[}')

    def test_published_config_bytes_pass_public_hash_then_require_index(self):
        self.config.update(head_dim=128, hidden_size=2560, intermediate_size=9728,
                           max_position_embeddings=262144, max_window_layers=36,
                           num_attention_heads=32, num_hidden_layers=36,
                           num_key_value_heads=8, vocab_size=151936)
        content = json.dumps(self.config, indent=2).encode()
        self.assertEqual(len(content), 727)
        self.assertEqual(hashlib.sha256(content).hexdigest(),
                         "5beea1a4a34c62782bfb2f911c606741a3bab8f92d80a118fa053c28af12e8ba")
        (self.root / "config.json").write_bytes(content)
        with self.assertRaises(preflight.CheckpointPreflightError) as caught:
            preflight.verify_checkpoint(str(self.root))
        self.assertEqual(caught.exception.code, "artifact_missing")
        preflight._validate_config(preflight._strict_json(content), _PINNED_GEOMETRY)

    def test_all_ten_published_artifact_identities(self):
        # These literal pins are the public compatibility contract, not fixture hashes.
        expected = {
            "config.json": ("5beea1a4a34c62782bfb2f911c606741a3bab8f92d80a118fa053c28af12e8ba", None),
            "model.safetensors.index.json": ("d6c42883a895dfef5b0080ed2116a1bcd764f558406b98923d675978a1abf29c", None),
            "model-00001-of-00003.safetensors": ("75311d91bb08cf0b882913da464a1e722a31fb44db35208663487efb7a3d8ed6", 3957900840),
            "model-00002-of-00003.safetensors": ("0b48adbb1f60e901153d91907ba11ce63bd4b8b584482e730f48808d055dfba1", 3987450520),
            "model-00003-of-00003.safetensors": ("7dd39ccca5e4de123c74c14af44c9bf2eb75df33b4614382af0134528e060d5d", 99630640),
            "tokenizer_config.json": ("a62ff0a2472a0fa1b8eaabcb57c59b58afa42a22831dc141400b6e0cf2b65ce3", None),
            "tokenizer.json": ("aeb13307a71acd8fe81861d94ad54ab689df773318809eed3cbe794b4492dae4", None),
            "vocab.json": ("ca10d7e9fb3ed18575dd1e277a2579c16d108e32f27439684afa0e10b1440910", None),
            "merges.txt": ("599bab54075088774b1733fde865d5bd747cbcc7a547c5bc12610e874e26f5e3", None),
            "generation_config.json": ("835fffe355c9438e7a25be099b3fccaa98350b83451f9fd2d99512e74f1ade48", None),
        }
        self.assertEqual({item.name: (item.sha256, item.size) for item in _PINNED_MANIFEST.artifacts}, expected)


class RevalidationTests(unittest.TestCase):
    def setUp(self):
        fixture = StorageTests()
        fixture.setUp()
        self.addCleanup(fixture.doCleanups)
        self.root = fixture.root
        self.manifest = fixture.manifest()
        self.previous = preflight._verify(str(self.root), self.manifest)

    def rejects(self, code, operation):
        with self.assertRaises(preflight.CheckpointPreflightError) as caught:
            operation()
        self.assertEqual(caught.exception.code, code)

    def test_revalidation_rereads_all_bytes(self):
        original = os.read
        count = 0

        def read(fd, size):
            nonlocal count
            data = original(fd, size)
            count += len(data)
            return data

        with mock.patch.object(preflight.os, "read", side_effect=read):
            current = preflight._revalidate(self.previous, self.manifest)
        self.assertEqual(count, sum(item.size for item in self.previous.artifacts))
        self.assertEqual(current, self.previous)
        self.assertIsNot(current, self.previous)

    def test_revalidation_rejects_byte_identical_inode_replacement(self):
        leaf = self.root / "config.json"
        data = leaf.read_bytes()
        leaf.rename(self.root / "retained-old-config")
        leaf.write_bytes(data)
        self.rejects("artifact_changed", lambda: preflight._revalidate(self.previous, self.manifest))

    def test_revalidation_rejects_changed_contents(self):
        leaf = self.root / "merges.txt"
        leaf.write_bytes(b"x" * leaf.stat().st_size)
        self.rejects("artifact_mismatch", lambda: preflight._revalidate(self.previous, self.manifest))

    def test_public_revalidation_does_not_accept_synthetic_manifest(self):
        self.rejects("artifact_mismatch", lambda: preflight.revalidate_checkpoint(self.previous))

    def test_revalidation_rejects_non_result(self):
        self.rejects("invalid_root", lambda: preflight.revalidate_checkpoint({"root": str(self.root)}))

    def test_observation_is_immutable_and_repr_omits_path(self):
        self.assertNotIn(str(self.root), repr(self.previous))
        self.assertNotIn("synthetic opaque artifact", repr(self.previous))
        self.assertIsInstance(self.previous.artifacts, tuple)
        self.assertIsInstance(self.previous.ancestors, tuple)
        with self.assertRaises(FrozenInstanceError):
            self.previous.payload_bytes = 0
        with self.assertRaises(FrozenInstanceError):
            self.previous.artifacts[0].identity.size = 0
        self.assertEqual(self.previous.root_identity.inode, self.root.stat().st_ino)

    def test_reconstructed_data_never_skips_verification(self):
        forged = replace(self.previous, payload_bytes=0)
        current = preflight._revalidate(forged, self.manifest)
        self.assertEqual(current.payload_bytes, 120)

    def test_entrypoints_have_no_manifest_or_test_mode_argument(self):
        with self.assertRaises(TypeError):
            preflight.verify_checkpoint(str(self.root), self.manifest)
        with self.assertRaises(TypeError):
            preflight.revalidate_checkpoint(self.previous, self.manifest)

    def test_import_and_verification_have_no_engine_network_or_process_effects(self):
        original_import = builtins.__import__

        def restricted_import(name, *args, **kwargs):
            if name.split(".")[0] in {"torch", "safetensors", "transformers", "vllm", "sglang",
                                      "subprocess", "socket", "urllib", "http", "requests"}:
                self.fail(f"Unexpected import: {name}")
            return original_import(name, *args, **kwargs)

        # Execute a fresh module namespace so import side effects are exercised,
        # without changing the dataclass identities in other tests.
        name = "runtime._checkpoint_preflight_effect_test"
        spec = importlib.util.spec_from_file_location(name, preflight.__file__)
        module = importlib.util.module_from_spec(spec)
        with mock.patch.dict(preflight.sys.modules, {name: module}):
            with mock.patch.object(builtins, "__import__", side_effect=restricted_import):
                spec.loader.exec_module(module)
                self.assertEqual(module._verify(str(self.root), self.manifest).payload_bytes, 120)


if __name__ == "__main__":
    unittest.main()
