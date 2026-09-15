"""CPU-only startup boundary tests; no native runtime or qualification evidence."""

import builtins
import copy
import importlib
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

from runtime import sglang_entry as entry
from runtime import checkpoint_preflight as preflight
import test_checkpoint_preflight as checkpoint_fixtures


def finish_without_io(coroutine):
    """Drive in-memory ASGI coroutines without an event loop or socketpair."""
    try:
        coroutine.send(None)
    except StopIteration as done:
        return done.value
    finally:
        coroutine.close()
    raise AssertionError("unexpected I/O suspension")


def public_settings():
    return {
        "schema_version": 1, "kind": "sglang_candidate_launch", "engine": "sglang",
        "recipe": "qwen3_4b_instruct2507_tp1_dp1_bf16_disk_reload_v1",
        "source_revision": "fdebc938f7f4d16fe6b9f55dcd9a767cf0899ea1",
        "checkpoint_revision": "cdbee75f17c01a7cc42f958dc650907174af0554",
        "binding_id": "01K00000000000000000000001",
        "incarnation": "01K00000000000000000000099",
        "endpoint": "http://127.0.0.1:20001",
        "served_name": "candidate-01K00000000000000000000001",
        "rendered_settings_digest": "a" * 64,
        "settings": {
            "recipe": "qwen3_4b_instruct2507_tp1_dp1_bf16_disk_reload_v1",
            "tensor_parallel_size": 1, "data_parallel_size": 1, "tokenizer_workers": 1,
            "model_dtype": "bfloat16", "context_tokens": 4096,
            "max_running_requests": 8, "max_total_tokens": 4096,
            "prefill_cuda_graphs": False, "decode_cuda_graphs": False,
            "memory_saver": True, "cpu_weight_backup": False,
            "speculative_decoding": False, "lora": False, "trust_remote_code": False,
            "disaggregation": False, "external_cache": False,
            "cpu_kv_offload": False, "native_grpc": False, "weight_restore": "disk_reload",
            "requested_budget": {"kv_cache_bytes": 4294967296,
                                 "static_memory_fraction_bps": 7500},
        },
        "minimum_kv_bytes": 603979776, "static_memory_fraction": "0.7500",
    }


class LaunchFixture:
    def setUp(self):
        self.public = public_settings()
        self.root = "/private/checkpoints/qwen"
        self.inference = b"inference-private-" + b"a" * 32
        self.admin = b"admin-private-" + b"b" * 32

    def argv(self, public=None):
        return ["--public-settings-json", json.dumps(public or self.public),
                "--launch-descriptor-fd", "3", "--inference-credential-fd", "4",
                "--admin-credential-fd", "5"]

    def payloads(self, public=None, root=None):
        return {3: json.dumps({"schema_version": 1,
                              "kind": "sglang_candidate_private_launch",
                              "checkpoint_root": self.root if root is None else root,
                              "public_settings": self.public if public is None else public}).encode(),
                4: self.inference, 5: self.admin}

    def build(self, argv=None, payloads=None):
        data = self.payloads() if payloads is None else payloads
        return entry.build_launch(self.argv() if argv is None else argv, data.__getitem__)

    def rejects(self, argv=None, payloads=None):
        with self.assertRaises(entry.LaunchError) as caught:
            self.build(argv, payloads)
        for private in (self.root, self.inference.decode(), self.admin.decode()):
            self.assertNotIn(private, str(caught.exception))
            self.assertNotIn(private, repr(caught.exception))

