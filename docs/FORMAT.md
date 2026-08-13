# FerriteDB beta format policy

## Format 1

Every beta database contains `format.json` with `{ "format": 1 }`. The manifest, `schema.json`, and `data.wal` are reserved database metadata. Metadata is opened as a bounded, non-followed regular file where applicable.

The WAL magic remains `FRTWAL01`. Format 1 adds an explicit database-level manifest so a future binary can reject unknown formats before interpreting data.

## Compatibility promise

- Alpha databases without `format.json` are not opened implicitly.
- Every later beta release must either read format 1 directly or ship a documented, tested migration command.
- Patch releases in the `0.1.x` beta line do not intentionally break the SDK API or format 1.
- A migration never modifies its source in place.

## Alpha migration

```bash
ferrite migrate ./alpha-db ./beta-db --backup ./alpha-db-backup
```

The destination and backup must be distinct siblings of the source database. The command opens that parent directory once, obtains the source writer lock, publishes an untouched backup, validates the legacy WAL, builds a versioned copy in an owner-only sibling staging directory, verifies it, and atomically publishes it without replacing an existing destination. Migration never changes the source WAL or user data; it may create the persistent `.ferrite.lock` coordination file used by every writer. If validation or publication fails, the source data remains unchanged.

## Checkpoint

```bash
ferrite checkpoint ./beta-db
```

Checkpoint rewrites current committed state into a synced sibling WAL, atomically replaces `data.wal`, syncs the database directory, and continues with a new WAL handle. Process-kill tests cover pre-publication and post-rename boundaries. Physical power-cut behavior has not been tested.
