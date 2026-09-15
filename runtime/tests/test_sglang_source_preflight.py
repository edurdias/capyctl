"""Synthetic source fixtures, never installed-runtime qualification evidence."""

import builtins
import hashlib
import importlib
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock

from runtime import sglang_source_preflight as source


class SourceTests(unittest.TestCase):
    def setUp(self):
        # /tmp is intentionally not a protected installation ancestor.
        self.temp = tempfile.TemporaryDirectory(prefix="mllm-source-test-", dir=Path.home())
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.parent = self.root / "entrypoints"
        self.parent.mkdir(mode=0o700)
        self.leaf = self.parent / "engine.py"
        self.payload = b"raise RuntimeError('never import this fixture')\n"
        self.leaf.write_bytes(self.payload)
        self.leaf.chmod(0o600)
        self.inventory = (("entrypoints/engine.py", hashlib.sha256(self.payload).hexdigest()),)

    def observe(self):
        return source._observe_sources(str(self.root), self.inventory)

    def rejects(self, code, fn=None):
        with self.assertRaises(source.SourcePreflightError) as caught:
            (fn or self.observe)()
        self.assertEqual(caught.exception.code, code)
        self.assertNotIn(str(self.root), repr(caught.exception))

    def test_valid_observation_is_immutable_and_root_is_private(self):
        observed = self.observe()
        self.assertEqual(observed.source_revision, source._REVISION)
        self.assertNotIn(str(self.root), repr(observed))
        with self.assertRaises(AttributeError):
            observed.source_revision = "main"

    def test_compiled_inventory_is_closed_and_well_formed(self):
        self.assertEqual(len(source._SOURCES), 9)
        self.assertEqual(len({name for name, _ in source._SOURCES}), 9)
        self.assertIn("plugins/__init__.py", dict(source._SOURCES))
        self.assertIn("platforms/__init__.py", dict(source._SOURCES))
        for name, digest in source._SOURCES:
            self.assertFalse(name.startswith("/"))
            self.assertNotIn("..", name.split("/"))
            self.assertRegex(digest, r"^[0-9a-f]{64}$")

    def test_production_api_rejects_synthetic_hash_inventory(self):
        self.rejects("artifact_missing", lambda: source.verify_sglang_sources(str(self.root)))
        with self.assertRaises(TypeError):
            source.verify_sglang_sources(str(self.root), self.inventory)

    def test_byte_mismatch(self):
        self.leaf.write_bytes(b"x" * len(self.payload))
        self.rejects("artifact_mismatch")

    def test_missing_leaf(self):
        self.leaf.unlink()
        self.rejects("artifact_missing")

    def test_symlink_intermediate(self):
        other = self.root / "real"
        self.parent.rename(other)
        self.parent.symlink_to(other, target_is_directory=True)
        self.rejects("unsafe_file")

    def test_symlink_leaf(self):
        other = self.parent / "other.py"
        self.leaf.rename(other)
        self.leaf.symlink_to(other)
        self.rejects("unsafe_file")

    def test_fifo_leaf_does_not_block(self):
        self.leaf.unlink()
        os.mkfifo(self.leaf)
        self.rejects("unsafe_file")

    def test_untrusted_write_permissions(self):
        for target in (self.root, self.parent, self.leaf):
            original = target.stat().st_mode & 0o777
            for extra in (0o020, 0o002):
                try:
                    target.chmod(original | extra)
                    self.rejects("unsafe_file")
                finally:
                    target.chmod(original)

    def test_oversized_source(self):
        with self.leaf.open("wb") as file:
            file.truncate(16 * 1024 * 1024 + 1)
        self.rejects("artifact_mismatch")

    def test_same_bytes_new_inode_rejected(self):
        before = self.observe()
        self.leaf.rename(self.leaf.with_suffix(".old"))
        self.leaf.write_bytes(self.payload)
        self.leaf.chmod(0o600)
        after = self.observe()
        self.assertNotEqual(before, after)
        with mock.patch.object(source, "verify_sglang_sources", return_value=after):
            self.rejects("artifact_changed", lambda: source.revalidate_sglang_sources(before))

    def test_revalidation_requires_same_inventory_and_identity(self):
        before = self.observe()
        with mock.patch.object(source, "verify_sglang_sources", return_value=self.observe()):
            self.assertEqual(source.revalidate_sglang_sources(before), before)
        self.rejects("invalid_root", lambda: source.revalidate_sglang_sources(object()))

    def test_invalid_roots(self):
        for root in ("relative", str(self.root) + "/", str(self.root) + "/../x", "//tmp"):
            self.rejects("invalid_root", lambda: source._observe_sources(root, self.inventory))

    def test_import_and_observation_never_import_engine(self):
        original = builtins.__import__

        def guarded(name, *args, **kwargs):
            if name.split(".")[0] in ("sglang", "torch", "transformers", "subprocess"):
                raise AssertionError("effectful import")
            return original(name, *args, **kwargs)

        with mock.patch("builtins.__import__", side_effect=guarded):
            importlib.reload(source)
            self.observe()
