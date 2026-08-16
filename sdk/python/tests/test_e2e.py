"""End-to-end integration tests using compiled FerriteDB sidecar binary."""

import asyncio
import json
import os
import shutil
import socket
import tempfile
import unittest
from ferritedb.client import AsyncFerriteDB, FerriteDB
from ferritedb.exceptions import FerriteConnectionError, FerriteDatabaseError
from ferritedb.sidecar import default_binary
from ferritedb.types import Delete, Put


class TestE2E(unittest.TestCase):
    """End-to-end tests validating Python SDK against the live Rust sidecar."""

    def setUp(self) -> None:
        self.temp_dir = tempfile.mkdtemp(prefix="ferrite-py-e2e-")
        self.binary = default_binary()

    def tearDown(self) -> None:
        shutil.rmtree(self.temp_dir, ignore_errors=True)

    def test_sync_sidecar_crud_and_transactions(self) -> None:
        db_path = os.path.join(self.temp_dir, "sync_db")
        socket_path = os.path.join(self.temp_dir, "sync_sidecar.sock")

        # 1. Open database
        db = FerriteDB.open(db_path, binary=self.binary, socket_path=socket_path)
        self.assertEqual(db.protocol.protocol, 1)
        self.assertEqual(db.protocol.compression, "none")
        self.assertIn("kv", db.protocol.capabilities)
        self.assertIn("transactions", db.protocol.capabilities)
        self.assertIn("prefix-list", db.protocol.capabilities)

        # 2. Transaction put
        db.transaction([
            Put("users/1", {"name": "Ada Lovelace", "active": True}),
            Put("users/2", {"name": "Grace Hopper", "active": False}),
            Put("config/theme", "dark"),
        ])

        # 3. Get records
        user1 = db.get("users/1")
        self.assertEqual(user1, {"name": "Ada Lovelace", "active": True})
        self.assertIsNone(db.get("users/nonexistent"))

        # 4. List records
        users = db.list("users/")
        self.assertEqual(len(users), 2)
        user_keys = [k for k, _ in users]
        self.assertEqual(user_keys, ["users/1", "users/2"])

        # 5. Single put and delete
        db.put("users/3", {"name": "Margaret Hamilton"})
        self.assertEqual(db.get("users/3"), {"name": "Margaret Hamilton"})
        db.delete("users/2")
        self.assertIsNone(db.get("users/2"))

        # Unfiltered list
        all_items = db.list()
        self.assertEqual(len(all_items), 3)  # users/1, config/theme, users/3

        # Complex nested structure
        nested_payload = {
            "matrix": [[1, 2], [3, 4]],
            "flags": [True, False, None],
            "meta": {"created_at": 1700000000, "tags": ["db", "rust", "python"]},
        }
        db.put("complex/doc", nested_payload)
        self.assertEqual(db.get("complex/doc"), nested_payload)

        # 6. Close and check file artifacts
        db.close()
        self.assertFalse(os.path.exists(socket_path))
        self.assertTrue(db.is_closed)

        with open(os.path.join(db_path, "data.wal"), "rb") as wal_file:
            wal_header = wal_file.read(8)
            self.assertEqual(wal_header, b"FRTWAL01")

        # 7. Reopen database and verify persistence
        reopened = FerriteDB.open(db_path, binary=self.binary)
        self.assertEqual(reopened.get("users/1"), {"name": "Ada Lovelace", "active": True})
        self.assertIsNone(reopened.get("users/2"))
        self.assertEqual(reopened.get("users/3"), {"name": "Margaret Hamilton"})
        self.assertEqual(reopened.get("config/theme"), "dark")
        self.assertEqual(reopened.get("complex/doc"), nested_payload)
        reopened.close()

    def test_sync_context_manager(self) -> None:
        db_path = os.path.join(self.temp_dir, "ctx_db")
        with FerriteDB.open(db_path, binary=self.binary) as db:
            db.put("alpha", 123)
            self.assertEqual(db.get("alpha"), 123)
        self.assertTrue(db.is_closed)
        with self.assertRaises(FerriteConnectionError):
            db.get("alpha")

    def test_schema_constraints_enforcement(self) -> None:
        db_path = os.path.join(self.temp_dir, "schema_db")
        schema_file = os.path.join(self.temp_dir, "schema.json")
        schema_data = {
            "collections": {
                "accounts": {
                    "primary_key": "id",
                    "unique": ["email"],
                }
            }
        }
        with open(schema_file, "w", encoding="utf-8") as f:
            json.dump(schema_data, f)

        with FerriteDB.open(db_path, binary=self.binary, schema=schema_file) as db:
            # Valid put
            db.put("accounts/1", {"id": "1", "email": "user1@example.com"})
            self.assertEqual(db.get("accounts/1"), {"id": "1", "email": "user1@example.com"})

            # Duplicate unique field 'email'
            with self.assertRaises(FerriteDatabaseError):
                db.put("accounts/2", {"id": "2", "email": "user1@example.com"})

            # Mismatched primary key in body vs key
            with self.assertRaises(FerriteDatabaseError):
                db.put("accounts/3", {"id": "mismatch", "email": "user3@example.com"})

    def test_close_preserves_replacement_socket(self) -> None:
        db_path = os.path.join(self.temp_dir, "socket_race_db")
        socket_path = os.path.join(self.temp_dir, "db.sock")

        db = FerriteDB.open(db_path, binary=self.binary, socket_path=socket_path)
        os.unlink(socket_path)

        # Create a new replacement socket listening at the same path
        replacement_sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        replacement_sock.bind(socket_path)
        try:
            db.close()
            # The replacement socket must NOT have been deleted by db.close()
            self.assertTrue(os.path.exists(socket_path))
        finally:
            replacement_sock.close()
            if os.path.exists(socket_path):
                os.unlink(socket_path)

    def test_async_sidecar_crud_and_transactions(self) -> None:
        async def run_async_test() -> None:
            db_path = os.path.join(self.temp_dir, "async_db")
            socket_path = os.path.join(self.temp_dir, "async_sidecar.sock")

            async with await AsyncFerriteDB.open(
                db_path,
                binary=self.binary,
                socket_path=socket_path,
            ) as db:
                self.assertEqual(db.protocol.protocol, 1)
                await db.put("temp/1", {"celsius": 21.0})
                self.assertEqual(await db.get("temp/1"), {"celsius": 21.0})

                await db.transaction([
                    Put("temp/2", {"celsius": 22.5}),
                    Put("temp/3", {"celsius": 24.0}),
                    Delete("temp/1"),
                ])

                self.assertIsNone(await db.get("temp/1"))
                self.assertEqual(await db.get("temp/2"), {"celsius": 22.5})

                items = await db.list("temp/")
                self.assertEqual(len(items), 2)

            self.assertFalse(os.path.exists(socket_path))

        asyncio.run(run_async_test())


if __name__ == "__main__":
    unittest.main()