class LaunchTests(LaunchFixture, unittest.TestCase):
    def test_valid_descriptor_stays_private_and_cannot_mutate(self):
        spec = self.build()
        for private in (self.root, self.inference.decode(), self.admin.decode()):
            self.assertNotIn(private, repr(spec))
            self.assertNotIn(private, str(spec))
        with self.assertRaises((AttributeError, TypeError)):
            spec.checkpoint_root = "/changed"

    def test_unknown_duplicate_and_mismatched_public_fields_fail(self):
        data = self.payloads()
        data[3] = data[3].replace(b'"schema_version": 1', b'"schema_version": 1, "schema_version": 1', 1)
        self.rejects(payloads=data)
        for target in ("private", "public", "settings", "budget"):
            public = copy.deepcopy(self.public)
            private = json.loads(self.payloads()[3])
            if target == "private":
                private["extra"] = "forbidden"
            elif target == "public":
                public["extra"] = "forbidden"
            elif target == "settings":
                public["settings"]["extra"] = True
            else:
                public["settings"]["requested_budget"]["extra"] = 1
            private["public_settings"] = public
            data = self.payloads()
            data[3] = json.dumps(private).encode()
            self.rejects(argv=self.argv(public), payloads=data)
        data = self.payloads()
        data[3] = data[3].replace(b"20001", b"20002")
        self.rejects(payloads=data)
        argv = self.argv()
        argv[1] = argv[1].replace('"memory_saver": true', '"memory_saver": true, "memory_saver": true')
        self.rejects(argv=argv)

    def test_json_whitespace_does_not_change_the_bound_descriptor(self):
        data = self.payloads()
        data[3] = json.dumps(json.loads(data[3]), indent=2).encode()
        argv = self.argv()
        argv[1] = json.dumps(self.public, indent=2)
        self.build(argv, data)

    def test_closed_recipe_types_and_bounds(self):
        mutations = [
            ("source_revision", "main"), ("checkpoint_revision", "main"),
            ("schema_version", True), ("schema_version", 1.0),
            ("engine", "vllm"), ("recipe", "restart"),
            ("binding_id", "ordinary"), ("incarnation", ""),
            ("endpoint", "http://0.0.0.0:20001"),
            ("endpoint", "http://127.0.0.1:020001"),
            ("endpoint", "http://127.0.0.1:65536"),
            ("served_name", "ordinary-model"), ("rendered_settings_digest", "z" * 64),
            ("minimum_kv_bytes", 603979775), ("static_memory_fraction", "0.75"),
        ]
        for key, value in mutations:
            with self.subTest(key=key, value=value):
                public = copy.deepcopy(self.public)
                public[key] = value
                self.rejects(self.argv(public), self.payloads(public))
        for key, original in self.public["settings"].items():
            if key == "requested_budget":
                continue
            public = copy.deepcopy(self.public)
            public["settings"][key] = (not original if type(original) is bool else
                                        original + 1 if type(original) is int else "other")
            with self.subTest(setting=key):
                self.rejects(self.argv(public), self.payloads(public))
        for key, value in (("kv_cache_bytes", 603979775), ("kv_cache_bytes", 2 ** 63),
                           ("kv_cache_bytes", True), ("static_memory_fraction_bps", 0),
                           ("static_memory_fraction_bps", 10001),
                           ("static_memory_fraction_bps", 7500.0)):
            public = copy.deepcopy(self.public)
            public["settings"]["requested_budget"][key] = value
            self.rejects(self.argv(public), self.payloads(public))

    def test_unsafe_checkpoint_roots_and_bounded_utf8(self):
        for root in ("relative", "/", "//tmp", "/tmp/", "/tmp//qwen", "/tmp/./qwen",
                     "/tmp/../qwen", "/tmp/\0qwen", "/tmp/\nqwen", "/" + "x" * 4096,
                     "/tmp/\ud800"):
            with self.subTest(root=repr(root)):
                self.rejects(payloads=self.payloads(root=root))
        for raw in (b"\xff", b"{" * 65537, b'{"x": NaN}', b"[" * 2000):
            data = self.payloads()
            data[3] = raw
            self.rejects(payloads=data)

    def test_missing_oversized_equal_and_invalid_credentials(self):
        for value in (b"", b"a" * 4097, b"Bearer " + b"a" * 32,
                      b"a" * 32 + b"\n", b"\xff" * 32, self.admin):
            data = self.payloads()
            data[4] = value
            self.rejects(payloads=data)
        data = self.payloads()
        del data[5]
        self.rejects(payloads=data)
        data = self.payloads()
        data[4] = b"short-but-valid"
        self.build(payloads=data)

    def test_descriptor_numbers_and_cli_are_closed(self):
        for value in ("0", "2", "-1", "2147483648", "03", "4", "3.0"):
            argv = self.argv()
            argv[3] = value
            self.rejects(argv=argv)
        for tail in (["--unknown", "value"], ["--launch-descriptor-fd", "6"], ["--help"]):
            self.rejects(argv=self.argv() + tail)

    def test_import_and_build_do_not_import_engines_or_effectful_modules(self):
        original = builtins.__import__

        def guarded(name, *args, **kwargs):
            if name.split(".")[0] in {"sglang", "torch", "subprocess", "socket", "urllib",
                                      "http", "requests", "transformers", "vllm"}:
                raise AssertionError("effectful import")
            return original(name, *args, **kwargs)

        with mock.patch.object(builtins, "__import__", side_effect=guarded):
            importlib.reload(entry)
            self.build()

    def test_production_fd_reader_is_bounded_and_closes_consumed_descriptor(self):
        with tempfile.TemporaryFile() as stream:
            stream.write(b"descriptor")
            stream.flush()
            stream.seek(0)
            fd = os.dup(stream.fileno())
            self.assertEqual(entry._read_descriptor(fd), b"descriptor")
            with self.assertRaises(OSError):
                os.fstat(fd)

    def test_script_bootstrap_uses_wrapper_location_and_sanitized_exit(self):
        wrapper = Path(entry.__file__).resolve()
        namespace = {"__name__": "__main__", "__package__": None, "__file__": str(wrapper)}
        error = io.StringIO()
        with mock.patch.object(sys, "path", []), mock.patch.object(sys, "argv", [str(wrapper)]), \
                mock.patch.object(sys, "stderr", error):
            with self.assertRaises(SystemExit) as caught:
                exec(compile(wrapper.read_bytes(), str(wrapper), "exec"), namespace)
            self.assertEqual(sys.path, [str(wrapper.parent.parent)])
        self.assertEqual(caught.exception.code, 1)
        self.assertEqual(error.getvalue(), "sglang_startup_failed: invalid_descriptor\n")

    def test_production_fd_reader_rejects_oversized_file_and_closes_it(self):
        with tempfile.TemporaryFile() as stream:
            stream.truncate(65537)
            fd = os.dup(stream.fileno())
            with self.assertRaises(entry.LaunchError):
                entry._read_descriptor(fd)
            with self.assertRaises(OSError):
                os.fstat(fd)

    def test_main_sanitizes_preflight_and_descriptor_errors(self):
        error = io.StringIO()
        with mock.patch.object(entry, "verify_checkpoint", side_effect=RuntimeError(self.root)), \
                mock.patch.object(entry, "_import_and_launch", side_effect=AssertionError("launched")):
            status = entry.main(self.argv(), self.payloads().__getitem__, error)
        self.assertEqual(status, 1)
        self.assertEqual(error.getvalue(), "sglang_startup_failed: startup_error\n")
        error = io.StringIO()
        status = entry.main(["--private-secret"], self.payloads().__getitem__, error)
        self.assertEqual(status, 1)
        self.assertNotIn("private-secret", error.getvalue())


