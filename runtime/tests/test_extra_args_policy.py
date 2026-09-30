"""Launch-time gate on deployment extra arguments (ADR 0014 §8, SPEC §8.2).

The deploy-time check sees only the spelling a deployment wrote. The engines'
parsers expand abbreviations, aliases and `=value` forms, so the launch gate
decides on the destination the installed parser resolved: a sensitive one
needs the host's named approval, carried to the entry in CAPYCTL_EXTRA_APPROVALS.
CPU-only; never evidence a build serves.
"""

import json
import unittest

from runtime import extra_args_policy as policy


def approvals(options=(), paths=(), trust_remote_code=False):
    return policy.parse_approvals(json.dumps(
        {"options": list(options), "paths": list(paths),
         "trust_remote_code": trust_remote_code}))


class ClassifyTests(unittest.TestCase):
    # T21 T22
    def test_sensitive_destination_shapes(self):
        for engine, dest in (("sglang", "decoupled_spec_bind_endpoint"),
                             ("sglang", "engine_info_bootstrap_port"),
                             ("sglang", "decrypted_config_file"),
                             ("sglang", "debug_tensor_dump_output_folder"),
                             ("sglang", "decoupled_spec_connect_endpoints"),
                             ("sglang", "remote_instance_weight_loader_seed_instance_ip"),
                             ("vllm", "master_addr"), ("vllm", "compilation_config"),
                             ("vllm", "numa_bind"), ("vllm", "worker_extension_cls"),
                             ("vllm", "hf_token"), ("vllm", "load_format")):
            with self.subTest(dest=dest):
                self.assertIsNotNone(policy.classify(engine, dest))
        for engine, dest in (("vllm", "reasoning_parser"), ("vllm", "dtype"),
                             ("sglang", "schedule_policy"), ("sglang", "reasoning_parser")):
            with self.subTest(dest=dest):
                self.assertIsNone(policy.classify(engine, dest))


