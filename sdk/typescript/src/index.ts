import { spawn, type ChildProcess } from "node:child_process";
import { randomUUID } from "node:crypto";
import { once } from "node:events";
import { existsSync } from "node:fs";
import { lstat, rm } from "node:fs/promises";
import { createConnection, type Socket } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";

export type Operation =
  | { Put: { key: string; value: unknown } }
  | { Delete: { key: string } };

export interface OpenOptions { binary?: string; schema?: string; socket?: string }

type Pending = { resolve(value: unknown): void; reject(error: Error): void };

async function removeSocket(path: string): Promise<void> {
  try {
    if ((await lstat(path)).isSocket()) {
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
  private constructor(private readonly child: ChildProcess, private readonly socket: Socket, private readonly socketPath: string) {
    socket.setEncoding("utf8");
    socket.on("data", (chunk: string) => this.receive(chunk));
    socket.on("error", (error) => this.rejectAll(error));
    child.on("exit", (code) => this.rejectAll(new Error(`FerriteDB sidecar exited (${code ?? "signal"})`)));
  }

  static async open(path: string, options: OpenOptions = {}): Promise<FerriteDB> {
    const binary = options.binary ?? process.env.FERRITE_BIN ?? "ferrite";
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
    try {
      while (!existsSync(socketPath)) {
        if (spawnError) throw new Error(`FerriteDB failed to start: ${spawnError.message}`);
        if (child.exitCode !== null) throw new Error(`FerriteDB failed to start: ${stderr}`);
        if (Date.now() >= deadline) throw new Error("FerriteDB sidecar startup timed out");
        await new Promise(resolve => setTimeout(resolve, 10));
      }
      const socket = createConnection(socketPath);
      await once(socket, "connect");
      return new FerriteDB(child, socket, socketPath);
    } catch (error) {
      await terminateChild(child);
      await removeSocket(socketPath);
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
    await removeSocket(this.socketPath);
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
