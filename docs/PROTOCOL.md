# FerriteDB local protocol

FerriteDB uses newline-delimited JSON over a local Unix domain socket. Every connection must complete one `hello` exchange before database methods are accepted.

## Handshake

The client sends its inclusive protocol range, ordered compression preferences, required capabilities, and optional capabilities:

```json
{"version":1,"id":1,"method":"hello","protocol":{"min":1,"max":1},"compression":["none"],"capabilities":{"required":["kv","transactions"],"optional":["prefix-list"]}}
```

A compatible sidecar selects one protocol and compression mode. The returned capabilities are the supported intersection of the client's required and optional capabilities, in stable server order:

```json
{"version":1,"id":1,"ok":true,"result":{"protocol":1,"compression":"none","capabilities":["kv","transactions","prefix-list"]}}
```

The handshake is connection-scoped and may succeed only once. Failed negotiation does not activate the connection; the client may correct its offer and retry `hello`. Non-handshake requests before success are rejected with `hello handshake required`.

## Version 1 capabilities

| Capability | Methods |
| --- | --- |
| `kv` | `put`, `get`, `delete` |
| `transactions` | `transaction` |
| `prefix-list` | `list`, including optional prefix filtering |

Version 1 supports only the `none` compression mode. This means NDJSON frames are sent uncompressed.

## Compatibility matrix

| Client offer | Result |
| --- | --- |
| Range includes protocol 1, compression includes `none`, all required capabilities supported | Select protocol 1, `none`, and the supported capability intersection |
| Protocol range does not include 1 or has `min > max` | `incompatible protocol versions` |
| Compression list does not include `none` | `no mutually supported compression` |
| A required capability is unknown | `unsupported required capability: <name>` |
| Only an optional capability is unknown | Handshake succeeds and omits that capability |
| A required handshake field is absent, has the wrong type, or an array contains a non-string value | Reject the handshake with a field-specific error; the connection remains unnegotiated |

The protocol range is inclusive, so a wider range such as `0..2` selects protocol `1`. Compression preferences are ordered by the client, but the sidecar may select any mutually supported mode; version 1 supports only `none`, so an offer such as `["gzip", "none"]` selects `none`. Empty compression lists are incompatible.

The top-level `version` remains the frame format discriminator. Protocol 1 requires it to equal `1` on the handshake and all subsequent requests. Future sidecars may add protocol versions and capabilities, but must select only values offered by the client and must reject unsupported required capabilities rather than silently degrading behavior.
