// The bridge's link to figmog (NWS-2957): one `figmog serve` stdio relay,
// started on first use in the project root. The relay is the same process an
// agent's MCP client starts (spec §6b): it finds (or launches) the
// repository's background owner, runs the handshake, injects this worktree's
// default file and the project's spec settings, and forwards line-delimited
// JSON-RPC frames to the owner's socket. Reusing it keeps every one of those
// rules in figmog itself instead of re-implementing them here.
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { createInterface } from "node:readline";
import { isObject } from "./figmog.js";

export interface RelayOptions {
  /** The figmog binary. */
  bin: string;
  /** The project root: the relay's working directory. */
  cwd: string;
  /** Extra `figmog serve` arguments (e.g. `--no-watch`). */
  args?: readonly string[];
}

/** A failure the bridge reports with an HTTP status. */
export class RelayError extends Error {
  readonly status: number;
  constructor(status: number, message: string) {
    super(message);
    this.name = "RelayError";
    this.status = status;
  }
}

interface Pending {
  resolve: (result: unknown) => void;
  reject: (error: Error) => void;
  timer: NodeJS.Timeout;
}

/** At most this many calls wait on the relay at once (the owner allows 32). */
export const MAX_PENDING = 16;
const STDERR_LINES = 20;

export class FigmogRelay {
  readonly #options: RelayOptions;
  #child: ChildProcessWithoutNullStreams | null = null;
  #pending = new Map<number, Pending>();
  #nextId = 1;
  #stderr: string[] = [];
  #closed = false;

  constructor(options: RelayOptions) {
    this.#options = options;
  }

  /** Whether a relay process is running. */
  get running(): boolean {
    return this.#child !== null;
  }

  /**
   * Calls `tool` with `args` and resolves with its MCP `CallToolResult`
   * (tool errors included, as `isError` content). Rejects with a
   * {@link RelayError} when figmog cannot be reached or does not answer in
   * `timeoutMs`.
   */
  call(tool: string, args: Record<string, unknown>, timeoutMs: number): Promise<unknown> {
    if (this.#closed) return Promise.reject(new RelayError(503, "the dev server is shutting down"));
    if (this.#pending.size >= MAX_PENDING) {
      return Promise.reject(new RelayError(503, `too many figmog calls in flight (limit ${MAX_PENDING}); retry`));
    }
    let child: ChildProcessWithoutNullStreams;
    try {
      child = this.#ensure();
    } catch (error) {
      return Promise.reject(new RelayError(502, `cannot start figmog: ${error instanceof Error ? error.message : String(error)}`));
    }
    const id = this.#nextId++;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.#pending.delete(id);
        reject(new RelayError(504, `figmog did not answer ${tool} within ${timeoutMs} ms`));
      }, timeoutMs);
      this.#pending.set(id, { resolve, reject, timer });
      const frame = { jsonrpc: "2.0", id, method: "tools/call", params: { name: tool, arguments: args } };
      child.stdin.write(`${JSON.stringify(frame)}\n`);
    });
  }

  #ensure(): ChildProcessWithoutNullStreams {
    if (this.#child !== null) return this.#child;
    this.#stderr = [];
    const child = spawn(this.#options.bin, ["serve", ...(this.#options.args ?? [])], {
      cwd: this.#options.cwd,
      stdio: ["pipe", "pipe", "pipe"],
    });
    this.#child = child;
    child.stdin.on("error", () => {
      // EPIPE after the relay exited: reported by the exit handler.
    });
    createInterface({ input: child.stdout }).on("line", (line) => {
      this.#onLine(line);
    });
    createInterface({ input: child.stderr }).on("line", (line) => {
      this.#stderr.push(line);
      if (this.#stderr.length > STDERR_LINES) this.#stderr.shift();
    });
    const gone = (why: string): void => {
      if (this.#child !== child) return;
      this.#child = null;
      const tail = this.#stderr.filter((line) => line.trim() !== "").slice(-3).join(" | ");
      this.#failAll(new RelayError(502, `figmog ${why}${tail === "" ? "" : `: ${tail}`}`));
    };
    child.on("error", (error) => {
      gone(`could not be started (${error.message})`);
    });
    child.on("exit", (code, signal) => {
      gone(`exited (${signal ?? `code ${code}`})`);
    });
    return child;
  }

  #onLine(line: string): void {
    let frame: unknown;
    try {
      frame = JSON.parse(line) as unknown;
    } catch {
      return;
    }
    // Notifications (frames with a method) are not answers.
    if (!isObject(frame) || "method" in frame || typeof frame["id"] !== "number") return;
    const pending = this.#pending.get(frame["id"]);
    if (pending === undefined) return;
    this.#pending.delete(frame["id"]);
    clearTimeout(pending.timer);
    const error = frame["error"];
    if (isObject(error)) {
      pending.reject(new RelayError(502, typeof error["message"] === "string" ? error["message"] : "figmog error"));
    } else {
      pending.resolve(frame["result"]);
    }
  }

  #failAll(error: Error): void {
    const pending = [...this.#pending.values()];
    this.#pending.clear();
    for (const { reject, timer } of pending) {
      clearTimeout(timer);
      reject(error);
    }
  }

  /**
   * Stops the relay: closes its stdin (it exits on EOF, and the background
   * owner idles out on its own once no client is left), then terminates it
   * if it is still running after `graceMs`.
   */
  async close(graceMs = 3000): Promise<void> {
    this.#closed = true;
    const child = this.#child;
    this.#failAll(new RelayError(503, "the dev server is shutting down"));
    if (child === null) return;
    await new Promise<void>((resolve) => {
      const timer = setTimeout(() => {
        child.kill("SIGTERM");
        resolve();
      }, graceMs);
      child.once("exit", () => {
        clearTimeout(timer);
        resolve();
      });
      child.stdin.end();
    });
  }
}
