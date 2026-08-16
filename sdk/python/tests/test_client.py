"""Unit tests for FerriteDB client error handling and operations."""

import socket
import threading
import unittest
from ferritedb.client import FerriteDB
from ferritedb.exceptions import (
    FerriteConnectionError,
    FerriteDatabaseError,
    FerriteProtocolError,
)
from ferritedb.protocol import encode_request
from ferritedb.types import Delete, OpenOptions, Put


class MockServer:
    """Minimal mock Unix socket server for testing handshake and protocol edge cases."""

    def __init__(self, socket_path: str) -> None:
        self.socket_path = socket_path
        self.server_sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.server_sock.bind(socket_path)
        self.server_sock.listen(1)
        self._thread = threading.Thread(target=self._run, daemon=True)
        self.responses: list[bytes] = []
        self._thread.start()

    def _run(self) -> None:
        try:
            conn, _ = self.server_sock.accept()
            file = conn.makefile("r", encoding="utf-8")
            for _ in range(10):
                line = file.readline()
                if not line:
                    break
                if self.responses:
                    conn.sendall(self.responses.pop(0))
                else:
                    # Default handshake response
                    resp = encode_request({
                        "version": 1,
                        "id": 1,
                        "ok": True,
                        "result": {
                            "protocol": 1,
                            "compression": "none",
                            "capabilities": ["kv", "transactions", "prefix-list"],
                        },
                    })
                    conn.sendall(resp)
            conn.close()
        except Exception:
            pass
        finally:
            self.server_sock.close()


class TestClientUnit(unittest.TestCase):
    """Unit tests for client state and options."""

    def test_open_options_defaults(self) -> None:
        opts = OpenOptions()
        self.assertIsNone(opts.binary)
        self.assertIsNone(opts.schema)
        self.assertIsNone(opts.socket)
        self.assertEqual(opts.timeout, 5.0)

    def test_closed_client_raises_connection_error(self) -> None:
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        client = FerriteDB(sock=sock)
        client.close()
        self.assertTrue(client.is_closed)

        with self.assertRaises(FerriteConnectionError):
            client.put("k", "v")

        with self.assertRaises(FerriteConnectionError):
            client.get("k")

        with self.assertRaises(FerriteConnectionError):
            client.delete("k")

        with self.assertRaises(FerriteConnectionError):
            client.list()

        with self.assertRaises(FerriteConnectionError):
            client.transaction([Put("a", 1)])


if __name__ == "__main__":
    unittest.main()