class StartupTests(LaunchFixture, unittest.TestCase):
    def setUp(self):
        super().setUp()
        fixture = checkpoint_fixtures.StorageTests()
        fixture.setUp()
        self.addCleanup(fixture.doCleanups)
        self.root = str(fixture.root)
        self.manifest = fixture.manifest()

    def test_missing_verified_source_contract_cannot_launch(self):
        error = io.StringIO()
        with mock.patch.object(entry, "verify_checkpoint", side_effect=lambda root: preflight._verify(root, self.manifest)), \
                mock.patch.object(entry, "_import_and_launch", side_effect=AssertionError("launched")):
            result = entry.main(self.argv(), self.payloads().__getitem__, error)
        self.assertEqual(result, 1)
        self.assertEqual(error.getvalue(), "sglang_startup_failed: pinned_source_contract_unavailable\n")

    def run_with_contract(self, prepare):
        events = []
        error = io.StringIO()

        def final_launch(spec, checkpoint, contract):
            events.append("launch")
            self.assertEqual(checkpoint._root, self.root)
            self.assertEqual(contract, "synthetic-contract")

        with mock.patch.object(entry, "verify_checkpoint", side_effect=lambda root: preflight._verify(root, self.manifest)), \
                mock.patch.object(entry, "revalidate_checkpoint", side_effect=lambda value: preflight._revalidate(value, self.manifest)), \
                mock.patch.object(entry, "_verified_native_contract", side_effect=prepare), \
                mock.patch.object(entry, "_import_and_launch", side_effect=final_launch):
            result = entry.main(self.argv(), self.payloads().__getitem__, error)
        return result, events, error.getvalue()

    def test_inode_replacement_between_preflight_and_final_revalidation_denies_launch(self):
        def prepare(spec, checkpoint):
            leaf = Path(self.root) / "config.json"
            old = leaf.read_bytes()
            leaf.rename(leaf.with_name("old-config"))
            leaf.write_bytes(old)
            return "synthetic-contract"

        result, events, error = self.run_with_contract(prepare)
        self.assertEqual(result, 1)
        self.assertEqual(events, [])
        self.assertEqual(error, "sglang_startup_failed: artifact_changed\n")

    def test_valid_revalidation_reaches_only_replaced_final_seam(self):
        result, events, error = self.run_with_contract(lambda spec, checkpoint: "synthetic-contract")
        self.assertEqual((result, events, error), (0, ["launch"], ""))

    def test_missing_memory_saver_evidence_denies_final_seam(self):
        def prepare(spec, checkpoint):
            raise entry.LaunchError("memory_saver_unavailable")

        result, events, error = self.run_with_contract(prepare)
        self.assertEqual((result, events), (1, []))
        self.assertEqual(error, "sglang_startup_failed: memory_saver_unavailable\n")


