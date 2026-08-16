"""Exceptions for FerriteDB Python SDK."""

from __future__ import annotations


class FerriteError(Exception):
    """Base exception for all FerriteDB SDK errors."""


class FerriteConnectionError(FerriteError):
    """Raised when connection to the FerriteDB sidecar fails or drops."""


class FerriteProtocolError(FerriteError):
    """Raised when protocol negotiation or message framing violates the contract."""


class FerriteSidecarError(FerriteError):
    """Raised when the FerriteDB sidecar process fails to start, crashes, or is missing."""


class FerriteTimeoutError(FerriteError):
    """Raised when a sidecar operation or startup deadline expires."""


class FerriteDatabaseError(FerriteError):
    """Raised when FerriteDB returns an explicit error response."""
