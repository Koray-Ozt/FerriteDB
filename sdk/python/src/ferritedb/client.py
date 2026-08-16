"""Synchronous and asynchronous FerriteDB clients."""

from __future__ import annotations

import asyncio
import os
import socket
from typing import Any, Sequence

from .exceptions import (
    FerriteConnectionError,
    FerriteDatabaseError,
    FerriteError,
    FerriteProtocolError,
)
from .protocol import (
    DEFAULT_PROTOCOL_OFFER,
    build_hello_request,
    decode_response,
    encode_request,
    validate_negotiation,
)
from .sidecar import SidecarProcess
from .types import (
    OpenOptions,
    Operation,
    ProtocolNegotiation,
    ProtocolOffer,
    serialize_operation,
)


class FerriteDB:
    """Synchronous client for FerriteDB."""

    def __init__(
        self,
        sock: socket.socket,
        sidecar: SidecarProcess | None = None,
        negotiation: ProtocolNegotiation | None = None,
    ) -> None:
        self._sock = sock
        self._sidecar = sidecar
        self._negotiation = negotiation
        self._next_id = 1
        self._closed = False
        self._file = sock.makefile("r", encoding="utf-8", newline="\n")

    @classmethod
    def open(
        cls,
        path: str,
        options: OpenOptions | None = None,
        *,
        binary: str | None = None,
        schema: str | None = None,
        socket_path: str | None = None,
        timeout: float = 5.0,
    ) -> FerriteDB:
        """Opens or creates a FerriteDB database, launching the sidecar process."""
        opts = options or OpenOptions(
            binary=binary,
            schema=schema,
            socket=socket_path,
            timeout=timeout,
        )
        if binary is not None:
            opts.binary = binary
        if schema is not None:
            opts.schema = schema
        if socket_path is not None:
            opts.socket = socket_path

        sidecar = SidecarProcess.launch(
            db_path=path,
            binary=opts.binary,
            socket_path=opts.socket,
            schema=opts.schema,
            timeout=opts.timeout,
        )

        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            sock.settimeout(opts.timeout)
            sock.connect(sidecar.socket_path)
            client = cls(sock=sock, sidecar=sidecar)
            client._negotiate()
            return client
        except Exception:
            sock.close()
            sidecar.stop()
            raise

    @property
    def protocol(self) -> ProtocolNegotiation:
        """Negotiated protocol parameters with the sidecar."""
        if self._negotiation is None:
            raise FerriteProtocolError("Protocol handshake is incomplete")
        return self._negotiation

    @property
    def is_closed(self) -> bool:
        """Returns True if the client connection has been closed."""
        return self._closed

    def put(self, key: str, value: Any) -> None:
        """Stores a JSON-serializable value at the given key."""
        self._request("put", {"key": key, "value": value})

    def get(self, key: str) -> Any | None:
        """Retrieves the value for the given key, or None if not found."""
        return self._request("get", {"key": key})

    def delete(self, key: str) -> None:
        """Deletes the record with the given key."""
        self._request("delete", {"key": key})

    def list(self, prefix: str | None = None) -> list[tuple[str, Any]]:
        """Lists key-value pairs, optionally filtered by key prefix."""
        payload: dict[str, Any] = {}
        if prefix is not None:
            payload["prefix"] = prefix
        res = self._request("list", payload)
        if not isinstance(res, list):
            return []
        return [(item[0], item[1]) for item in res]

    def transaction(self, operations: Sequence[Operation | dict[str, Any]]) -> None:
        """Executes an atomic batch of Put and Delete operations."""
        serialized = [serialize_operation(op) for op in operations]
        self._request("transaction", {"operations": serialized})

    def close(self) -> None:
        """Closes the socket connection and stops the sidecar process."""
        if self._closed:
            return
        self._closed = True
        try:
            self._file.close()
        except Exception:
            pass
        try:
            self._sock.close()
        except Exception:
            pass
        if self._sidecar:
            self._sidecar.stop()

    def __enter__(self) -> FerriteDB:
        return self

    def __exit__(self, exc_type: Any, exc_val: Any, exc_tb: Any) -> None:
        self.close()

    def _request(self, method: str, fields: dict[str, Any]) -> Any:
        if self._closed:
            raise FerriteConnectionError("Cannot execute operation on closed FerriteDB connection")

        req_id = self._next_id
        self._next_id += 1
        req = {"version": 1, "id": req_id, "method": method, **fields}

        try:
            self._sock.sendall(encode_request(req))
            line = self._file.readline()
            if not line:
                raise FerriteConnectionError("Server closed connection prematurely")
        except OSError as err:
            raise FerriteConnectionError(f"Communication error with sidecar: {err}") from err

        response = decode_response(line)
        if not response.get("ok"):
            error_msg = response.get("error", "Unknown FerriteDB error")
            raise FerriteDatabaseError(str(error_msg))

        return response.get("result")

    def _negotiate(self, offer: ProtocolOffer = DEFAULT_PROTOCOL_OFFER) -> None:
        req_id = self._next_id
        self._next_id += 1
        req = build_hello_request(req_id, offer)

        try:
            self._sock.sendall(encode_request(req))
            line = self._file.readline()
            if not line:
                raise FerriteConnectionError("Server closed connection during handshake")
        except OSError as err:
            raise FerriteConnectionError(f"Failed handshake with sidecar: {err}") from err

        response = decode_response(line)
        if not response.get("ok"):
            raise FerriteProtocolError(f"Handshake rejected: {response.get('error')}")

        self._negotiation = validate_negotiation(response.get("result"), offer)


