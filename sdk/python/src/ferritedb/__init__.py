"""FerriteDB Python SDK.

Official client library for FerriteDB with sync and async support.
"""

from __future__ import annotations

from .client import AsyncFerriteDB, FerriteDB
from .exceptions import (
    FerriteConnectionError,
    FerriteDatabaseError,
    FerriteError,
    FerriteProtocolError,
    FerriteSidecarError,
    FerriteTimeoutError,
)
from .protocol import validate_negotiation
from .types import (
    Delete,
    OpenOptions,
    Operation,
    ProtocolNegotiation,
    ProtocolOffer,
    Put,
)

__version__ = "0.1.0"

__all__ = [
    "AsyncFerriteDB",
    "Delete",
    "FerriteConnectionError",
    "FerriteDatabaseError",
    "FerriteDB",
    "FerriteError",
    "FerriteProtocolError",
    "FerriteSidecarError",
    "FerriteTimeoutError",
    "OpenOptions",
    "Operation",
    "ProtocolNegotiation",
    "ProtocolOffer",
    "Put",
    "validate_negotiation",
    "__version__",
]
