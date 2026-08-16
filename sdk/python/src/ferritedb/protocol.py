"""Protocol negotiation and message serialization for FerriteDB."""

from __future__ import annotations

import json
from typing import Any

from .exceptions import FerriteProtocolError
from .types import ProtocolNegotiation, ProtocolOffer

DEFAULT_PROTOCOL_OFFER = ProtocolOffer()


def build_hello_request(req_id: int, offer: ProtocolOffer = DEFAULT_PROTOCOL_OFFER) -> dict[str, Any]:
    """Constructs the initial protocol handshake request."""
    return {
        "version": 1,
        "id": req_id,
        "method": "hello",
        "protocol": {
            "min": offer.min_protocol,
            "max": offer.max_protocol,
        },
        "compression": list(offer.compression),
        "capabilities": {
            "required": list(offer.required_capabilities),
            "optional": list(offer.optional_capabilities),
        },
    }


def validate_negotiation(value: Any, offer: ProtocolOffer = DEFAULT_PROTOCOL_OFFER) -> ProtocolNegotiation:
    """Validates the sidecar's hello response against offered protocol parameters."""
    if not isinstance(value, dict):
        raise FerriteProtocolError("invalid protocol negotiation: expected an object")

    protocol = value.get("protocol")
    if type(protocol) is not int or protocol < offer.min_protocol or protocol > offer.max_protocol:
        raise FerriteProtocolError("invalid protocol negotiation: protocol was not offered")

    compression = value.get("compression")
    if not isinstance(compression, str) or compression not in offer.compression:
        raise FerriteProtocolError("invalid protocol negotiation: compression was not offered")

    raw_capabilities = value.get("capabilities")
    if not isinstance(raw_capabilities, list) or not all(isinstance(c, str) for c in raw_capabilities):
        raise FerriteProtocolError("invalid protocol negotiation: capabilities must be strings")

    capabilities = list(raw_capabilities)
    for req in offer.required_capabilities:
        if req not in capabilities:
            raise FerriteProtocolError("invalid protocol negotiation: required capability missing")

    allowed_capabilities = set(offer.required_capabilities).union(offer.optional_capabilities)
    for cap in capabilities:
        if cap not in allowed_capabilities:
            raise FerriteProtocolError("invalid protocol negotiation: capability was not offered")

    return ProtocolNegotiation(
        protocol=protocol,
        compression=compression,
        capabilities=tuple(capabilities),
    )


def encode_request(request: dict[str, Any]) -> bytes:
    """Encodes a request dictionary into an NDJSON byte line."""
    return json.dumps(request, separators=(",", ":")).encode("utf-8") + b"\n"


def decode_response(line: bytes | str) -> dict[str, Any]:
    """Decodes an NDJSON line into a response dictionary."""
    if isinstance(line, bytes):
        line = line.decode("utf-8")
    line = line.strip()
    if not line:
        raise FerriteProtocolError("Empty response line received")
    try:
        data = json.loads(line)
    except json.JSONDecodeError as err:
        raise FerriteProtocolError(f"Malformed JSON response: {err}") from err

    if not isinstance(data, dict):
        raise FerriteProtocolError("Expected JSON object in response")
    return data
