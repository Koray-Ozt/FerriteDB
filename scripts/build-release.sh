#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-dist}
case "$OUT" in
  /*) ;;
  *) OUT="$ROOT/$OUT" ;;
esac
rm -rf "$OUT"
mkdir -p "$OUT"

cargo build --release -p ferrite-cli --locked --manifest-path "$ROOT/Cargo.toml"
mkdir -p "$ROOT/packages/linux-x64/bin"
install -m755 "$ROOT/target/release/ferrite" "$ROOT/packages/linux-x64/bin/ferrite"
install -m644 "$ROOT/LICENSE" "$ROOT/packages/linux-x64/LICENSE"
install -m644 "$ROOT/LICENSE" "$ROOT/sdk/typescript/LICENSE"
install -m644 "$ROOT/README.md" "$ROOT/sdk/typescript/README.md"
trap 'rm -rf "$ROOT/packages/linux-x64/bin"; rm -f "$ROOT/packages/linux-x64/LICENSE" "$ROOT/sdk/typescript/LICENSE" "$ROOT/sdk/typescript/README.md"' EXIT

npm ci --include=dev --ignore-scripts --prefix "$ROOT/sdk/typescript"
npm run build --prefix "$ROOT/sdk/typescript"
(cd "$ROOT/packages/linux-x64" && npm pack --pack-destination "$OUT")
(cd "$ROOT/sdk/typescript" && npm pack --pack-destination "$OUT")
cp "$ROOT/target/release/ferrite" "$OUT/ferrite-linux-x64"
chmod 0755 "$OUT/ferrite-linux-x64"
(
  cd "$OUT"
  sha256sum ferrite-linux-x64 ferritedb-linux-x64-*.tgz ferritedb-sdk-*.tgz > SHA256SUMS
)