class AsyncFerriteDB:
    """Asynchronous (asyncio) client for FerriteDB."""

    def __init__(
        self,
        reader: asyncio.StreamReader,
        writer: asyncio.StreamWriter,
        sidecar: SidecarProcess | None = None,
        negotiation: ProtocolNegotiation | None = None,
    ) -> None:
        self._reader = reader
        self._writer = writer
        self._sidecar = sidecar
        self._negotiation = negotiation
        self._next_id = 1
        self._closed = False

    @classmethod
    async def open(
        cls,
        path: str,
        options: OpenOptions | None = None,
        *,
        binary: str | None = None,
        schema: str | None = None,
        socket_path: str | None = None,
        timeout: float = 5.0,
    ) -> AsyncFerriteDB:
        """Asynchronously opens or creates a FerriteDB database."""
        opts = options or OpenOptions(
            binary=binary,
            schema=schema,
            socket=socket_path,
            timeout=timeout,
        )
        if binary is not None:
            opts.binary = binary
        if schema is not None:
            opts.schema = schema
        if socket_path is not None:
            opts.socket = socket_path

        loop = asyncio.get_running_loop()
        sidecar = await loop.run_in_executor(
            None,
            lambda: SidecarProcess.launch(
                db_path=path,
                binary=opts.binary,
                socket_path=opts.socket,
                schema=opts.schema,
                timeout=opts.timeout,
            ),
        )

        try:
            reader, writer = await asyncio.wait_for(
                asyncio.open_unix_connection(sidecar.socket_path),
                timeout=opts.timeout,
            )
            client = cls(reader=reader, writer=writer, sidecar=sidecar)
            await client._negotiate()
            return client
        except Exception:
            sidecar.stop()
            raise

    @property
    def protocol(self) -> ProtocolNegotiation:
        """Negotiated protocol parameters with the sidecar."""
        if self._negotiation is None:
            raise FerriteProtocolError("Protocol handshake is incomplete")
        return self._negotiation

    @property
    def is_closed(self) -> bool:
        """Returns True if the async client connection has been closed."""
        return self._closed

    async def put(self, key: str, value: Any) -> None:
        """Asynchronously stores a JSON-serializable value at the given key."""
        await self._request("put", {"key": key, "value": value})

    async def get(self, key: str) -> Any | None:
        """Asynchronously retrieves the value for the given key, or None if not found."""
        return await self._request("get", {"key": key})

    async def delete(self, key: str) -> None:
        """Asynchronously deletes the record with the given key."""
        await self._request("delete", {"key": key})

    async def list(self, prefix: str | None = None) -> list[tuple[str, Any]]:
        """Asynchronously lists key-value pairs, optionally filtered by key prefix."""
        payload: dict[str, Any] = {}
        if prefix is not None:
            payload["prefix"] = prefix
        res = await self._request("list", payload)
        if not isinstance(res, list):
            return []
        return [(item[0], item[1]) for item in res]

    async def transaction(self, operations: Sequence[Operation | dict[str, Any]]) -> None:
        """Asynchronously executes an atomic batch of Put and Delete operations."""
        serialized = [serialize_operation(op) for op in operations]
        await self._request("transaction", {"operations": serialized})

    async def close(self) -> None:
        """Asynchronously closes the connection and stops the sidecar process."""
        if self._closed:
            return
        self._closed = True
        try:
            self._writer.close()
            await self._writer.wait_closed()
        except Exception:
            pass
        if self._sidecar:
            loop = asyncio.get_running_loop()
            await loop.run_in_executor(None, self._sidecar.stop)

    async def __aenter__(self) -> AsyncFerriteDB:
        return self

    async def __aexit__(self, exc_type: Any, exc_val: Any, exc_tb: Any) -> None:
        await self.close()

    async def _request(self, method: str, fields: dict[str, Any]) -> Any:
        if self._closed:
            raise FerriteConnectionError("Cannot execute operation on closed FerriteDB connection")

        req_id = self._next_id
        self._next_id += 1
        req = {"version": 1, "id": req_id, "method": method, **fields}

        try:
            self._writer.write(encode_request(req))
            await self._writer.drain()
            line = await self._reader.readline()
            if not line:
                raise FerriteConnectionError("Server closed connection prematurely")
        except OSError as err:
            raise FerriteConnectionError(f"Communication error with sidecar: {err}") from err

        response = decode_response(line)
        if not response.get("ok"):
            error_msg = response.get("error", "Unknown FerriteDB error")
            raise FerriteDatabaseError(str(error_msg))

        return response.get("result")

    async def _negotiate(self, offer: ProtocolOffer = DEFAULT_PROTOCOL_OFFER) -> None:
        req_id = self._next_id
        self._next_id += 1
        req = build_hello_request(req_id, offer)

        try:
            self._writer.write(encode_request(req))
            await self._writer.drain()
            line = await self._reader.readline()
            if not line:
                raise FerriteConnectionError("Server closed connection during handshake")
        except OSError as err:
            raise FerriteConnectionError(f"Failed handshake with sidecar: {err}") from err

        response = decode_response(line)
        if not response.get("ok"):
            raise FerriteProtocolError(f"Handshake rejected: {response.get('error')}")

        self._negotiation = validate_negotiation(response.get("result"), offer)
