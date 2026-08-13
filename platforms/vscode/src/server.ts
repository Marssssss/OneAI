// OneAiServer — the extension's backend: spawns `oneai app-server --listen
// stdio` as a child process (Codex/LSP model — no manual server start), speaks
// newline-delimited JSON-RPC 2.0 over the child's stdin/stdout, and exposes a
// Promise-based `call(method, params)` + `onEvent(kind, handler)` API to the
// webview (relayed via postMessage in extension.ts).
//
// Framing matches `serve_stdio` (one JSON object per line). stderr is logged
// to an output channel (the app-server writes banners/diagnostics there; stdout
// stays clean for the message stream).

import { spawn, type ChildProcess } from "child_process";
import { EventEmitter } from "events";
import type { OutputChannel } from "vscode";

/** JSON-RPC 2.0 envelope — mirrors crates/oneai-app-server/src/protocol.rs. */
type RpcRequest = { jsonrpc: "2.0"; id: number | string; method: string; params?: unknown };
type RpcNotification = { jsonrpc: "2.0"; method: string; params?: unknown };
type RpcResponse = { jsonrpc: "2.0"; id: number | string; result?: unknown; error?: { code: number; message: string; data?: unknown } };

/** A pending `call()` awaiting its matching response by id. */
interface Pending {
  resolve: (value: unknown) => void;
  reject: (err: Error) => void;
}

/** Arguments for `oneai app-server` (provider config → env, per package.json). */
export interface EngineConfig {
  oneaiPath: string;
  providerKind: string;
  apiKey: string;
  baseUrl: string;
  model: string;
  autoRestart: boolean;
}

/**
 * Owns the spawned engine child process and the JSON-RPC correlation state.
 * The webview never spawns anything (it can't) — it relays through this class
 * via extension.ts's postMessage bridge.
 */
export class OneAiServer extends EventEmitter {
  private child: ChildProcess | undefined;
  private nextId = 1;
  private pending = new Map<number | string, Pending>();
  private buf = ""; // stdout is line-buffered; a message may arrive in chunks
  private restartTimer: NodeJS.Timeout | undefined;
  private backoffMs = 500;
  private disposed = false;

  constructor(
    private readonly cfg: EngineConfig,
    private readonly log: OutputChannel,
  ) {
    super();
  }

  /** Spawn the engine child + start the stdout reader. */
  start(): void {
    const env = {
      ...process.env,
      ONEAI_API_KEY: this.cfg.apiKey || process.env.ONEAI_API_KEY || "",
      ONEAI_BASE_URL: this.cfg.baseUrl || process.env.ONEAI_BASE_URL || "",
      ONEAI_MODEL: this.cfg.model || process.env.ONEAI_MODEL || "",
      ONEAI_PROVIDER_KIND: this.cfg.providerKind,
    };
    this.log.appendLine(`[oneai] spawning ${this.cfg.oneaiPath} app-server --listen stdio`);
    this.child = spawn(this.cfg.oneaiPath, ["app-server", "--listen", "stdio"], {
      env,
      stdio: ["pipe", "pipe", "pipe"],
    });

    this.child.stdout?.setEncoding("utf8");
    this.child.stdout?.on("data", (chunk: string) => this.onStdout(chunk));
    this.child.stderr?.setEncoding("utf8");
    this.child.stderr?.on("data", (chunk: string) => this.log.append(`[engine stderr] ${chunk}`));
    this.child.on("exit", (code, signal) => this.onExit(code, signal));
  }

  /** Send a JSON-RPC request; resolves with `result` or rejects with the error. */
  call(method: string, params?: unknown): Promise<unknown> {
    if (!this.child || !this.child.stdin || this.child.stdin.destroyed) {
      return Promise.reject(new Error("engine not running"));
    }
    const id = this.nextId++;
    const req: RpcRequest = { jsonrpc: "2.0", id, method, params };
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.child!.stdin!.write(JSON.stringify(req) + "\n");
    });
  }

  /** Subscribe to `event` notifications by `params.kind`. Returns an unsubscriber. */
  onEvent(kind: string, handler: (params: unknown) => void): () => void {
    const listener = (k: string, params: unknown) => {
      if (k === kind) handler(params);
    };
    this.on("event", listener);
    return () => this.off("event", listener);
  }

  /** Tear down the child + cancel any pending calls. */
  dispose(): void {
    this.disposed = true;
    if (this.restartTimer) clearTimeout(this.restartTimer);
    for (const [, p] of this.pending) p.reject(new Error("engine disposed"));
    this.pending.clear();
    this.child?.kill("SIGTERM");
    this.child = undefined;
  }

  private onStdout(chunk: string): void {
    this.buf += chunk;
    let nl: number;
    while ((nl = this.buf.indexOf("\n")) >= 0) {
      const line = this.buf.slice(0, nl).trim();
      this.buf = this.buf.slice(nl + 1);
      if (!line) continue;
      try {
        this.handleMessage(JSON.parse(line));
      } catch (e) {
        this.log.appendLine(`[oneai] bad JSON line: ${line.slice(0, 200)}`);
      }
    }
  }

  private handleMessage(msg: RpcResponse | RpcNotification): void {
    // Response to a pending call (has id + result/error).
    if ("id" in msg && ("result" in msg || "error" in msg)) {
      const p = this.pending.get(msg.id);
      if (!p) return;
      this.pending.delete(msg.id);
      if (msg.error) p.reject(new Error(`${msg.error.message} (code ${msg.error.code})`));
      else p.resolve(msg.result);
      return;
    }
    // Notification (no id). The app-server's single outbound method is `event`,
    // params = the full EngineYield (with its `kind` tag).
    if ("method" in msg && !("id" in msg)) {
      const params = (msg as RpcNotification).params as { kind?: string } | undefined;
      const kind = params?.kind ?? "";
      this.emit("event", kind, params);
    }
  }

  private onExit(code: number | null, signal: NodeJS.Signals | null): void {
    this.log.appendLine(`[oneai] engine exited (code=${code} signal=${signal})`);
    // Reject anything still pending — the responses will never come.
    for (const [, p] of this.pending) p.reject(new Error("engine exited"));
    this.pending.clear();
    this.child = undefined;
    if (this.disposed || !this.cfg.autoRestart) return;
    // Exponential backoff capped at ~30s (Codex/LSP restart pattern).
    this.backoffMs = Math.min(this.backoffMs * 2, 30_000);
    this.restartTimer = setTimeout(() => {
      this.log.appendLine(`[oneai] restarting engine (backoff ${this.backoffMs}ms)`);
      this.start();
    }, this.backoffMs);
  }
}
