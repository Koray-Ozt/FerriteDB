"""Tests for sidecar process management and socket isolation."""

import os
import shutil
import tempfile
import unittest
from ferritedb.client import FerriteDB
from ferritedb.exceptions import FerriteSidecarError
from ferritedb.sidecar import SocketIdentity, get_socket_identity, remove_socket_safe


class TestSidecar(unittest.TestCase):
    """Tests for sidecar startup failures and socket safety guarantees."""

    def setUp(self) -> None:
        self.temp_dir = tempfile.mkdtemp(prefix="ferrite-py-test-")

    def tearDown(self) -> None:
        shutil.rmtree(self.temp_dir, ignore_errors=True)

    def test_missing_binary_fails_cleanly(self) -> None:
        db_path = os.path.join(self.temp_dir, "testdb")
        fake_binary = os.path.join(self.temp_dir, "nonexistent-binary")
        socket_path = os.path.join(self.temp_dir, "missing.sock")

        with self.assertRaises(FerriteSidecarError) as ctx:
            FerriteDB.open(db_path, binary=fake_binary, socket_path=socket_path)

        self.assertIn("does not exist", str(ctx.exception))
        self.assertFalse(os.path.exists(socket_path))

    def test_preexisting_socket_is_rejected_without_deletion(self) -> None:
        db_path = os.path.join(self.temp_dir, "testdb")
        socket_path = os.path.join(self.temp_dir, "existing.sock")

        with open(socket_path, "w", encoding="utf-8") as f:
            f.write("user-owned-file")

        with self.assertRaises(FerriteSidecarError) as ctx:
            FerriteDB.open(db_path, socket_path=socket_path)

        self.assertIn("socket path already exists", str(ctx.exception))
        self.assertTrue(os.path.exists(socket_path))
        with open(socket_path, "r", encoding="utf-8") as f:
            self.assertEqual(f.read(), "user-owned-file")

    def test_remove_socket_safe_identity_mismatch(self) -> None:
        file_path = os.path.join(self.temp_dir, "plain.txt")
        with open(file_path, "w", encoding="utf-8") as f:
            f.write("test")

        # Identity pointing to different inode
        dummy_identity = SocketIdentity(dev=1, ino=99999999)
        # Should not remove a plain file or non-matching socket
        remove_socket_safe(file_path, dummy_identity)
        self.assertTrue(os.path.exists(file_path))


if __name__ == "__main__":
    unittest.main()
