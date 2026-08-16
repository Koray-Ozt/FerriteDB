# FerriteDB Python SDK

Official Python client SDK for **FerriteDB**, featuring both synchronous and asynchronous (`asyncio`) APIs, Unix domain socket transport, protocol v1 capability handshake, and automated sidecar process lifecycle management.

## Installation

```bash
pip install ferritedb
```

Requires Python 3.10 or newer. Zero external runtime dependencies (standard library only).

## Quickstart

### Synchronous Usage

```python
from ferritedb import FerriteDB, Put, Delete

# Opens or creates database and launches sidecar process
with FerriteDB.open("./app-db") as db:
    # Key-value operations
    db.put("settings/theme", {"dark": True, "fontSize": 14})
    print(db.get("settings/theme"))

    # Atomic multi-operation transaction
    db.transaction([
        Put("users/1", {"name": "Ada Lovelace", "role": "admin"}),
        Put("users/2", {"name": "Grace Hopper", "role": "engineer"}),
        Delete("temporary/session"),
    ])

    # Prefix scan
    for key, user in db.list("users/"):
        print(key, user)
```

### Asynchronous Usage (`asyncio`)

```python
import asyncio
from ferritedb import AsyncFerriteDB, Put, Delete

async def main():
    async with await AsyncFerriteDB.open("./app-db") as db:
        await db.put("sensors/temp", 23.5)
        temp = await db.get("sensors/temp")
        print("Temperature:", temp)

        await db.transaction([
            Put("metrics/requests", 1024),
            Delete("metrics/deprecated"),
        ])

        results = await db.list("metrics/")
        print("Metrics:", results)

asyncio.run(main())
```

## Features

- **Sync & Async**: First-class synchronous (`FerriteDB`) and asynchronous (`AsyncFerriteDB`) clients.
- **Context Managers**: Clean RAII resource management via `with` and `async with` ensuring sidecar process and sockets are cleanly torn down.
- **Protocol v1 Handshake**: Automatic capability negotiation (`kv`, `transactions`, `prefix-list`) and version verification over Unix domain sockets.
- **Zero Runtime Dependencies**: Built entirely using Python's standard library (`socket`, `subprocess`, `asyncio`, `json`).
- **Atomic Operations**: Typed transaction support with `Put` and `Delete` operation objects or dictionaries.
- **Safe Socket Ownership**: Verifies filesystem socket inode identity prior to unlinking to avoid race conditions.

## Options

`FerriteDB.open` and `AsyncFerriteDB.open` accept configuration options:

```python
from ferritedb import FerriteDB, OpenOptions

options = OpenOptions(
    binary="/custom/path/to/ferrite",  # Or set FERRITE_BIN environment variable
    schema="./schema.json",            # Optional JSON schema constraint
    socket="/tmp/custom-ferrite.sock", # Explicit socket path
    timeout=5.0                        # Startup deadline in seconds
)

with FerriteDB.open("./app-db", options=options) as db:
    ...
```

## Development & Testing

Run unit and integration test suites:

```bash
# Run unit tests
python3 -m unittest discover -s sdk/python/tests -v

# Run with custom Ferrite binary
FERRITE_BIN=../../target/debug/ferrite python3 -m unittest discover -s sdk/python/tests -v
```
