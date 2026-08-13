# Changelog

All notable changes to FerriteDB are documented here.

## 0.1.0-beta.1

### Added

- Linux x86_64 public beta distribution through `@ferritedb/sdk` and an exact-version platform sidecar package.
- Versioned format 1 manifest and fail-closed handling of unversioned or unknown database formats.
- Backup-first, non-destructive alpha-to-beta migration command.
- Crash-safe manual checkpoint command with bounded recovery transactions.
- Process-kill coverage around WAL begin/write/commit/sync and checkpoint publication boundaries.
- Clean-package release acceptance, dependency audits, checksums, and tag-driven release workflow.
- FSL-1.1-ALv2 source-available license.

### Compatibility

Alpha data is not opened implicitly. Use `ferrite migrate SOURCE DEST --backup BACKUP`. Later beta versions must read format 1 or provide a tested migration.

### Known limitations

This beta is unaudited and is not intended for production, security-critical workloads, or irreplaceable data. Only Linux x86_64 and Node.js are supported. Checkpointing is manual, operations are serialized, and physical power-cut behavior has not been tested.
