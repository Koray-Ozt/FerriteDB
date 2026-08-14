export interface ProtocolNegotiation {
  protocol: number;
  compression: string;
  capabilities: string[];
}

export interface ProtocolOffer {
  protocol: { min: number; max: number };
  compression: readonly string[];
  capabilities: {
    required: readonly string[];
    optional: readonly string[];
  };
}

export function validateNegotiation(value: unknown, offer: ProtocolOffer): ProtocolNegotiation {
  if (typeof value !== "object" || value === null) {
    throw new Error("invalid protocol negotiation: expected an object");
  }

  const result = value as Record<string, unknown>;
  if (!Number.isSafeInteger(result.protocol)
    || (result.protocol as number) < offer.protocol.min
    || (result.protocol as number) > offer.protocol.max) {
    throw new Error("invalid protocol negotiation: protocol was not offered");
  }
  if (typeof result.compression !== "string" || !offer.compression.includes(result.compression)) {
    throw new Error("invalid protocol negotiation: compression was not offered");
  }
  if (!Array.isArray(result.capabilities)
    || !result.capabilities.every(capability => typeof capability === "string")) {
    throw new Error("invalid protocol negotiation: capabilities must be strings");
  }

  const capabilities = result.capabilities as string[];
  if (!offer.capabilities.required.every(capability => capabilities.includes(capability))) {
    throw new Error("invalid protocol negotiation: required capability missing");
  }
  const offeredCapabilities = new Set([
    ...offer.capabilities.required,
    ...offer.capabilities.optional
  ]);
  if (!capabilities.every(capability => offeredCapabilities.has(capability))) {
    throw new Error("invalid protocol negotiation: capability was not offered");
  }

  return {
    protocol: result.protocol as number,
    compression: result.compression,
    capabilities: [...capabilities]
  };
}
