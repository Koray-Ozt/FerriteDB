<p align="center">
  <img src="docs/assets/ferritedb-banner.svg" alt="FerriteDB — local-first database engine built in Rust" width="100%" />
</p>

<p align="center"><strong>A small local JSON document/key-value database with explicit durable commits and a language-neutral sidecar.</strong></p>

> [!WARNING]
> FerriteDB is an **unaudited public beta** for Linux x86_64 and Node.js. Do not use it for production, security-critical workloads, or irreplaceable data.

## Beta capabilities

- persistent JSON key/value records and atomic multi-operation put/delete transactions;
- checksummed WAL, committed-only recovery, explicit corruption failures, and bounded untrusted lengths;
- optional JSON collection schema with string primary keys and unique fields, rebuilt and checked on restart;
- local-only version 1 NDJSON protocol over Unix domain sockets with a mandatory semantic capability handshake;
- direct `put`, `get`, `delete`, `list`, `verify`, `backup`, `restore`, `export`, and `import` commands;
- dependency-light TypeScript SDK that starts and owns the Rust sidecar.
- crash-safe manual WAL checkpointing and an explicit, backup-first alpha-to-beta migration command.

## Build and try it

The easiest installation is a single command; no Rust toolchain or separate sidecar setup is required:

```bash
npm install @ferritedb/sdk@beta
```

For a complete walkthrough—from the first write through schemas, checkpoint, backup/restore, JSONL portability, and the CLI—see **[Getting started with FerriteDB](docs/GETTING_STARTED.md)**.

```bash
cargo build -p ferrite-cli
DB=$(mktemp -d)/example
./target/debug/ferrite put "$DB" greeting '{"text":"hello"}'
./target/debug/ferrite get "$DB" greeting
./target/debug/ferrite list "$DB"
./target/debug/ferrite verify "$DB"
./target/debug/ferrite export "$DB" export.jsonl
```

Commands never overwrite backup, restore, export, or import destinations. Backup, restore, export, and import are built in owner-only hidden sibling staging paths and published with an atomic no-replace rename only after verification/sync succeeds. Export destinations must be outside the source database directory. A crash or validation failure can leave a hidden `.ferrite-staging-*` artifact for manual inspection, but the requested destination is never exposed as complete and cleanup never recursively deletes a path that another process may have replaced. A database directory is created if absent. Commits and newly created database metadata are fsynced before success. Stored schema metadata must be a non-symlink regular file no larger than 1 MiB and is opened with bounded, non-blocking reads. JSONL exports include stored schema metadata as the first line, and import restores it before applying records so primary-key and unique constraints survive a round trip.

A schema is JSON:

```json
{
  "collections": {
    "users": { "primary_key": "id", "unique": ["email"] }
  }
}
```

Start the sidecar with it:

```bash
./target/debug/ferrite serve ./app-db --socket /tmp/ferrite.sock --schema ./schema.json
```

Each connection starts with a `hello` request that negotiates the highest mutually supported protocol, compression mode, and required/optional capabilities. Version 1 supports `none` compression and the `kv`, `transactions`, and `prefix-list` capabilities. Requests sent before a successful handshake are rejected. See [PROTOCOL.md](docs/PROTOCOL.md) for the compatibility contract.

The negotiated protocol then accepts one JSON object per line and returns one response per line:

```json
{"version":1,"id":0,"method":"hello","protocol":{"min":1,"max":1},"compression":["none"],"capabilities":{"required":["kv","transactions"],"optional":["prefix-list"]}}
{"version":1,"id":1,"method":"put","key":"users/u1","value":{"id":"u1","email":"ada@example.com"}}
{"version":1,"id":2,"method":"transaction","operations":[{"Put":{"key":"a","value":1}},{"Delete":{"key":"b"}}]}
```

## TypeScript SDK

```bash
cd sdk/typescript
npm ci --include=dev
npm run build
```

```ts
import { FerriteDB } from "@ferritedb/sdk";

const db = await FerriteDB.open("./app-db");
await db.put("settings/theme", { dark: true });
console.log(await db.get("settings/theme"));
await db.transaction([
  { Put: { key: "counter", value: 1 } },
  { Delete: { key: "obsolete" } }
]);
await db.close();
```

## Verify the repository

```bash
cargo fmt --all --check
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cd sdk/typescript
npm run build
FERRITE_BIN=../../target/debug/ferrite npm test
FERRITE_BIN=../../target/debug/ferrite npm run e2e
```

## Limits and limitations

The beta limits keys to 4 KiB, JSON values and WAL records to about 1 MiB, transactions to 1,024 operations/8 MiB, and a WAL file to 64 MiB between manual checkpoints. The database uses an in-memory ordered map rebuilt from the WAL at startup; it is not a page store or B+ tree. An advisory file lock enforces one writer process per database. The sidecar handles at most 64 clients in connection workers with a 30-second read timeout while serializing database operations through one process-local lock.

The packaged beta supports **Linux x86_64 only**. The source can build on macOS, but macOS is not a supported beta distribution target. There is no Windows transport, remote TCP, authentication, encryption, SQL, distributed replication, telemetry, or cloud service. Backup holds the source writer lock for the complete verified copy and therefore rejects a running writer instead of producing a concurrent snapshot. Fully written but uncommitted WAL transactions are ignored during recovery; a physically truncated WAL tail is reported as corruption and is never repaired in place. Process-kill boundaries are covered by automated tests; durability has not received physical power-cut testing or an independent security audit.

## Layout

- `crates/ferrite-core`: database, schema validation, constraints, WAL and recovery
- `crates/ferrite-cli`: direct commands and local Unix-socket sidecar
- `sdk/typescript`: Node.js SDK and real sidecar-to-disk e2e test

Beta format 1 databases are identified by `format.json`. Future beta versions must read format 1 or provide a tested migration. Alpha databases without a manifest are not opened implicitly; use `ferrite migrate SOURCE DEST --backup BACKUP` to create an untouched backup and a separately verified beta copy. See [FORMAT.md](docs/FORMAT.md), [THREAT_MODEL.md](docs/THREAT_MODEL.md), [CONTRIBUTING.md](CONTRIBUTING.md), and [SECURITY.md](SECURITY.md).

FerriteDB is licensed under [FSL-1.1-ALv2](LICENSE), which permits internal use and redistribution that does not compete with FerriteDB, and converts each release to Apache-2.0 after two years. This is not legal advice; evaluate the license for your use case.