class HealthTests(unittest.TestCase):
    key = "private-" + "k" * 32

    def test_health_prefix_variants_require_exact_inference_key(self):
        for path in ("/health", "/health_generate", "/health/foo", "/health/",
                     "//health_generate", "/%68ealth_generate", "/%2568ealth_generate",
                     "/health_generate?x=1", "/x/../health_generate", "/./health_generate",
                     "/%2fhealth_generate", "/health%5fgenerate", "/healthcheck"):
            with self.subTest(path=path):
                self.assertFalse(entry.private_health_allowed(path, "", self.key))
                self.assertFalse(entry.private_health_allowed(path, "Bearer admin-key", self.key))
                self.assertTrue(entry.private_health_allowed(path, "Bearer " + self.key, self.key))
        for authorization in (None, "bearer " + self.key, "Bearer " + self.key + " ",
                              "Bearer \u00e9", "Bearer \n" + self.key):
            self.assertFalse(entry.private_health_allowed("/health_generate", authorization, self.key))
        self.assertFalse(entry.private_health_allowed("/health_generate", "Bearer ", ""))
        self.assertTrue(entry.private_health_allowed("/v1/chat/completions", "", self.key))

    def test_malformed_path_is_denied_even_with_credentials(self):
        for path in ("health", "/health%", "/health%xx", "/health\0", "/health\\x",
                     "/health#fragment", "/" + "x" * 8192, "/%ff", None):
            self.assertFalse(entry.private_health_allowed(path, "Bearer " + self.key, self.key))

    def test_middleware_gates_options_and_does_not_change_native_auth(self):
        async def exercise(path, method, headers):
            messages = []
            calls = []

            async def app(scope, receive, send):
                calls.append(scope)

            async def receive():
                raise AssertionError("body must not be read")

            async def send(message):
                messages.append(message)

            gate = entry.PrivateHealthMiddleware(app, self.key)
            scope = {"type": "http", "path": path, "method": method, "headers": headers}
            await gate(scope, receive, send)
            return calls, messages

        for method in ("GET", "POST", "OPTIONS"):
            calls, messages = finish_without_io(exercise("/health_generate", method, []))
            self.assertEqual(calls, [])
            self.assertEqual(messages[0]["status"], 401)
        calls, messages = finish_without_io(exercise("/v1/chat/completions", "POST", []))
        self.assertEqual(len(calls), 1)
        self.assertEqual(messages, [])
        auth = (b"authorization", ("Bearer " + self.key).encode())
        calls, messages = finish_without_io(exercise("/health_generate", "OPTIONS", [auth]))
        self.assertEqual(len(calls), 1)
        self.assertEqual(messages, [])
        calls, messages = finish_without_io(exercise("/health_generate", "GET", [auth, auth]))
        self.assertEqual(calls, [])
        self.assertEqual(messages[0]["status"], 401)


if __name__ == "__main__":
    unittest.main()
