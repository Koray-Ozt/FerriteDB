"""Tests for FerriteDB protocol negotiation and serialization."""

import unittest
from ferritedb.exceptions import FerriteProtocolError
from ferritedb.protocol import (
    DEFAULT_PROTOCOL_OFFER,
    build_hello_request,
    decode_response,
    encode_request,
    validate_negotiation,
)
from ferritedb.types import Delete, ProtocolOffer, Put, serialize_operation


class TestProtocol(unittest.TestCase):
    """Tests for protocol negotiation rules and message framing."""

    def setUp(self) -> None:
        self.offer = ProtocolOffer(
            min_protocol=1,
            max_protocol=1,
            compression=("none",),
            required_capabilities=("kv", "transactions"),
            optional_capabilities=("prefix-list",),
        )

    def test_build_hello_request(self) -> None:
        req = build_hello_request(42, self.offer)
        self.assertEqual(req["version"], 1)
        self.assertEqual(req["id"], 42)
        self.assertEqual(req["method"], "hello")
        self.assertEqual(req["protocol"], {"min": 1, "max": 1})
        self.assertEqual(req["compression"], ["none"])
        self.assertEqual(req["capabilities"]["required"], ["kv", "transactions"])
        self.assertEqual(req["capabilities"]["optional"], ["prefix-list"])

    def test_validates_compatible_negotiation(self) -> None:
        result = validate_negotiation(
            {
                "protocol": 1,
                "compression": "none",
                "capabilities": ["kv", "transactions", "prefix-list"],
            },
            self.offer,
        )
        self.assertEqual(result.protocol, 1)
        self.assertEqual(result.compression, "none")
        self.assertEqual(result.capabilities, ("kv", "transactions", "prefix-list"))

    def test_rejects_incompatible_or_malformed_negotiations(self) -> None:
        invalid_cases = [
            None,
            {},
            "invalid",
            123,
            {"protocol": "1", "compression": "none", "capabilities": ["kv", "transactions"]},
            {"protocol": 2, "compression": "none", "capabilities": ["kv", "transactions"]},
            {"protocol": 0, "compression": "none", "capabilities": ["kv", "transactions"]},
            {"protocol": 1, "compression": "gzip", "capabilities": ["kv", "transactions"]},
            {"protocol": 1, "compression": "none", "capabilities": ["kv"]},
            {"protocol": 1, "compression": "none", "capabilities": ["kv", "transactions", "unknown-cap"]},
            {"protocol": 1, "compression": "none", "capabilities": ["kv", "transactions", 123]},
        ]

        for case in invalid_cases:
            with self.subTest(case=case):
                with self.assertRaises(FerriteProtocolError):
                    validate_negotiation(case, self.offer)

    def test_operation_serialization(self) -> None:
        put_op = Put("users/1", {"name": "Ada"})
        self.assertEqual(serialize_operation(put_op), {"Put": {"key": "users/1", "value": {"name": "Ada"}}})

        del_op = Delete("users/1")
        self.assertEqual(serialize_operation(del_op), {"Delete": {"key": "users/1"}})

        raw_put = {"Put": {"key": "a", "value": 1}}
        self.assertEqual(serialize_operation(raw_put), raw_put)

        with self.assertRaises(ValueError):
            serialize_operation({"Unknown": 123})

        with self.assertRaises(TypeError):
            serialize_operation(123)  # type: ignore

    def test_encoding_decoding_roundtrip(self) -> None:
        msg = {"version": 1, "id": 1, "method": "put", "key": "k", "value": "v"}
        encoded = encode_request(msg)
        self.assertTrue(encoded.endswith(b"\n"))
        decoded = decode_response(encoded)
        self.assertEqual(decoded, msg)

    def test_decode_invalid_json(self) -> None:
        with self.assertRaises(FerriteProtocolError):
            decode_response(b"{invalid json\n")
        with self.assertRaises(FerriteProtocolError):
            decode_response(b"\n")


if __name__ == "__main__":
    unittest.main()
