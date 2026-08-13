# Getting started with FerriteDB

FerriteDB is a local JSON document/key-value database. A Rust process owns the database files and exposes either short-lived command-line operations or a versioned NDJSON protocol over a Unix socket. The TypeScript SDK starts and stops that sidecar for Node.js applications.

> [!WARNING]
> FerriteDB is an unaudited public beta for Linux x86_64 and Node.js. Do not use it for production, security-critical workloads, or irreplaceable data.

## What you will build

This guide shows how to:

1. install FerriteDB with npm;
2. create and query a local database from Node.js;
3. add a collection schema and unique constraint;
4. use backup, restore, verify, export, and import;
5. connect from TypeScript through the managed sidecar.

## Requirements

The packaged beta supports Linux x86_64. Windows and macOS packages are not part of this beta.

Install:

- Node.js 20 or newer and npm.

Check the tools:

```bash
node --version
npm --version
```

## Install

Create a Node.js project and install the beta SDK. The matching Linux sidecar is installed automatically:

```bash
mkdir ferrite-example
cd ferrite-example
npm init -y
npm install @ferritedb/sdk@beta
```

Create `example.mjs`:

```js
import { FerriteDB } from "@ferritedb/sdk";

const db = await FerriteDB.open("./app-db");
try {
  await db.put("settings/theme", { dark: true, accent: "blue" });
  console.log(await db.get("settings/theme"));
} finally {
  await db.close();
}
```

```bash
node example.mjs
```

The remaining CLI sections are for contributors or manual administration. Download `ferrite-linux-x64` from the matching GitHub Release and set `FERRITE` to its path, or build from source with `cargo build -p ferrite-cli`.

## Five-minute CLI quickstart

Create a temporary workspace and choose a database path. FerriteDB creates the database directory on the first write:

```bash
WORKDIR=$(mktemp -d)
DB="$WORKDIR/app-db"
```

Write two JSON values:

```bash
"$FERRITE" put "$DB" settings/theme '{"dark":true,"accent":"blue"}'
"$FERRITE" put "$DB" users/u1 '{"name":"Ada"}'
```

Read one value:

```bash
"$FERRITE" get "$DB" settings/theme
```

Expected JSON output:

```json
{"accent":"blue","dark":true}
```

List every key/value pair:

```bash
"$FERRITE" list "$DB"
```

List only keys under a prefix:

```bash
"$FERRITE" list "$DB" users/
```

Delete a value and confirm that `get` returns `null`:

```bash
"$FERRITE" delete "$DB" users/u1
"$FERRITE" get "$DB" users/u1
```

Verify the database before relying on an operational copy:

```bash
"$FERRITE" verify "$DB"
```

A healthy database prints:

```text
ok
```

Each direct CLI command opens the database with an exclusive writer lock, performs the operation, and exits. Do not run these commands against a database currently owned by a sidecar.

## Keys and JSON values

Without a schema, a key can be any non-empty UTF-8 string that does not contain a NUL byte and fits within the 4 KiB key limit.

A common convention is:

```text
collection/id
```

Values can be any JSON value up to 1 MiB. Shell quoting matters: wrap JSON in single quotes so the shell does not consume its double quotes.

```bash
"$FERRITE" put "$DB" counters/jobs '42'
"$FERRITE" put "$DB" flags/ready 'true'
"$FERRITE" put "$DB" users/u2 '{"name":"Grace","roles":["admin"]}'
```

## Add a schema

A schema defines collections, a string primary-key field, and optional unique fields. Create `schema.json`:

```json
{
  "collections": {
    "users": {
      "primary_key": "id",
      "unique": ["email"]
    }
  }
}
```

Start a new database through the sidecar and install the schema:

```bash
SCHEMA_DB="$WORKDIR/schema-db"
SOCKET="$WORKDIR/ferrite.sock"
"$FERRITE" serve "$SCHEMA_DB" --socket "$SOCKET" --schema ./schema.json
```

The command stays in the foreground. Stop it with `Ctrl-C` after testing, or let the TypeScript SDK manage it as shown below.

When a schema is active:

- keys must use exactly `collection/id`;
- the collection must exist in the schema;
- each stored document must be a JSON object;
- its primary-key field must be a string equal to the key's `id` segment;
- values in each declared unique field cannot be repeated in that collection.

For example, this key and document agree:

```text
key:      users/u1
document: {"id":"u1","email":"ada@example.com"}
```

The schema is persisted as database metadata. Later opens load it from the database; you do not need to pass `--schema` again. Passing an incompatible schema fails without replacing the stored schema.

## Backup and restore

Stop the sidecar before backing up the database. The MVP takes an exclusive writer lock rather than making an online snapshot.

```bash
BACKUP="$WORKDIR/app-db.backup"
RESTORED="$WORKDIR/restored-db"

"$FERRITE" backup "$DB" "$BACKUP"
"$FERRITE" verify "$BACKUP"
"$FERRITE" restore "$BACKUP" "$RESTORED"
"$FERRITE" verify "$RESTORED"
"$FERRITE" list "$RESTORED"
```

FerriteDB does not overwrite an existing backup or restore destination. It prepares output in an owner-only hidden sibling staging path and atomically publishes it only after validation and sync succeed.

## Export and import JSONL

