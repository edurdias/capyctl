"""CPU-only startup boundary tests; no native runtime or qualification evidence."""

import builtins
import copy
import importlib
import io
import json
import os
from pathlib import Path
import sys
import subprocess
import tempfile
import unittest
from unittest import mock

from runtime import sglang_entry as entry
from runtime import sglang_native_composition as composition
from runtime import sglang_device as device
from runtime import sglang_server_args as server_args
from runtime import sglang_startup_guards as guards

PLACEMENT_DIGEST = "0123456789abcdef" * 4
PLACEMENT_UUID = "GPU-12345678-1234-1234-1234-123456789abc"


def finish_without_io(coroutine):
    """Drive in-memory ASGI coroutines without an event loop or socketpair."""
    try:
        coroutine.send(None)
    except StopIteration as done:
        return done.value
    finally:
        coroutine.close()
    raise AssertionError("unexpected I/O suspension")


def settings():
    """ADR 0014 §2: a deep-parking deployment stating only its memory."""
    return {
        "dtype": None, "quantization": None, "kv_cache_dtype": None,
        "context_length": None, "max_running_requests": None, "cuda_graphs": False,
        "language_model_only": False, "trust_remote_code": False,
        "max_total_tokens": None, "chunked_prefill_size": None,
        "tokenizer_workers": 1, "memory_saver": True, "cpu_weight_backup": False,
        "weight_restore": "disk_reload",
        "memory": {"request_bytes": 16 << 30, "kv_cache_bytes": 4 << 30,
                   "margin_bytes": 8 << 30, "static_bytes": 8 << 30},
        "extra_args": [],
    }


def public_settings():
    return {
        "schema_version": 2, "kind": "sglang_launch", "engine": "sglang",
        "binding_id": "01K00000000000000000000001",
        "incarnation": "01K00000000000000000000099",
        "endpoint": "http://127.0.0.1:20001",
        "served_name": "toy",
        "rendered_settings_digest": "a" * 64,
        "device": {"host_id": "host-a", "hardware_fingerprint": "hardware-v1",
                   "device_id": "gpu0", "memory_domain": "uma"},
        "settings": settings(),
    }


