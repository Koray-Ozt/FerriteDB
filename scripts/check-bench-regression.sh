#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BASELINE="$ROOT/docs/benchmarks/baseline.json"

if [ ! -f "$BASELINE" ]; then
    echo "Error: Baseline file not found at $BASELINE"
    exit 1
fi

echo "==> Running FerriteDB Performance & Regression Suite Check..."
cargo run --release -p ferrite-core --bin ferrite-bench --manifest-path "$ROOT/Cargo.toml" -- \
    --check "$BASELINE" \
    --threshold 15.0

echo "==> All performance regression checks passed!"
