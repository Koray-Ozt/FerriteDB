# FerriteDB beta threat model

## Scope

FerriteDB is a local sidecar for a single OS user. The beta protects database integrity against malformed local inputs, process crashes at tested boundaries, accidental path reuse, and untrusted protocol frames within documented limits.

The `0.1.0-beta.1` distribution targets Linux x86_64. Atomic migration publication relies on Linux directory file descriptors, `/proc/self/fd`, and `renameat2(RENAME_NOREPLACE)` so pathname checks and publication stay bound to one directory identity.

## Trust boundaries

- Database, schema, import, export, backup, restore, migration, and socket paths are caller-controlled.
- WAL, format, schema, and JSONL content may be malformed or truncated.
- NDJSON requests are untrusted even though the socket is local.
- NPM packages and GitHub release artefacts cross a software-supply-chain boundary.

## Controls

- Local Unix socket mode is `0600`; no TCP listener exists.
- Request lines, values, keys, records, WAL files, metadata, operation counts, and worker counts have explicit limits.
- Checksummed WAL recovery fails closed on malformed structure or unknown records.
- Database metadata uses bounded reads and rejects symlinks or non-regular files where applicable.
- Backup, restore, import, export, migration, and checkpoint use owner-only staging and atomic publication patterns; user-visible destinations are not overwritten.
- SDK and sidecar versions are exact-matched in beta packages.
- CI runs Rust and NPM dependency audits, malformed input tests, crash tests, and clean-package acceptance.

## Non-goals for the beta

- Protection from a malicious process running as the same OS user.
- Encryption at rest, key management, authentication, authorization, or network isolation.
- Physical power-cut guarantees, kernel/filesystem defects, malicious storage hardware, or memory corruption.
- Multi-tenant isolation or safe exposure of the Unix socket to untrusted users.
- Production or security-critical use.

## Residual risks

The engine rebuilds an in-memory map from a bounded whole WAL and is not a streaming page store. Manual checkpointing is required before the 64 MiB WAL limit. The project has not received an independent security audit. A process killed after a commit record enters the OS cache but before `sync_all` returns may reopen to either the complete old state or the complete new state; partial transactions remain invisible.