class CheckTests(unittest.TestCase):
    # T21: unapproved sensitive destinations are refused whatever the spelling
    # resolved from; the refusal names no value.
    def test_unapproved_sensitive_destination_is_refused(self):
        for engine, dest, value in (("vllm", "master_addr", "10.0.0.1"),
                                    ("sglang", "engine_info_bootstrap_port", 29500),
                                    ("sglang", "decoupled_spec_bind_endpoint", "tcp://0.0.0.0:1"),
                                    ("vllm", "compilation_config", {"level": 3})):
            with self.subTest(dest=dest):
                with self.assertRaises(policy.Refused) as refused:
                    policy.check(engine, {dest: value}, approvals(), "/srv/models/m")
                self.assertNotIn(str(value), str(refused.exception))

    # T21: named approval admits code, listener and configuration options.
    def test_named_approval_admits(self):
        policy.check("vllm", {"compilation_config": {"level": 3}},
                     approvals(["--compilation-config"]), "/srv/models/m")
        policy.check("vllm", {"worker_extension_cls": "pkg.Ext"},
                     approvals(["--worker-extension-cls"]), "/srv/models/m")
        policy.check("vllm", {"reasoning_parser": "qwen3"}, approvals(), "/srv/models/m")

    # T21: an approved path option must name a path inside an approved
    # directory; a checkpoint-exempt one may name one inside the checkpoint.
    def test_path_values_must_lie_inside_approved_directories(self):
        with self.assertRaises(policy.Refused):
            policy.check("vllm", {"download_dir": "/srv/cache/hf"},
                         approvals(["--download-dir"]), "/srv/models/m")
        policy.check("vllm", {"download_dir": "/srv/cache/hf"},
                     approvals(["--download-dir"], ["/srv/cache"]), "/srv/models/m")
        for escape in ("/srv/cache/../etc", "/srv/cachex", "relative", ["/srv/cache", "/etc"],
                       {"path": "/srv/cache"}):
            with self.subTest(escape=escape):
                with self.assertRaises(policy.Refused):
                    policy.check("vllm", {"download_dir": escape},
                                 approvals(["--download-dir"], ["/srv/cache"]), "/srv/models/m")
        policy.check("vllm", {"chat_template": "/srv/models/m/chat.jinja"}, approvals(),
                     "/srv/models/m")
        with self.assertRaises(policy.Refused):
            policy.check("vllm", {"chat_template": "/tmp/chat.jinja"}, approvals(),
                         "/srv/models/m")

    # T21: vLLM `--speculative-config` is an object: admitted only with named
    # approval, every key on the closed list and the draft model inside an
    # approved directory (found live 2026-09-25: as a plain path it never passed).
    def test_speculative_config_is_checked_key_by_key(self):
        ok = approvals(["--speculative-config"], ["/srv/models"])
        policy.check("vllm", {"speculative_config": {"method": "mtp", "num_speculative_tokens": 3}},
                     ok, "/srv/models/m")
        policy.check("vllm", {"speculative_config": {"method": "dflash", "model": "/srv/models/d",
                                                     "num_speculative_tokens": 7}}, ok, "/srv/models/m")
        policy.check("vllm", {"speculative_config": '{"model": "/srv/models/d"}'}, ok, "/srv/models/m")
        policy.check("vllm", {"speculative_config": {"method": "mtp", "moe_backend": "triton"}}, ok,
                     "/srv/models/m")
        for value in ({"method": "mtp"},):
            with self.assertRaises(policy.Refused):
                policy.check("vllm", {"speculative_config": value}, approvals(paths=["/srv/models"]),
                             "/srv/models/m")
        for value in ({"model": "/etc/d"}, {"model": "/srv/models/../etc"}, {"model": "org/repo"},
                      {"model": ["/srv/models/d"]}, {"method": "mtp", "tokenizer": "/srv/models/t"},
                      {"method": "mtp", "draft_model_config": {"x": 1}}, ["/srv/models/d"], "not json"):
            with self.subTest(value=value):
                with self.assertRaises(policy.Refused):
                    policy.check("vllm", {"speculative_config": value}, ok, "/srv/models/m")

    # T21: code-loading shapes (a class, a plugin, a loader) need approval
    # whatever their name, e.g. vLLM `--scheduler-cls`, `--io-processor-plugin`
    # and SGLang `--custom-weight-loader`.
    def test_code_loading_shapes_need_approval(self):
        for engine, dest in (("vllm", "scheduler_cls"), ("vllm", "io_processor_plugin"),
                             ("sglang", "custom_weight_loader"), ("vllm", "future_class")):
            with self.subTest(dest=dest):
                self.assertEqual(policy.classify(engine, dest), policy.CODE)
                with self.assertRaises(policy.Refused):
                    policy.check(engine, {dest: "pkg.Thing"}, approvals(), "/srv/models/m")

    # T21: containment is checked on the resolved path too, so a symlink
    # inside an approved directory cannot lead outside it.
    def test_a_symlink_cannot_escape_an_approved_directory(self):
        import os
        import tempfile
        with tempfile.TemporaryDirectory() as root:
            approved_dir = os.path.join(root, "cache")
            outside = os.path.join(root, "outside")
            os.mkdir(approved_dir)
            os.mkdir(outside)
            os.symlink(outside, os.path.join(approved_dir, "escape"))
            os.mkdir(os.path.join(approved_dir, "real"))
            allowed = approvals(["--download-dir"], [approved_dir])
            policy.check("vllm", {"download_dir": os.path.join(approved_dir, "real")},
                         allowed, "/srv/models/m")
            with self.assertRaises(policy.Refused):
                policy.check("vllm", {"download_dir": os.path.join(approved_dir, "escape")},
                             allowed, "/srv/models/m")
            with self.assertRaises(policy.Refused):
                policy.check("vllm", {"download_dir": os.path.join(approved_dir, "escape", "x")},
                             allowed, "/srv/models/m")

    # T21 T03: approvals arrive in one closed JSON document; anything else is
    # a closed failure, and none at all approves nothing.
    def test_approvals_are_strict(self):
        self.assertEqual(policy.parse_approvals(None), policy.Approvals((), (), False))
        for bad in ("{", "[]", '{"options": [], "paths": []}',
                    '{"options": ["x"], "paths": [], "trust_remote_code": false}',
                    '{"options": [], "paths": ["relative"], "trust_remote_code": false}',
                    '{"options": [], "paths": ["/a/../b"], "trust_remote_code": false}',
                    '{"options": [], "paths": [], "trust_remote_code": 1}'):
            with self.subTest(bad=bad):
                with self.assertRaises(policy.Refused):
                    policy.parse_approvals(bad)


if __name__ == "__main__":
    unittest.main()
