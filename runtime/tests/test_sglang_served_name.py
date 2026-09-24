"""CPU-only: the served name survives a disk weight reload (no native engine)."""

import sys
import types
import unittest
from unittest import mock

from runtime import sglang_entry as entry


def _fake_tokenizer_module():
    class TokenizerManager:
        def __init__(self):
            self.served_model_name = "route-name"
            self.model_path = "/models/m"

        # SGLang 0.5.20: a disk reload renames the served model to the path.
        def _update_model_path_info(self, model_path, load_format):
            self.served_model_name = model_path
            self.model_path = model_path

    module = types.ModuleType("sglang.srt.managers.tokenizer_manager")
    module.TokenizerManager = TokenizerManager
    return module


class ServedNameAcrossReloadTests(unittest.TestCase):
    # T16 T22: found live 2026-09-23 (M28, host-b). A deep wake reloads the
    # weights from disk; SGLang then served under the checkpoint path, so the
    # route's name vanished from /v1/models and the wake's fresh probe failed.
    def test_reload_keeps_the_served_name_and_updates_the_path(self):
        module = _fake_tokenizer_module()
        with mock.patch.dict(sys.modules, {"sglang.srt.managers.tokenizer_manager": module}):
            self.assertTrue(entry._keep_served_name_on_reload())
            manager = module.TokenizerManager()
            manager._update_model_path_info("/models/m", "auto")
        self.assertEqual(manager.served_model_name, "route-name")
        self.assertEqual(manager.model_path, "/models/m")

    def test_an_installation_without_the_rename_is_left_alone(self):
        module = types.ModuleType("sglang.srt.managers.tokenizer_manager")
        module.TokenizerManager = type("TokenizerManager", (), {})
        with mock.patch.dict(sys.modules, {"sglang.srt.managers.tokenizer_manager": module}):
            self.assertFalse(entry._keep_served_name_on_reload())


    def test_an_installation_without_the_manager_module_is_left_alone(self):
        with mock.patch.dict(sys.modules, {"sglang.srt.managers.tokenizer_manager": None}):
            self.assertFalse(entry._keep_served_name_on_reload())


if __name__ == "__main__":
    unittest.main()
