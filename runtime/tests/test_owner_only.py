"""The owner-only rule for mllm's own runtime helpers (Python mirror).

Owner decisions 2026-09-22 and 2026-09-23: mllm's own helper files and the
directories on the way to them are trusted when owned by root or the service
user, never other-writable, and group-writable only through the owner's
private group. mllm's private state stays strict. CPU fixtures only.
"""

import os
from pathlib import Path
import stat
import tempfile
import types
import unittest
from unittest import mock

from runtime import owner_only
from runtime import sglang_saver_binding as saver
from test_sglang_observation_transport import BridgeFixture


def info(mode, uid=None, gid=1000):
    return types.SimpleNamespace(st_mode=stat.S_IFREG | mode,
                                 st_uid=os.geteuid() if uid is None else uid, st_gid=gid)


def private(gid, uid):
    return True


def shared(gid, uid):
    return False


def undetermined(gid, uid):
    return None


class OwnerOnlyRuleTests(unittest.TestCase):
    # T21 T37
    def test_private_group_write_is_the_owners_and_shared_group_write_is_refused(self):
        for mode in (0o664, 0o620, 0o775):
            with self.subTest(mode=oct(mode)):
                owner_only.check(info(mode), private_group=private)
                with self.assertRaises(owner_only.OwnerOnlyError) as caught:
                    owner_only.check(info(mode), private_group=shared)
                self.assertEqual(caught.exception.problem, "writable_by_shared_group")
                with self.assertRaises(owner_only.OwnerOnlyError) as caught:
                    owner_only.check(info(mode), private_group=undetermined)
                self.assertEqual(caught.exception.problem, "group_undetermined")

    # T21 T37
    def test_other_write_and_foreign_owner_are_refused_whatever_the_group(self):
        for lookup in (private, shared, undetermined):
            with self.assertRaises(owner_only.OwnerOnlyError) as caught:
                owner_only.check(info(0o646), private_group=lookup)
            self.assertEqual(caught.exception.problem, "writable_by_other")
        with self.assertRaises(owner_only.OwnerOnlyError) as caught:
            owner_only.check(info(0o644, uid=os.geteuid() + 1), private_group=private)
        self.assertEqual(caught.exception.problem, "not_owned")
        # Without group write the group is never consulted.
        owner_only.check(info(0o644), private_group=undetermined)
        owner_only.check(info(0o644, uid=0), private_group=undetermined)

    # T21 T37: SPEC §9.1, mirrors owner_only.rs. With a directory service in
    # `passwd:` another account may share the group unseen: undetermined.
    def test_a_directory_service_leaves_the_group_undetermined(self):
        self.assertTrue(owner_only.nss_enumerable(None))
        self.assertTrue(owner_only.nss_enumerable("passwd: files\ngroup: files\n"))
        self.assertTrue(owner_only.nss_enumerable("passwd:         files systemd\n"))
        self.assertTrue(owner_only.nss_enumerable(
            "# passwd: ldap\npasswd: files [NOTFOUND=return]\n"))
        for text in ("passwd: files systemd sss\n", "passwd: files ldap\n",
                     "passwd: compat\n", "passwd: nis files\n"):
            with self.subTest(text=text):
                self.assertFalse(owner_only.nss_enumerable(text))
        with mock.patch.object(owner_only, "_nsswitch", return_value="passwd: files sss\n"):
            self.assertIsNone(owner_only.system_private_group(os.getegid(), os.geteuid()))

    # T21 T37: the system lookup against this host's real account database.
    def test_system_lookup_matches_the_account_database(self):
        uid = os.geteuid()
        gid = os.getegid()
        verdict = owner_only.system_private_group(gid, uid)
        self.assertIn(verdict, (True, False, None))
        # A group that does not exist is never assumed private.
        self.assertIsNone(owner_only.system_private_group(2 ** 31 - 7, uid))


class ObservationAncestorTests(unittest.TestCase):
    """The observation listener's ancestors follow the owner-only rule; its
    own 0700 directory and 0600 socket stay strict private state."""

    def setUp(self):
        from runtime import sglang_observation_server as server
        self.server = server
        self.directory = tempfile.TemporaryDirectory(prefix="obs-anc-", dir=Path.home())
        self.addCleanup(self.directory.cleanup)
        self.ancestor = Path(self.directory.name) / "shared"
        self.ancestor.mkdir()
        os.chmod(self.ancestor, 0o775)
        self.sockets = self.ancestor / "sockets"
        self.sockets.mkdir()
        os.chmod(self.sockets, 0o700)
        self.owner = saver.current_process_identity()

    def start(self):
        return self.server.SchedulerObservationServer.start(
            path=str(self.sockets / "o"), bridge=BridgeFixture(self.owner),
            binding_id="binding-1", incarnation_id="incarnation-1",
            expected_owner=self.owner, expected_peer=self.owner)

    # T21 T37
    def test_group_writable_ancestor_is_accepted_only_under_a_private_group(self):
        with mock.patch.object(owner_only, "system_private_group", side_effect=private):
            listener = self.start()
            listener.close()
        with mock.patch.object(owner_only, "system_private_group", side_effect=shared):
            with self.assertRaises(self.server.ObservationServerError):
                self.start()
        self.assertFalse((self.sockets / "o").exists())

    # T21 T37: the socket's own directory is private state: no group write,
    # even under a private group.
    def test_socket_directory_stays_strict(self):
        os.chmod(self.sockets, 0o770)
        with mock.patch.object(owner_only, "system_private_group", side_effect=private):
            with self.assertRaises(self.server.ObservationServerError):
                self.start()


if __name__ == "__main__":
    unittest.main()
