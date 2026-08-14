import { strict as assert } from "node:assert";
import test from "node:test";
import { validateNegotiation } from "../src/protocol.js";

const offer = {
  protocol: { min: 1, max: 1 },
  compression: ["none"],
  capabilities: {
    required: ["kv", "transactions"],
    optional: ["prefix-list"]
  }
} as const;

test("validates a compatible protocol negotiation", () => {
  assert.deepEqual(
    validateNegotiation(
      { protocol: 1, compression: "none", capabilities: ["kv", "transactions", "prefix-list"] },
      offer
    ),
    { protocol: 1, compression: "none", capabilities: ["kv", "transactions", "prefix-list"] }
  );
});

test("rejects malformed or incompatible protocol negotiations", () => {
  const invalid = [
    null,
    {},
    { protocol: "1", compression: "none", capabilities: ["kv", "transactions"] },
    { protocol: 2, compression: "none", capabilities: ["kv", "transactions"] },
    { protocol: 1, compression: "gzip", capabilities: ["kv", "transactions"] },
    { protocol: 1, compression: "none", capabilities: ["kv"] },
    { protocol: 1, compression: "none", capabilities: ["kv", "transactions", 1] }
  ];

  for (const value of invalid) {
    assert.throws(() => validateNegotiation(value, offer), /invalid protocol negotiation/);
  }
});
