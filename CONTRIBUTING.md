# Contributing

FerriteDB is currently establishing its storage and recovery invariants. Focused changes with explicit tests are preferred over broad abstractions.

## Development

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

For behavior changes, add a focused failing test first, implement the smallest coherent change, then run the full workspace checks.

## Pull requests

- Keep each pull request scoped to one behavior or architectural boundary.
- Document changes to durability, recovery, or the on-disk format explicitly.
- Do not claim format compatibility until the project declares a stable-format milestone.
- Avoid new dependencies unless they materially reduce correctness or maintenance risk.