Export creates an NDJSON/JSONL file with one JSON object per line:

```bash
EXPORT="$WORKDIR/app-db.jsonl"
IMPORTED="$WORKDIR/imported-db"

"$FERRITE" export "$DB" "$EXPORT"
"$FERRITE" import "$IMPORTED" "$EXPORT"
"$FERRITE" verify "$IMPORTED"
"$FERRITE" list "$IMPORTED"
```

If the database has a schema, the first export line carries FerriteDB schema metadata. Import restores the schema before applying records, preserving primary-key and unique constraints.

Important rules:

- export, import, backup, and restore never overwrite their destination;
- an export destination must be outside the source database directory;
- import creates a new database and rejects an existing destination;
- a failed operation may leave a hidden `.ferrite-staging-*` artifact for inspection.

## Use FerriteDB from TypeScript

The SDK owns the matching sidecar process and resolves the exact-version Linux package automatically.

Create `example.mjs`:

```js
import { FerriteDB } from "@ferritedb/sdk";

const db = await FerriteDB.open("./example-db");

try {
  await db.put("settings/theme", { dark: true });

  await db.transaction([
    { Put: { key: "users/u1", value: { name: "Ada" } } },
    { Put: { key: "users/u2", value: { name: "Grace" } } }
  ]);

  console.log(await db.get("settings/theme"));
  console.log(await db.list("users/"));
} finally {
  await db.close();
}
```

Run it:

```bash
node example.mjs
```

`FerriteDB.open()`:

1. chooses a private temporary Unix-socket path unless you provide one;
2. starts the Rust binary with `serve`;
3. waits up to five seconds for startup;
4. connects over the version 1 NDJSON protocol.

`db.close()` closes the socket, terminates the SDK-owned child process with a bounded shutdown, and removes only the socket created for that SDK instance. Put `close()` in a `finally` block so normal error paths also clean up the sidecar.

### Open with a schema from TypeScript

```js
const db = await FerriteDB.open("./users-db", {
  schema: "./schema.json"
});
```

The schema file must be a non-symlink regular file no larger than 1 MiB.

## Run the sidecar manually

Most Node.js applications should use the SDK lifecycle. For protocol debugging or another language client, start the sidecar yourself:

```bash
SOCKET="$WORKDIR/manual.sock"
"$FERRITE" serve "$DB" --socket "$SOCKET"
```

The socket is created with mode `0600`. Requests and responses are newline-delimited JSON. A request must fit within 2 MiB and include protocol version `1` and a numeric request ID.

Example requests:

```json
{"version":1,"id":1,"method":"put","key":"users/u1","value":{"name":"Ada"}}
{"version":1,"id":2,"method":"get","key":"users/u1"}
{"version":1,"id":3,"method":"list","prefix":"users/"}
{"version":1,"id":4,"method":"transaction","operations":[{"Put":{"key":"a","value":1}},{"Delete":{"key":"b"}}]}
```

The sidecar supports at most 64 active connection workers, applies a 30-second read timeout, and serializes database operations through one process-local lock.

## Common errors

### `database is locked`

Another FerriteDB process owns the database. Stop the sidecar or wait for the other direct command to finish. The beta permits one writer process per database.

### `socket path already exists`

FerriteDB does not delete an existing path because it may belong to another process or the user. Choose a different socket path or remove a stale socket only after verifying that no live sidecar owns it.

### `import destination already exists`

Import and restore are no-overwrite operations. Choose a new path. FerriteDB intentionally does not merge an import into an existing database.

### `WAL exceeds 64 MiB`

Run `"$FERRITE" checkpoint "$DB"` while no sidecar owns the database. Do not manually edit or truncate the WAL.

### `corrupt WAL: truncated ...`

FerriteDB fails closed and does not repair a physically truncated WAL in place. Preserve the original database and restore a verified backup. Do not continue writing to the damaged database.

### A schema write is rejected

Check that the key uses `collection/id`, the document is an object, the primary-key field equals `id`, and unique fields do not duplicate another document.

## Limits and safety boundary

The beta currently enforces:

| Resource | Limit |
| --- | ---: |
| Key | 4 KiB |
| JSON value | 1 MiB |
| Stored schema | 1 MiB |
| Operations per transaction | 1,024 |
| Encoded transaction data | 8 MiB |
| WAL file | 64 MiB |
| Sidecar request | 2 MiB |
| Active sidecar workers | 64 |
| Sidecar read timeout | 30 seconds |

FerriteDB is local-only and opens no TCP listener. It does not provide authentication, encryption at rest, SQL, replication, cloud backup, telemetry, automatic updates, or Windows transport. Recovery reads the bounded WAL into memory; it is not streaming. Process-kill boundaries are tested, but physical power-cut testing and an independent security audit have not been completed.

## Verify your checkout

Run the repository quality gates before making changes:

```bash
cargo fmt --all --check
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings

cd sdk/typescript
npm ci --include=dev
npm run build
FERRITE_BIN=../../target/debug/ferrite npm test
FERRITE_BIN=../../target/debug/ferrite npm run e2e
```

See also:

- [README](../README.md) for the project overview and current limitations;
- [Contributing](../CONTRIBUTING.md) for development expectations;
- [Security policy](../SECURITY.md) for vulnerability reporting.