class ScriptIdentityTests(unittest.TestCase):
    # T22: production executes the wrapper as a script, unlike imported fixtures.
    def test_script_and_helpers_share_the_launchspec_type(self):
        wrapper = str(Path(entry.__file__).resolve())
        script = """
import runpy, sys
namespace = runpy.run_path(sys.argv[1], run_name="__mp_main__")
from runtime import sglang_server_args
assert namespace["LaunchSpec"] is sglang_server_args.sglang_entry.LaunchSpec
"""
        result = subprocess.run([sys.executable, "-IS", "-c", script, wrapper],
                                capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)


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
                              "kind": "sglang_private_launch",
                              "checkpoint_root": self.root if root is None else root,
                              "public_settings": self.public if public is None else public}).encode(),
                4: self.inference, 5: self.admin}

    def scoped_payloads(self, digest=None):
        data = self.payloads()
        private = json.loads(data[3])
        private["schema_version"] = 2
        private["launch_scope"] = {
            "session_id": "01K00000000000000000000002",
            "deployment_id": "01K00000000000000000000003",
            "operation_id": "01K00000000000000000000004",
            "step_id": "01K00000000000000000000005",
            "binding_id": self.public["binding_id"],
            "incarnation": self.public["incarnation"],
            "revision": 1, "generation": 2,
            "issued_at_ms": 1000, "deadline_ms": 10000,
        }
        if digest is not None:
            private["placement_digest"] = digest
        data[3] = json.dumps(private).encode()
        return data

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
    def test_scoped_v2_descriptor_is_private_immutable_and_not_launch_authority(self):
        data = self.scoped_payloads()
        spec = self.build(payloads=data)
        expected = json.loads(data[3])["launch_scope"]
        self.assertEqual(json.loads(spec._launch_scope_json), expected)
        self.assertNotIn(expected["session_id"], repr(spec))
        self.assertNotIn("launch_scope", self.argv()[1])
        with self.assertRaises((AttributeError, TypeError)):
            spec._launch_scope_json = "{}"
        # The scope is private immutable metadata, not launch authority: the
        # boundary still refuses through the gates' own closed categories and
        # never reaches the audited argument mapper without a held contract.
        with mock.patch.object(composition, "enforce_closed_plugins", side_effect=lambda: None):
            contract = entry._verified_native_contract(spec)
        self.assertFalse(contract.placement_asserted)
        with mock.patch.object(entry, "_guarded_engine_import", side_effect=lambda: (object(), object())), \
                mock.patch.object(entry, "_require_capabilities"), \
                mock.patch.object(server_args, "construct_server_args",
                                  side_effect=AssertionError("constructed")):
            with self.assertRaises((AttributeError, TypeError)):
                entry._import_and_launch(spec, None)
        self.assertIsNone(self.build()._launch_scope_json)

    def test_placement_digest_is_an_optional_exact_v2_private_field(self):
        # Present and well-formed: the digest binds the descriptor to the
        # service-authorized inventory claim and is carried, never re-derived.
        spec = self.build(payloads=self.scoped_payloads(digest=PLACEMENT_DIGEST))
        self.assertEqual(spec._placement_digest, PLACEMENT_DIGEST)
        self.assertNotIn(PLACEMENT_DIGEST, repr(spec))
        # Absent: placement stays explicitly unasserted, never guessed.
        self.assertIsNone(self.build(payloads=self.scoped_payloads())._placement_digest)
        self.assertIsNone(self.build()._placement_digest)
        # Malformed digests refuse by name; nothing is repaired or inferred.
        for digest in ("", "z" * 64, PLACEMENT_DIGEST.upper(), PLACEMENT_DIGEST[:-1],
                       PLACEMENT_DIGEST + "0", 0, True, None, 64):
            with self.subTest(digest=digest):
                data = self.scoped_payloads()
                private = json.loads(data[3])
                private["placement_digest"] = digest
                data[3] = json.dumps(private).encode()
                self.rejects(payloads=data)
        # Version 1 never gains the field, and a digest cannot replace the
        # scope: both refuse as unknown shapes.
        data = self.payloads()
        private = json.loads(data[3])
        private["placement_digest"] = PLACEMENT_DIGEST
        data[3] = json.dumps(private).encode()
        self.rejects(payloads=data)
        data = self.scoped_payloads()
        private = json.loads(data[3])
        del private["launch_scope"]
        data[3] = json.dumps(private).encode()
        self.rejects(payloads=data)

    def test_v2_scope_rejects_wrong_types_bounds_identity_and_unknown_fields(self):
        original = json.loads(self.scoped_payloads()[3])
        mutations = [("unexpected", 1), ("binding_id", original["launch_scope"]["step_id"]),
                     ("incarnation", original["launch_scope"]["step_id"])]
        for name in ("session_id", "deployment_id", "operation_id", "step_id",
                     "binding_id", "incarnation"):
            mutations.extend((name, value) for value in (None, True, "bad", "x" * 27))
        for name in ("revision", "generation"):
            mutations.extend((name, value) for value in (0, -1, True, 1.0, "1", 2 ** 63))
        for name in ("issued_at_ms", "deadline_ms"):
            mutations.extend((name, value) for value in (-1, True, 1.0, "1", 2 ** 63))
        mutations.extend((("deadline_ms", 1000), ("deadline_ms", 999)))
        for key, value in mutations:
            with self.subTest(key=key, value=value):
                private = copy.deepcopy(original)
                private["launch_scope"][key] = value
                data = self.payloads()
                data[3] = json.dumps(private).encode()
                self.rejects(payloads=data)
        for missing in original["launch_scope"]:
            private = copy.deepcopy(original)
            del private["launch_scope"][missing]
            data = self.payloads()
            data[3] = json.dumps(private).encode()
            self.rejects(payloads=data)

    def test_descriptor_versions_never_infer_or_discard_scope(self):
        for version in (1, 3, True, 2.0, "2"):
            data = self.scoped_payloads()
            private = json.loads(data[3])
            private["schema_version"] = version
            data[3] = json.dumps(private).encode()
            self.rejects(payloads=data)
        data = self.payloads()
        private = json.loads(data[3])
        private["schema_version"] = 2
        data[3] = json.dumps(private).encode()
        self.rejects(payloads=data)
        data = self.scoped_payloads()
        data[3] = data[3].replace(b'"generation": 2', b'"generation": 2, "generation": 2')
        self.rejects(payloads=data)

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
        for target in ("private", "public", "settings", "memory"):
            public = copy.deepcopy(self.public)
            private = json.loads(self.payloads()[3])
            if target == "private":
                private["extra"] = "forbidden"
            elif target == "public":
                public["extra"] = "forbidden"
            elif target == "settings":
                public["settings"]["extra"] = True
            else:
                public["settings"]["memory"]["extra"] = 1
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

    def test_device_selection_is_closed_and_bound_to_private_descriptor(self):
        for key in self.public["device"]:
            for value in ("", None, True, 0, "gpu 0", "../gpu0", "x" * 257):
                public = copy.deepcopy(self.public)
                public["device"][key] = value
                with self.subTest(key=key, value=value):
                    self.rejects(self.argv(public), self.payloads(public))
        for device in ({}, {**self.public["device"], "cuda_index": 0}):
            public = copy.deepcopy(self.public)
            public["device"] = device
            self.rejects(self.argv(public), self.payloads(public))
        public = copy.deepcopy(self.public)
        public["device"]["device_id"] = "gpu7"
        self.rejects(self.argv(public), self.payloads())
        spec = self.build(self.argv(public), self.payloads(public))
        self.assertEqual(json.loads(spec._public_json)["device"]["device_id"], "gpu7")

    def test_served_name_is_the_route_token_not_the_binding_derivation(self):
        # The served name is the deployment's route name: ordinary ASCII
        # tokens are accepted unchanged, and the retired
        # `candidate-{binding_id}` derivation is neither required nor special.
        for name in ("toy", "a-route", "route.1_v2", "x" * 256):
            public = copy.deepcopy(self.public)
            public["served_name"] = name
            spec = self.build(self.argv(public), self.payloads(public))
            self.assertEqual(json.loads(spec._public_json)["served_name"], name)

    def test_closed_public_types_and_bounds(self):
        # T14: the public descriptor is closed; the pinned recipe, checkpoint
        # revision and Qwen3-4B KV bound are gone (ADR 0014 §9), and so is the
        # source revision token (ADR 0008): any of them is an unknown key.
        mutations = [
            ("source_revision", "94602c9c2b7cbdb8efd5c52802dac6a1c180089e"),
            ("source_revision", "main"),
            ("schema_version", 1), ("schema_version", True), ("schema_version", 2.0),
            ("engine", "vllm"),
            ("binding_id", "ordinary"), ("incarnation", ""),
            # The retired kinds are invalid now, not deprecated aliases.
            ("kind", "sglang_candidate_launch"), ("kind", "sglang_candidate_private_launch"),
            ("endpoint", "http://0.0.0.0:20001"),
            ("endpoint", "http://127.0.0.1:020001"),
            ("endpoint", "http://127.0.0.1:65536"),
            ("served_name", ""), ("served_name", "has space"),
            ("served_name", "tab\tname"), ("served_name", "x" * 257),
            ("served_name", "caf\u00e9-route"), ("served_name", "route\u202ename"),
            ("rendered_settings_digest", "z" * 64),
        ]
        for key, value in mutations:
            with self.subTest(key=key, value=value):
                public = copy.deepcopy(self.public)
                public[key] = value
                self.rejects(self.argv(public), self.payloads(public))
        for retired in ("recipe", "checkpoint_revision", "minimum_kv_bytes",
                        "static_memory_fraction"):
            public = copy.deepcopy(self.public)
            public[retired] = "anything"
            with self.subTest(retired=retired):
                self.rejects(self.argv(public), self.payloads(public))

    def test_typed_settings_accept_any_model_values_and_refuse_bad_shapes(self):
        # E1 / ADR 0011: values pass as the engine spells them; only type,
        # range and closure are checked here.
        accepted = {"dtype": "float16", "quantization": "modelopt_fp4",
                    "kv_cache_dtype": "fp8_e4m3", "context_length": 32768,
                    "max_running_requests": 16, "cuda_graphs": True,
                    "language_model_only": True, "trust_remote_code": True,
                    "max_total_tokens": 65536, "chunked_prefill_size": -1,
                    "tokenizer_workers": 4,
                    "extra_args": ["--reasoning-parser", "qwen3"]}
        public = copy.deepcopy(self.public)
        public["settings"].update(accepted)
        self.build(self.argv(public), self.payloads(public))
        refused = [("dtype", "int8"), ("dtype", 1), ("quantization", "fp 8"),
                   ("kv_cache_dtype", ""), ("context_length", 0),
                   ("context_length", True), ("context_length", 2 ** 31),
                   ("max_running_requests", -1), ("max_total_tokens", 1.0),
                   ("chunked_prefill_size", 0), ("chunked_prefill_size", -2),
                   ("cuda_graphs", 0), ("language_model_only", None),
                   ("trust_remote_code", "false"), ("tokenizer_workers", 0),
                   ("memory_saver", 1), ("cpu_weight_backup", None),
                   ("weight_restore", "cpu_backup"), ("extra_args", "--x"),
                   ("extra_args", [1]), ("extra_args", ["a\nb"]),
                   ("extra_args", ["--x"] * 257)]
        for key, value in refused:
            with self.subTest(key=key, value=value):
                public = copy.deepcopy(self.public)
                public["settings"][key] = value
                self.rejects(self.argv(public), self.payloads(public))
        public = copy.deepcopy(self.public)
        public["settings"].update(cpu_weight_backup=True, weight_restore="cpu_backup")
        self.build(self.argv(public), self.payloads(public))

    def test_memory_request_is_closed_and_static_share_is_request_minus_margin(self):
        for key, value in (("request_bytes", 0), ("request_bytes", True),
                           ("kv_cache_bytes", 0), ("margin_bytes", -1),
                           ("static_bytes", (16 << 30) - (8 << 30) + 1),
                           ("kv_cache_bytes", 17 << 30),
                           ("request_bytes", 2 ** 63)):
            with self.subTest(key=key, value=value):
                public = copy.deepcopy(self.public)
                public["settings"]["memory"][key] = value
                self.rejects(self.argv(public), self.payloads(public))

        # An explicit request below KV plus margin: the static pool is the KV.
        public = copy.deepcopy(self.public)
        public["settings"]["memory"].update(request_bytes=8 << 30, static_bytes=4 << 30)
        self.build(self.argv(public), self.payloads(public))

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
            # SPEC §9.1 / T21: the wrapper's parent is never an import root;
            # the package resolves from the runtime directory itself
            # (test_runtime_imports.py runs the real bootstrap under -IS).
            self.assertEqual(sys.path, [])
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
        with mock.patch.object(entry, "_verified_native_contract", side_effect=RuntimeError(self.root)), \
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
        # _import_and_launch pins the process environment (loopback_rendezvous).
        environ = mock.patch.dict(os.environ)
        environ.start()
        self.addCleanup(environ.stop)

    def _launch_recording_environment(self, drift=None):
        """Run main to the launch seam; return (result, stderr, env at launch)."""
        seen = {}
        launch = mock.Mock()
        launch.launch_server.side_effect = lambda *args, **kwargs: seen.update(os.environ)

        def construct(spec, placement, constructor, **_):
            if drift:
                drift()
            return mock.Mock()

        error = io.StringIO()
        with mock.patch.object(entry, "_verified_native_contract", return_value=mock.Mock()), \
                mock.patch.object(entry, "_guarded_engine_import",
                                  side_effect=lambda: (mock.Mock(), launch)), \
                mock.patch.object(entry, "_require_capabilities"), \
                mock.patch.object(entry, "_observation_target", return_value=None), \
                mock.patch.object(server_args, "construct_server_args", side_effect=construct):
            result = entry.main(self.argv(), self.payloads().__getitem__, error)
        return result, error.getvalue(), seen

    # T21: SPEC §8.2, found live 2026-09-23 (M08). SGLang 0.5.20's default
    # rendezvous is a torch TCPStore, which listens on every interface; the
    # engine now starts with a private file store and loopback transports.
    def test_engine_starts_with_a_private_file_rendezvous(self):
        os.environ["MASTER_ADDR"] = "0.0.0.0"
        os.environ["GLOO_SOCKET_IFNAME"] = "eth0"
        result, error, seen = self._launch_recording_environment()
        self.assertEqual((result, error), (0, ""))
        method = seen["SGLANG_DISTRIBUTED_INIT_METHOD_OVERRIDE"]
        self.assertTrue(method.startswith("file:///"), method)
        directory = os.path.dirname(method[len("file://"):])
        self.assertEqual(os.stat(directory).st_mode & 0o777, 0o700)
        self.assertEqual((seen["GLOO_SOCKET_IFNAME"], seen["NCCL_SOCKET_IFNAME"]), ("lo", "lo"))
        self.assertNotIn("MASTER_ADDR", seen)
        os.rmdir(directory)

    # T21: a rendezvous input changed after pinning never reaches the engine.
    def test_rendezvous_drift_before_launch_is_refused(self):
        def drift():
            os.environ["SGLANG_DISTRIBUTED_INIT_METHOD_OVERRIDE"] = "tcp://0.0.0.0:29500"

        result, error, seen = self._launch_recording_environment(drift)
        self.assertEqual(result, 1)
        self.assertEqual(error, "sglang_startup_failed: loopback_rendezvous_failed\n")
        self.assertEqual(seen, {})

    def _green_gate_patches(self, events, roots):
        # ADR 0008: no source audit gate; the capability probe runs after
        # the guarded import (its own tests use synthetic package trees).
        def plugins():
            events.append("plugins")

        def capabilities(spec, arguments, launch):
            events.append("capabilities")

        return (mock.patch.object(composition, "enforce_closed_plugins", side_effect=plugins),
                mock.patch.object(entry, "_require_capabilities", side_effect=capabilities))

    def test_held_contract_reaches_the_guarded_import_boundary_and_launches(self):
        events = []
        roots = []
        error = io.StringIO()
        arguments = mock.Mock()
        checked = mock.Mock()
        native = checked._native
        launch_calls = []

        def guarded_import():
            events.append("import")
            launch = mock.Mock()
            launch_calls.append(launch)
            return arguments, launch

        placements = []

        def recorded_construct(spec, placement, constructor, **_):
            placements.append(placement)
            return checked

        patches = (*self._green_gate_patches(events, roots),
                   mock.patch.object(entry, "_guarded_engine_import", side_effect=guarded_import),
                   mock.patch.object(server_args, "construct_server_args",
                                     side_effect=recorded_construct))
        for patch in patches:
            patch.start()
        try:
            result = entry.main(self.argv(), self.payloads().__getitem__, error)
        finally:
            for patch in reversed(patches):
                patch.stop()
        self.assertEqual(result, 0)
        self.assertEqual(error.getvalue(), "")
        # The gates ran in composition's fixed order before the boundary, the
        # capabilities were probed after the guarded import, and the
        # unasserted placement is carried as None, never faked.
        self.assertEqual(events, ["plugins", "import", "capabilities"])
        self.assertEqual(placements, [None])
        launch_calls[0].launch_server.assert_called_once_with(native)

    def test_unasserted_placement_fails_closed_before_engine_start(self):
        # The descriptor carries no authorized placement digest, so the
        # contract records placement_asserted=False and the audited argument
        # mapper refuses it before any construction; the engine start call is
        # never reached.
        events = []
        error = io.StringIO()
        launch = mock.Mock()
        with mock.patch.object(composition, "enforce_closed_plugins", side_effect=lambda: None), \
                mock.patch.object(entry, "_require_capabilities"), \
                mock.patch.object(entry, "_guarded_engine_import",
                                  side_effect=lambda: (events.append("import"), (mock.Mock(), launch))[1]):
            result = entry.main(self.argv(), self.payloads().__getitem__, error)
        self.assertEqual(result, 1)
        self.assertEqual(events, ["import"])
        self.assertEqual(error.getvalue(), "sglang_startup_failed: placement_mismatch\n")
        launch.assert_not_called()

    def _asserted_placement_patches(self, events, inventory_digest, uuid=PLACEMENT_UUID):
        # Green gates plus a synthetic device inventory: the real placement
        # observation runs, but collect_inventory (its only fresh-evidence
        # seam) is substituted. This is a CPU fixture, never qualification.
        def inventory():
            return device.DeviceInventory(
                host_id="host-a", architecture="x86_64", boot_id="boot",
                devices=(device.PhysicalDevice(
                    PLACEMENT_UUID, "0000:09:00.0", 0, "0x10de", "0x2684"),),
                digest=inventory_digest, observed_at_ns=1)

        return (*self._green_gate_patches(events, []),
                mock.patch.object(device, "collect_inventory", side_effect=inventory),
                mock.patch.dict(os.environ, {"CUDA_VISIBLE_DEVICES": uuid}))

    def test_asserted_placement_reaches_the_guarded_boundary_and_launches(self):
        # The descriptor carries the service-authorized inventory digest; the
        # entry assembles the mapping from the descriptor's device selectors,
        # that digest, and the inherited namespace, and compose asserts
        # placement against freshly collected inventory. The observed
        # placement — never a guessed one — reaches the audited mapper.
        events = []
        error = io.StringIO()
        arguments = mock.Mock()
        checked = mock.Mock()
        native = checked._native
        launch_calls = []
        placements = []

        def guarded_import():
            events.append("import")
            launch = mock.Mock()
            launch_calls.append(launch)
            return arguments, launch

        def recorded_construct(spec, placement, constructor, **_):
            placements.append(placement)
            return checked

        patches = (*self._asserted_placement_patches(events, PLACEMENT_DIGEST),
                   mock.patch.object(entry, "_guarded_engine_import", side_effect=guarded_import),
                   mock.patch.object(server_args, "construct_server_args",
                                     side_effect=recorded_construct))
        for patch in patches:
            patch.start()
        try:
            result = entry.main(self.argv(), self.scoped_payloads(digest=PLACEMENT_DIGEST).__getitem__, error)
        finally:
            for patch in reversed(patches):
                patch.stop()
        self.assertEqual(result, 0)
        self.assertEqual(error.getvalue(), "")
        self.assertEqual(events, ["plugins", "import", "capabilities"])
        self.assertEqual(len(placements), 1)
        placement = placements[0]
        self.assertEqual(placement.physical_gpu_uuid, PLACEMENT_UUID)
        self.assertEqual(placement.host_id, "host-a")
        self.assertEqual(placement.device_id, "gpu0")
        self.assertEqual(placement.memory_domain, "uma")
        self.assertEqual(placement.cuda_visible_uuids, (PLACEMENT_UUID,))
        self.assertEqual(placement.cuda_index, 0)
        launch_calls[0].launch_server.assert_called_once_with(native)

    def test_live_mapping_mismatch_fails_closed_through_the_placement_gate(self):
        # The host's real inventory digest differs from the descriptor's
        # authorized claim: observe_placement refuses, and the launch boundary
        # is never reached.
        for uuid in (PLACEMENT_UUID, "GPU-99999999-9999-9999-9999-999999999999"):
            with self.subTest(uuid=uuid):
                events = []
                error = io.StringIO()
                patches = (*self._asserted_placement_patches(events, "f" * 64, uuid=uuid),
                           mock.patch.object(entry, "_import_and_launch",
                                             side_effect=AssertionError("launched")))
                for patch in patches:
                    patch.start()
                try:
                    result = entry.main(self.argv(),
                                        self.scoped_payloads(digest=PLACEMENT_DIGEST).__getitem__, error)
                finally:
                    for patch in reversed(patches):
                        patch.stop()
                self.assertEqual(result, 1)
                # The earlier gates ran green; the placement gate is what
                # refused, before any engine import.
                self.assertEqual(events, ["plugins"])
                self.assertEqual(error.getvalue(),
                                 "sglang_startup_failed: placement_failed\n")

    def test_digest_without_an_inherited_namespace_fails_closed(self):
        # The digest alone is not placement: without the guarded inherited
        # CUDA namespace the mapping cannot be assembled honestly, and the
        # placement gate refuses rather than guessing a device.
        events = []
        error = io.StringIO()
        patches = (*self._green_gate_patches(events, []),
                   mock.patch.dict(os.environ, {}, clear=True),
                   mock.patch.object(entry, "_import_and_launch",
                                     side_effect=AssertionError("launched")))
        for patch in patches:
            patch.start()
        try:
            result = entry.main(self.argv(), self.scoped_payloads(digest=PLACEMENT_DIGEST).__getitem__, error)
        finally:
            for patch in reversed(patches):
                patch.stop()
        self.assertEqual(result, 1)
        self.assertEqual(events, ["plugins"])
        self.assertEqual(error.getvalue(), "sglang_startup_failed: placement_failed\n")

    def test_mapping_assembly_failure_folds_into_the_placement_category(self):
        # A mapping the entry cannot honestly assemble is a placement-gate
        # refusal, never the blanket startup category: the assembly runs
        # inside composition's closed-category fold. Today's descriptor
        # boundary makes the malformed shape unreachable, so the assembly is
        # substituted at its seam (the entry's own function).
        events = []
        error = io.StringIO()
        patches = (*self._green_gate_patches(events, []),
                   mock.patch.object(entry, "_placement_mapping",
                                     side_effect=KeyError("device")),
                   mock.patch.object(entry, "_import_and_launch",
                                     side_effect=AssertionError("launched")))
        for patch in patches:
            patch.start()
        try:
            result = entry.main(self.argv(), self.scoped_payloads(digest=PLACEMENT_DIGEST).__getitem__, error)
        finally:
            for patch in reversed(patches):
                patch.stop()
        self.assertEqual(result, 1)
        # The assembly is refused before any gate runs; the category is the
        # placement gate's own, not the blanket startup one.
        self.assertEqual(events, [])
        self.assertEqual(error.getvalue(), "sglang_startup_failed: placement_failed\n")

    def test_each_gate_failure_surfaces_its_own_closed_category_from_main(self):
        cases = (
            ("plugin_closure_failed",
             {"enforce_closed_plugins": mock.Mock(
                  side_effect=guards.StartupGuardError("external_plugins_present"))}),
        )
        for expected, overrides in cases:
            with self.subTest(expected=expected):
                error = io.StringIO()
                patches = [mock.patch.object(composition, name, value)
                           for name, value in overrides.items()]
                patches.append(mock.patch.object(entry, "_import_and_launch",
                                                 side_effect=AssertionError("launched")))
                for patch in patches:
                    patch.start()
                try:
                    result = entry.main(self.argv(), self.payloads().__getitem__, error)
                finally:
                    for patch in reversed(patches):
                        patch.stop()
                self.assertEqual((result, error.getvalue()),
                                 (1, "sglang_startup_failed: " + expected + "\n"))

    def test_composition_denial_category_renders_from_main(self):
        # compose's own categories pass through the conversion verbatim; the
        # placement category is unreachable from today's invocation (the
        # descriptor carries no digest), so it is exercised at the seam.
        error = io.StringIO()
        with mock.patch.object(composition, "compose",
                               side_effect=composition.NativeCompositionError("placement_failed")), \
                mock.patch.object(entry, "_import_and_launch",
                                  side_effect=AssertionError("launched")):
            result = entry.main(self.argv(), self.payloads().__getitem__, error)
        self.assertEqual(result, 1)
        self.assertEqual(error.getvalue(), "sglang_startup_failed: placement_failed\n")

    def test_failed_engine_import_denies_through_startup_error(self):
        error = io.StringIO()
        with mock.patch.object(composition, "enforce_closed_plugins", side_effect=lambda: None), \
                mock.patch.object(entry, "_guarded_engine_import", side_effect=ImportError("sglang")):
            result = entry.main(self.argv(), self.payloads().__getitem__, error)
        self.assertEqual(result, 1)
        self.assertEqual(error.getvalue(), "sglang_startup_failed: startup_error\n")

    def run_with_contract(self, prepare):
        events = []
        error = io.StringIO()

        def final_launch(spec, contract, *_):
            events.append("launch")
            self.assertEqual(contract, "synthetic-contract")

        with mock.patch.object(entry, "_verified_native_contract", side_effect=prepare), \
                mock.patch.object(entry, "_import_and_launch", side_effect=final_launch):
            result = entry.main(self.argv(), self.payloads().__getitem__, error)
        return result, events, error.getvalue()

    def test_valid_contract_reaches_only_replaced_final_seam(self):
        result, events, error = self.run_with_contract(lambda spec: "synthetic-contract")
        self.assertEqual((result, events, error), (0, ["launch"], ""))

    def test_missing_memory_saver_evidence_denies_final_seam(self):
        def prepare(spec):
            raise entry.LaunchError("memory_saver_unavailable")

        result, events, error = self.run_with_contract(prepare)
        self.assertEqual((result, events), (1, []))
        self.assertEqual(error, "sglang_startup_failed: memory_saver_unavailable\n")

    def test_mapper_refusal_surfaces_its_closed_category(self):
        # T14 T22: a reserved extra argument or a resolved-value drift reaches
        # the operator as the mapper's own category, never a blanket error.
        error = io.StringIO()
        contract = mock.Mock()
        with mock.patch.object(entry, "_verified_native_contract", return_value=contract), \
                mock.patch.object(entry, "_guarded_engine_import",
                                  side_effect=lambda: (mock.Mock(), mock.Mock())), \
                mock.patch.object(entry, "_require_capabilities"), \
                mock.patch.object(server_args, "construct_server_args",
                                  side_effect=server_args.ServerArgsError("effective_args_mismatch")):
            result = entry.main(self.argv(), self.payloads().__getitem__, error)
        self.assertEqual(result, 1)
        self.assertEqual(error.getvalue(), "sglang_startup_failed: effective_args_mismatch\n")


