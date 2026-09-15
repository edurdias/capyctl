"""CPU-only source observations; never saver binary/worker qualification."""

import builtins
import hashlib
import importlib
import importlib.util
import os
from pathlib import Path
import tempfile
import tarfile
import unittest
from unittest import mock


class SaverSourceTests(unittest.TestCase):
    def setUp(self):
        self.assertIsNotNone(importlib.util.find_spec("runtime.saver_source_preflight"),
                             "import-free saver source verifier is missing")
        self.source = importlib.import_module("runtime.saver_source_preflight")
        self.temp = tempfile.TemporaryDirectory(prefix="mllm-saver-source-", dir=Path.home())
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.root.chmod(0o700)
        self.parent = self.root / "hooks"
        self.parent.mkdir(mode=0o700)
        self.leaf = self.parent / "base.py"
        self.payload = b"raise RuntimeError('do not import')\n"
        self.leaf.write_bytes(self.payload)
        self.leaf.chmod(0o600)
        self.inventory = (("hooks/base.py", hashlib.sha256(self.payload).hexdigest()),)

    def observe(self):
        return self.source._observe_sources(str(self.root), self.inventory)

    def rejects(self, code, fn=None):
        with self.assertRaises(self.source.SaverSourcePreflightError) as caught:
            (fn or self.observe)()
        self.assertEqual(caught.exception.code, code)
        self.assertNotIn(str(self.root), repr(caught.exception))

    def test_release_identity_is_not_sglang_revision(self):
        observed = self.observe()
        self.assertEqual(observed.release, "0.0.9.post1")
        self.assertEqual(observed.source_archive_sha256,
                         "25fd4b691ed3242c3a18b2bef0dbe9de84d2e7068b96a37686a923d55c274f43")
        self.assertFalse(hasattr(observed, "source_revision"))
        self.assertNotIn(str(self.root), repr(observed))
        with self.assertRaises(AttributeError):
            observed.release = "untrusted"

    def test_public_api_has_no_caller_inventory(self):
        self.rejects("artifact_missing", lambda: self.source.verify_saver_sources(str(self.root)))
        with self.assertRaises(TypeError):
            self.source.verify_saver_sources(str(self.root), self.inventory)

    def test_changed_bytes_and_missing_file_rejected(self):
        self.leaf.write_bytes(b"different")
        self.rejects("artifact_mismatch")
        self.leaf.unlink()
        self.rejects("artifact_missing")

    def test_symlink_and_fifo_rejected(self):
        original = self.parent / "original.py"
        self.leaf.rename(original)
        self.leaf.symlink_to(original)
        self.rejects("unsafe_file")
        self.leaf.unlink()
        os.mkfifo(self.leaf)
        self.rejects("unsafe_file")

    def test_writable_ancestors_and_leaf_rejected(self):
        for target in (self.root, self.parent, self.leaf):
            original = target.stat().st_mode & 0o777
            try:
                target.chmod(original | 0o020)
                self.rejects("unsafe_file")
            finally:
                target.chmod(original)

    def test_oversized_file_rejected(self):
        with self.leaf.open("wb") as output:
            output.truncate(16 * 1024 * 1024 + 1)
        self.rejects("artifact_mismatch")

    def test_revalidation_rejects_replacement_identity(self):
        before = self.observe()
        # Only the fixed production inventory is substituted; filesystem checks run.
        with mock.patch.object(self.source, "_SOURCES", self.inventory):
            self.assertEqual(self.source.revalidate_saver_sources(before), before)
            self.leaf.rename(self.parent / "old.py")
            self.leaf.write_bytes(self.payload)
            self.leaf.chmod(0o600)
            self.rejects("artifact_changed", lambda: self.source.revalidate_saver_sources(before))
        self.rejects("invalid_root", lambda: self.source.revalidate_saver_sources(object()))

    def test_invalid_root_rejected(self):
        for root in ("relative", str(self.root) + "/", str(self.root) + "/../x"):
            self.rejects("invalid_root", lambda: self.source._observe_sources(root, self.inventory))

    def test_import_and_observation_do_not_import_native_packages(self):
        original = builtins.__import__

        def guarded(name, *args, **kwargs):
            if name.split(".")[0] in ("torch_memory_saver", "torch", "sglang", "transformers", "subprocess"):
                raise AssertionError("effectful import")
            return original(name, *args, **kwargs)

        with mock.patch("builtins.__import__", side_effect=guarded):
            importlib.reload(self.source)
            self.observe()

    @unittest.skipUnless(os.environ.get("TMS_SOURCE_ARCHIVE"), "pinned source archive not supplied")
    def test_fixed_inventory_accepts_archive_and_rejects_each_changed_source(self):
        archive = Path(os.environ["TMS_SOURCE_ARCHIVE"])
        self.assertEqual(hashlib.sha256(archive.read_bytes()).hexdigest(),
                         "25fd4b691ed3242c3a18b2bef0dbe9de84d2e7068b96a37686a923d55c274f43")
        prefix = "torch_memory_saver-0.0.9.post1/torch_memory_saver/"
        payloads = {}
        with tarfile.open(archive, "r:gz") as source_archive:
            for member in source_archive.getmembers():
                if member.name.startswith(prefix) and member.name.endswith(".py"):
                    relative = member.name[len(prefix):]
                    self.assertTrue(member.isfile())
                    self.assertFalse(Path(relative).is_absolute())
                    self.assertNotIn("..", Path(relative).parts)
                    with source_archive.extractfile(member) as incoming:
                        payloads[relative] = incoming.read()
        self.assertEqual(len(payloads), 9)
        self.assertEqual(set(payloads), {name for name, _ in self.source._SOURCES})
        for relative, payload in payloads.items():
            target = self.root / relative
            target.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
            target.write_bytes(payload)
            target.chmod(0o600)
        verified = self.source.verify_saver_sources(str(self.root))
        self.assertEqual(sum(len(record.artifacts) for _, record in verified.directories), 9)
        self.assertEqual(self.source.revalidate_saver_sources(verified), verified)
        for relative, payload in payloads.items():
            with self.subTest(source=relative):
                target = self.root / relative
                target.write_bytes(payload + b"# drift\n")
                self.rejects("artifact_mismatch", lambda: self.source.verify_saver_sources(str(self.root)))
                target.write_bytes(payload)
