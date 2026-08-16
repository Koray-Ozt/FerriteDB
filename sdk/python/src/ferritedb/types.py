"""Type definitions and data models for FerriteDB Python SDK."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Mapping, Sequence, Union


@dataclass(frozen=True, slots=True)
class Put:
    """Represents a transactional put operation."""

    key: str
    value: Any

    def to_dict(self) -> dict[str, Any]:
        return {"Put": {"key": self.key, "value": self.value}}


@dataclass(frozen=True, slots=True)
class Delete:
    """Represents a transactional delete operation."""

    key: str

    def to_dict(self) -> dict[str, Any]:
        return {"Delete": {"key": self.key}}


Operation = Union[Put, Delete, Mapping[str, Any]]


def serialize_operation(op: Operation) -> dict[str, Any]:
    """Converts an Operation into its wire format dictionary."""
    if isinstance(op, (Put, Delete)):
        return op.to_dict()
    if isinstance(op, Mapping):
        if "Put" in op or "Delete" in op:
            return dict(op)
        raise ValueError(f"Invalid operation mapping: {op}")
    raise TypeError(f"Unsupported operation type: {type(op)}")


@dataclass(slots=True)
class OpenOptions:
    """Options for opening a FerriteDB database instance."""

    binary: str | None = None
    schema: str | None = None
    socket: str | None = None
    timeout: float = 5.0


@dataclass(frozen=True, slots=True)
class ProtocolNegotiation:
    """Result of a successful protocol handshake with the sidecar."""

    protocol: int
    compression: str
    capabilities: tuple[str, ...] = field(default_factory=tuple)


@dataclass(frozen=True, slots=True)
class ProtocolOffer:
    """Capabilities and version range offered during handshake."""

    min_protocol: int = 1
    max_protocol: int = 1
    compression: tuple[str, ...] = ("none",)
    required_capabilities: tuple[str, ...] = ("kv", "transactions")
    optional_capabilities: tuple[str, ...] = ("prefix-list",)