class PreimportGuardTests(unittest.TestCase):
    def test_preimport_guard_runs_containment_then_plugin_closure_in_order(self):
        events = []
        with mock.patch.object(guards, "contain_startup_output",
                               side_effect=lambda: events.append("containment")), \
                mock.patch.object(guards, "enforce_closed_plugins",
                                  side_effect=lambda: events.append("plugins")):
            guards.preimport_guard()
        self.assertEqual(events, ["containment", "plugins"])

    def test_guard_fails_closed_on_nonempty_plugin_selection(self):
        with mock.patch.object(guards, "contain_startup_output"), \
                mock.patch.dict(os.environ, {"SGLANG_PLUGINS": "PRIVATE-SELECTION"}):
            with self.assertRaises(guards.StartupGuardError) as caught:
                guards.preimport_guard()
        self.assertEqual(caught.exception.code, "external_plugin_selection")
        self.assertNotIn("PRIVATE-SELECTION", str(caught.exception))
        self.assertNotIn("PRIVATE-SELECTION", repr(caught.exception))
        for platform in ("SGLANG_PLATFORM",):
            with mock.patch.object(guards, "contain_startup_output"), \
                    mock.patch.dict(os.environ, {platform: "PRIVATE-SELECTION"}):
                with self.assertRaises(guards.StartupGuardError) as caught:
                    guards.preimport_guard()
            self.assertEqual(caught.exception.code, "external_plugin_selection")

    def spawned_namespace(self):
        return {"__name__": "__mp_main__", "__package__": None,
                "__file__": str(Path(entry.__file__).resolve())}

    def test_spawned_interpreter_runs_the_guard_during_spawn_preparation(self):
        # CPython spawn executes the main script as __mp_main__ before unpickling
        # the Process and its native argument classes. The exec below runs the
        # entry's real module body in that role; the source-position assertion
        # documents that the guard call site precedes the first post-guard
        # import, so no module-body effect can precede the guard either.
        wrapper = Path(entry.__file__).resolve()
        text = wrapper.read_text()
        self.assertLess(text.index('if __name__ == "__mp_main__":'),
                        text.index("from runtime.sglang_launch_spec import"))
        with mock.patch.object(guards, "preimport_guard") as guard:
            namespace = self.spawned_namespace()
            with mock.patch.object(sys, "path", []):
                exec(compile(wrapper.read_bytes(), str(wrapper), "exec"), namespace)
        guard.assert_called_once_with()
        self.assertIn("LaunchSpec", namespace)

    def test_spawned_interpreter_fails_closed_before_any_module_body_effect(self):
        wrapper = Path(entry.__file__).resolve()
        with mock.patch.object(guards, "preimport_guard",
                               side_effect=guards.StartupGuardError("external_plugin_selection")):
            namespace = self.spawned_namespace()
            with mock.patch.object(sys, "path", []):
                with self.assertRaises(SystemExit) as caught:
                    exec(compile(wrapper.read_bytes(), str(wrapper), "exec"), namespace)
        self.assertEqual(caught.exception.code, 1)
        # The module body aborted at the guard: nothing after it was defined,
        # and in real spawn no Process argument would have been unpickled.
        self.assertNotIn("LaunchSpec", namespace)


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
