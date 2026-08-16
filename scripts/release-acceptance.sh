#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
DIST=$(mktemp -d)
APP=$(mktemp -d)
SDK_STAGE=$(mktemp -d)
cleanup() {
  rm -rf "$DIST" "$APP" "$SDK_STAGE"
  rm -rf "$ROOT/packages/linux-x64/bin"
  rm -f "$ROOT/packages/linux-x64/LICENSE" "$ROOT/sdk/typescript/LICENSE" "$ROOT/sdk/typescript/README.md"
}
trap cleanup EXIT

cargo build --release -p ferrite-cli --locked --manifest-path "$ROOT/Cargo.toml"
install -Dm755 "$ROOT/target/release/ferrite" "$ROOT/packages/linux-x64/bin/ferrite"
install -m644 "$ROOT/LICENSE" "$ROOT/packages/linux-x64/LICENSE"
install -m644 "$ROOT/LICENSE" "$ROOT/sdk/typescript/LICENSE"
install -m644 "$ROOT/README.md" "$ROOT/sdk/typescript/README.md"

npm ci --include=dev --ignore-scripts --prefix "$ROOT/sdk/typescript"
npm run build --prefix "$ROOT/sdk/typescript"

(cd "$ROOT/packages/linux-x64" && npm pack --pack-destination "$DIST")
LINUX_PACKAGE=$(printf '%s\n' "$DIST"/ferritedb-linux-x64-*.tgz)

cp -R "$ROOT/sdk/typescript/dist" "$SDK_STAGE/dist"
cp "$ROOT/sdk/typescript/package.json" "$ROOT/README.md" "$ROOT/LICENSE" "$SDK_STAGE/"
python3 - "$SDK_STAGE/package.json" "$LINUX_PACKAGE" <<'PY'
import json
import pathlib
import sys
path = pathlib.Path(sys.argv[1])
data = json.loads(path.read_text())
data["optionalDependencies"]["@ferritedb/linux-x64"] = f"file:{sys.argv[2]}"
path.write_text(json.dumps(data, indent=2) + "\n")
PY
(cd "$SDK_STAGE" && npm pack --pack-destination "$DIST")
SDK_PACKAGE=$(printf '%s\n' "$DIST"/ferritedb-sdk-*.tgz)

cd "$APP"
npm init -y >/dev/null
npm install --ignore-scripts "$SDK_PACKAGE"
node --input-type=module <<'JS'
import { mkdtemp, rm } from "node:fs/promises";
import { spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { FerriteDB } from "@ferritedb/sdk";

const require = createRequire(import.meta.url);
const binary = require.resolve("@ferritedb/linux-x64/bin/ferrite");
const root = await mkdtemp(join(tmpdir(), "ferrite-acceptance-"));
try {
  const path = join(root, "db");
  const backup = join(root, "backup");
  const restored = join(root, "restored");
  let db = await FerriteDB.open(path);
  await db.transaction([
    { Put: { key: "users/1", value: { name: "Ada" } } },
    { Put: { key: "users/2", value: { name: "Grace" } } }
  ]);
  await db.close();
  db = await FerriteDB.open(path);
  if ((await db.get("users/1"))?.name !== "Ada") throw new Error("restart verification failed");
  await db.close();
  for (const args of [
    ["checkpoint", path],
    ["backup", path, backup],
    ["restore", backup, restored],
    ["verify", restored]
  ]) {
    const result = spawnSync(binary, args, { encoding: "utf8" });
    if (result.status !== 0) throw new Error(`${args[0]} failed: ${result.stderr}`);
  }
  db = await FerriteDB.open(restored);
  if ((await db.get("users/2"))?.name !== "Grace") throw new Error("restore verification failed");
  await db.close();
  console.log("release acceptance passed (typescript)");
} finally {
  await rm(root, { recursive: true, force: true });
}
JS

PYTHONPATH="$ROOT/sdk/python/src" python3 - <<PY
import os
import shutil
import tempfile
from ferritedb import FerriteDB, Put, Delete

root = tempfile.mkdtemp(prefix="ferrite-py-acceptance-")
try:
    db_path = os.path.join(root, "db")
    with FerriteDB.open(db_path, binary="$ROOT/target/release/ferrite") as db:
        db.put("status", {"ok": True})
        assert db.get("status") == {"ok": True}
        db.transaction([
            Put("k1", "v1"),
            Put("k2", "v2"),
            Delete("status"),
        ])
        assert db.get("status") is None
        assert db.get("k1") == "v1"
        assert len(db.list()) == 2
    
    with FerriteDB.open(db_path, binary="$ROOT/target/release/ferrite") as reopened:
        assert reopened.get("k2") == "v2"
        assert reopened.get("status") is None
    print("release acceptance passed (python)")
finally:
    shutil.rmtree(root, ignore_errors=True)
PY
