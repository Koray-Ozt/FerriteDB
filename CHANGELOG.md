# Changelog

All notable changes to FerriteDB are documented here.

## 0.2.0-beta.1

### Added

- Fixed-page storage engine (`pager.rs`): 4 KiB and 8 KiB page layouts, `FERRITE\0` binary header metadata, LIFO free-list recycling, RAII page reference guards, and crash-safe WAL sequence synchronization (#5).
- Slotted-page frame architecture (`slotted_page.rs`): forward slot directory, backward payload packing, tombstone slot reuse, compact-on-write defragmentation, and multi-page overflow records up to 64 KiB (#6).
- Memory-bounded Buffer Pool Manager (`buffer_pool.rs`): configurable RAM budget (default 64 MiB / 16,384 frames), second-chance `CLOCK` and `LRU-K` eviction policies, atomic pin counting, and strict WAL-pinned dirty page eviction invariant (#7).
- Continuous performance & latency regression suite (`workload.rs`, `bench_metrics.rs`, `ferrite-bench` CLI, `benches/`): Criterion benchmarks for core storage layers, full YCSB Workload A–F generator with Uniform/Zipfian/Latest distributions, microsecond latency percentile histograms, WAL write amplification analysis, and CI regression gate (#8).

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
