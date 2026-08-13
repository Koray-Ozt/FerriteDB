import { spawn, type ChildProcess } from "node:child_process";
import { randomUUID } from "node:crypto";
import { once } from "node:events";
import { existsSync } from "node:fs";
import { lstat, rm } from "node:fs/promises";
import { createConnection, type Socket } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";

export type Operation =
  | { Put: { key: string; value: unknown } }
  | { Delete: { key: string } };

export interface OpenOptions { binary?: string; schema?: string; socket?: string }

type Pending = { resolve(value: unknown): void; reject(error: Error): void };
type SocketIdentity = { dev: number; ino: number };

const require = createRequire(import.meta.url);

function defaultBinary(): string {
  if (process.platform !== "linux" || process.arch !== "x64") {
    throw new Error(`FerriteDB does not support ${process.platform}-${process.arch} in this beta`);
  }
  try {
    return require.resolve("@ferritedb/linux-x64/bin/ferrite");
  } catch (error) {
    throw new Error("FerriteDB Linux sidecar package is missing; reinstall @ferritedb/sdk", { cause: error });
  }
}

async function socketIdentity(path: string): Promise<SocketIdentity> {
  const stats = await lstat(path);
  if (!stats.isSocket()) throw new Error(`FerriteDB socket path is not a socket: ${path}`);
  return { dev: stats.dev, ino: stats.ino };
}

async function removeSocket(path: string, identity: SocketIdentity): Promise<void> {
  try {
    const current = await lstat(path);
    if (current.isSocket() && current.dev === identity.dev && current.ino === identity.ino) {
      await rm(path);
    }
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
  }
}

async function waitForExit(child: ChildProcess, timeoutMs: number): Promise<boolean> {
  if (child.exitCode !== null || child.signalCode !== null) return true;
  return new Promise(resolve => {
    const timer = setTimeout(() => {
      child.off("exit", stopped);
      child.off("close", stopped);
      resolve(false);
    }, timeoutMs);
    const stopped = (): void => {
      clearTimeout(timer);
      child.off("exit", stopped);
      child.off("close", stopped);
      resolve(true);
    };
    child.once("exit", stopped);
    child.once("close", stopped);
  });
}

async function terminateChild(child: ChildProcess): Promise<void> {
  if (child.pid === undefined || child.exitCode !== null || child.signalCode !== null) return;
  child.kill();
  if (await waitForExit(child, 1000)) return;

  child.kill("SIGKILL");
  if (!(await waitForExit(child, 1000))) {
    throw new Error("FerriteDB sidecar did not terminate");
  }
}

export class FerriteDB {
  private nextId = 1;
  private buffer = "";
  private readonly pending = new Map<number, Pending>();
  private constructor(private readonly child: ChildProcess, private readonly socket: Socket, private readonly socketPath: string, private readonly socketIdentity: SocketIdentity) {
    socket.setEncoding("utf8");
    socket.on("data", (chunk: string) => this.receive(chunk));
    socket.on("error", (error) => this.rejectAll(error));
    child.on("exit", (code) => this.rejectAll(new Error(`FerriteDB sidecar exited (${code ?? "signal"})`)));
  }

  static async open(path: string, options: OpenOptions = {}): Promise<FerriteDB> {
    const binary = options.binary ?? process.env.FERRITE_BIN ?? defaultBinary();
    const socketPath = options.socket ?? join(tmpdir(), `ferrite-${process.pid}-${randomUUID()}.sock`);
    if (existsSync(socketPath)) {
      throw new Error(`FerriteDB socket path already exists: ${socketPath}`);
    }
    const args = ["serve", path, "--socket", socketPath];
    if (options.schema) args.push("--schema", options.schema);
    const child = spawn(binary, args, { stdio: ["ignore", "ignore", "pipe"] });
    let stderr = "";
    let spawnError: Error | undefined;
    child.once("error", error => { spawnError = error; });
    child.stderr?.setEncoding("utf8"); child.stderr?.on("data", chunk => { stderr += String(chunk); });
    const deadline = Date.now() + 5000;
    let identity: SocketIdentity | undefined;
    try {
      while (!existsSync(socketPath)) {
        if (spawnError) throw new Error(`FerriteDB failed to start: ${spawnError.message}`);
        if (child.exitCode !== null) throw new Error(`FerriteDB failed to start: ${stderr}`);
        if (Date.now() >= deadline) throw new Error("FerriteDB sidecar startup timed out");
        await new Promise(resolve => setTimeout(resolve, 10));
      }
      identity = await socketIdentity(socketPath);
      const socket = createConnection(socketPath);
      await once(socket, "connect");
      return new FerriteDB(child, socket, socketPath, identity);
    } catch (error) {
      await terminateChild(child);
      if (identity) await removeSocket(socketPath, identity);
      throw error;
    }
  }

  put(key: string, value: unknown): Promise<void> { return this.request("put", { key, value }) as Promise<void>; }
  get<T = unknown>(key: string): Promise<T | null> { return this.request("get", { key }) as Promise<T | null>; }
  delete(key: string): Promise<void> { return this.request("delete", { key }) as Promise<void>; }
  list<T = unknown>(prefix?: string): Promise<Array<[string, T]>> { return this.request("list", prefix === undefined ? {} : { prefix }) as Promise<Array<[string, T]>>; }
  transaction(operations: Operation[]): Promise<void> { return this.request("transaction", { operations }) as Promise<void>; }

  async close(): Promise<void> {
    this.socket.end();
    await terminateChild(this.child);
    await removeSocket(this.socketPath, this.socketIdentity);
  }

  private request(method: string, fields: Record<string, unknown>): Promise<unknown> {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.socket.write(`${JSON.stringify({ version: 1, id, method, ...fields })}\n`, error => {
        if (error) { this.pending.delete(id); reject(error); }
      });
    });
  }

  private receive(chunk: string): void {
    this.buffer += chunk;
    for (;;) {
      const newline = this.buffer.indexOf("\n");
      if (newline < 0) return;
      const line = this.buffer.slice(0, newline); this.buffer = this.buffer.slice(newline + 1);
      const response = JSON.parse(line) as { id: number; ok: boolean; result?: unknown; error?: string };
      const pending = this.pending.get(response.id); if (!pending) continue;
      this.pending.delete(response.id);
      if (response.ok) pending.resolve(response.result); else pending.reject(new Error(response.error ?? "FerriteDB error"));
    }
  }
  private rejectAll(error: Error): void { for (const pending of this.pending.values()) pending.reject(error); this.pending.clear(); }
}
