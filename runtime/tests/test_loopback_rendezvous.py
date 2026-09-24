"""CPU tests for the loopback-only single-rank rendezvous (SPEC §8.2, T21).

These check the environment the entries hand the engine; they are not
evidence that an engine opens no listener. That is checked live (`ss -ltnp`).
"""

import os
import stat
import tempfile
import unittest

from runtime import loopback_rendezvous as rendezvous


class PinTests(unittest.TestCase):
    # T21: found live 2026-09-23 (M08), the SGLang scheduler's torch TCPStore
    # listened on `*:<port>` and accepted a connection from another host.
    def test_sglang_uses_a_private_file_store_and_loopback_transports(self):
        environ = {"MASTER_ADDR": "0.0.0.0", "MASTER_PORT": "29500",
                   "NCCL_SOCKET_IFNAME": "eth0", "PATH": "/usr/bin"}
        with tempfile.TemporaryDirectory() as root:
            store = os.path.join(root, "rdzv")
            os.mkdir(store, 0o700)
            expected = rendezvous.pin("sglang", environ, make_dir=lambda: store,
                                      cleanup=False)
            self.assertEqual(environ[rendezvous.SGLANG_OVERRIDE], "file://" + store + "/store")
            self.assertEqual(environ["GLOO_SOCKET_IFNAME"], "lo")
            self.assertEqual(environ["NCCL_SOCKET_IFNAME"], "lo")
            # An inherited TCP rendezvous never survives.
            self.assertNotIn("MASTER_ADDR", environ)
            self.assertNotIn("MASTER_PORT", environ)
            self.assertEqual(environ["PATH"], "/usr/bin")
            rendezvous.verify(expected, environ)

    def test_default_store_directory_is_owner_only(self):
        environ = {}
        expected = rendezvous.pin("sglang", environ, cleanup=False)
        path = expected[rendezvous.SGLANG_OVERRIDE][len("file://"):]
        directory = os.path.dirname(path)
        try:
            self.assertTrue(directory.startswith("/"))
            self.assertEqual(stat.S_IMODE(os.stat(directory).st_mode), 0o700)
        finally:
            os.rmdir(directory)

    # T21: found live 2026-09-23, a signalled stop never runs exit handlers,
    # so the host names the directory (inside its private root) and removes it
    # once the group is gone.
    def test_a_host_named_directory_is_created_owner_only(self):
        with tempfile.TemporaryDirectory() as root:
            os.chmod(root, 0o700)
            store = os.path.join(root, "01K00000000000000000000002")
            environ = {rendezvous.HOST_DIR: store}
            expected = rendezvous.pin("sglang", environ, cleanup=False)
            self.assertEqual(environ[rendezvous.SGLANG_OVERRIDE], "file://" + store + "/store")
            self.assertEqual(stat.S_IMODE(os.stat(store).st_mode), 0o700)
            rendezvous.verify(expected, environ)

    def test_a_host_named_directory_is_refused_unless_fresh_and_private(self):
        with tempfile.TemporaryDirectory() as root:
            os.chmod(root, 0o700)
            existing = os.path.join(root, "existing")
            os.mkdir(existing, 0o700)
            shared = os.path.join(root, "shared")
            os.mkdir(shared, 0o755)
            for path in (existing, os.path.join(shared, "x"), "relative/x",
                         os.path.join(root, "missing", "x")):
                with self.subTest(path=path), self.assertRaises(rendezvous.RendezvousError):
                    rendezvous.pin("sglang", {rendezvous.HOST_DIR: path}, cleanup=False)

    def test_a_shared_store_directory_is_refused(self):
        with tempfile.TemporaryDirectory() as root:
            os.chmod(root, 0o755)
            with self.assertRaises(rendezvous.RendezvousError):
                rendezvous.pin("sglang", {}, make_dir=lambda: root, cleanup=False)

    def test_vllm_resolves_loopback_and_keeps_its_own_file_store(self):
        environ = {"HOST_IP": "10.0.0.5", "VLLM_HOST_IP": "10.0.0.5"}
        expected = rendezvous.pin("vllm", environ)
        self.assertEqual(environ["VLLM_HOST_IP"], "127.0.0.1")
        self.assertNotIn("HOST_IP", environ)
        self.assertNotIn(rendezvous.SGLANG_OVERRIDE, environ)
        rendezvous.verify(expected, environ)

    def test_unknown_engine_is_refused(self):
        with self.assertRaises(rendezvous.RendezvousError):
            rendezvous.pin("other", {})


class VerifyTests(unittest.TestCase):
    def test_any_drift_before_start_is_refused(self):
        base = {}
        expected = rendezvous.pin("vllm", base)
        for name, value in (("VLLM_HOST_IP", "0.0.0.0"), ("GLOO_SOCKET_IFNAME", "eth0"),
                            ("MASTER_ADDR", "127.0.0.1"),
                            (rendezvous.SGLANG_OVERRIDE, "tcp://127.0.0.1:1")):
            environ = dict(base, **{name: value})
            with self.subTest(name=name), self.assertRaises(rendezvous.RendezvousError) as caught:
                rendezvous.verify(expected, environ)
            self.assertNotIn(value, str(caught.exception))
        missing = dict(base)
        del missing["NCCL_SOCKET_IFNAME"]
        with self.assertRaises(rendezvous.RendezvousError):
            rendezvous.verify(expected, missing)

    def test_empty_expectation_is_refused(self):
        with self.assertRaises(rendezvous.RendezvousError):
            rendezvous.verify({}, {})


if __name__ == "__main__":
    unittest.main()
