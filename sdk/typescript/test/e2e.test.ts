import { strict as assert } from "node:assert";
import { existsSync } from "node:fs";
import { mkdtemp, readFile, rm, unlink } from "node:fs/promises";
import { once } from "node:events";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import test from "node:test";
import { FerriteDB } from "../src/index.js";

test("Node sidecar transaction persists to disk", async () => {
  const root = await mkdtemp(join(tmpdir(), "ferrite-sdk-"));
  const binary = resolve(process.env.FERRITE_BIN ?? "../../target/debug/ferrite");
  const socket = join(root, "sidecar.sock");
  const db = await FerriteDB.open(root, { binary, socket });
  await db.transaction([
    { Put: { key: "users/1", value: { name: "Ada" } } },
    { Put: { key: "users/2", value: { name: "Grace" } } }
  ]);
  assert.deepEqual(await db.get("users/1"), { name: "Ada" });
  assert.equal((await db.list("users/")).length, 2);
  await db.delete("users/2");
  await db.close();
  assert.equal(existsSync(socket), false);
  assert.equal((await readFile(join(root, "data.wal"))).subarray(0, 8).toString(), "FRTWAL01");
  const reopened = await FerriteDB.open(root, { binary });
  assert.equal(await reopened.get("users/2"), null);
  await reopened.close();
  await rm(root, { recursive: true, force: true });
});

test("a missing sidecar binary rejects cleanly", async () => {
  const root = await mkdtemp(join(tmpdir(), "ferrite-sdk-missing-"));
  const socket = join(root, "missing.sock");

  await assert.rejects(
    FerriteDB.open(join(root, "db"), {
      binary: join(root, "does-not-exist"),
      socket
    }),
    /failed to start|ENOENT/
  );
  assert.equal(existsSync(socket), false);
  await rm(root, { recursive: true, force: true });
});

test("a pre-existing socket path is rejected without deleting it", async () => {
  const root = await mkdtemp(join(tmpdir(), "ferrite-sdk-existing-"));
  const socket = join(root, "do-not-delete");
  const { writeFile } = await import("node:fs/promises");
  await writeFile(socket, "owned by user");

  await assert.rejects(
    FerriteDB.open(join(root, "db"), { socket }),
    /socket path already exists/
  );
  assert.equal(await readFile(socket, "utf8"), "owned by user");
  await rm(root, { recursive: true, force: true });
});

test("the packaged Linux sidecar is discovered without a binary option", {
  skip: process.env.FERRITE_PACKAGED_TEST !== "1"
}, async () => {
  const root = await mkdtemp(join(tmpdir(), "ferrite-sdk-packaged-"));
  const previous = process.env.FERRITE_BIN;
  delete process.env.FERRITE_BIN;
  try {
    const db = await FerriteDB.open(join(root, "db"));
    await db.put("packaged", { works: true });
    assert.deepEqual(await db.get("packaged"), { works: true });
    await db.close();
  } finally {
    if (previous === undefined) delete process.env.FERRITE_BIN;
    else process.env.FERRITE_BIN = previous;
    await rm(root, { recursive: true, force: true });
  }
});

test("close preserves a replacement socket path it does not own", async () => {
  const root = await mkdtemp(join(tmpdir(), "ferrite-sdk-socket-race-"));
  const socket = join(root, "db.sock");
  const binary = resolve(process.env.FERRITE_BIN ?? "../../target/debug/ferrite");
  const db = await FerriteDB.open(join(root, "db"), { binary, socket });
  await unlink(socket);

  const replacement = createServer();
  replacement.listen(socket);
  await once(replacement, "listening");
  try {
    await db.close();
    assert.equal(existsSync(socket), true);
  } finally {
    replacement.close();
    await once(replacement, "close");
    await rm(root, { recursive: true, force: true });
  }
});
