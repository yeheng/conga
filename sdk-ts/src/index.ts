/**
 * conga-sdk — drive the conga agent harness from TypeScript.
 *
 * One job: spawn `conga exec --json`, parse its NDJSON wire stream into
 * typed events, and surface the CI exit-code contract. No new protocol —
 * the wire schema is the one the gateway and Tauri desktop app already
 * speak (see ./wire.ts).
 */

import { spawn } from "node:child_process";
import { EventEmitter } from "node:events";

import { parseWireEvent, type WireEvent } from "./wire.js";

export * from "./wire.js";

/** Permission mode, mirroring `conga exec --mode=...`. */
export type ExecMode = "suggest" | "auto-edit" | "full-auto" | "plan";

export interface ExecOptions {
  /** Task text. `"-"` is passed through (conga reads the task from stdin). */
  task: string;
  mode?: ExecMode;
  /** Resume a previous session by id, or `"last"`. */
  resume?: string;
  /**
   * JS extension scripts (QuickJS sandbox inside the harness), passed as
   * repeatable `--ext-js=`. Requires a conga build with `--features ext-js`.
   * Scripts register tools/hooks on the `conga` global — see the repo's
   * `docs/plans/2026-09-13-language-boundary-decision.md` §2.3.
   */
  extJs?: string[];
  /** The conga binary to run. Default: `"conga"` (resolved via PATH). */
  congaBin?: string;
  /** Working directory for the spawn (the agent's project dir). */
  cwd?: string;
  /** Extra environment merged over `process.env`. */
  env?: Record<string, string>;
}

/**
 * Exit-code contract of `conga exec`:
 * `0` completed · `1` turn error · `130` aborted · `2` usage/setup error.
 */
export interface ExecResult {
  exitCode: number | null;
  signal: NodeJS.Signals | null;
  /** Every wire event of the turn, in arrival order. */
  events: WireEvent[];
}

export interface CongaExec extends AsyncIterable<WireEvent> {
  on(event: "wire", listener: (ev: WireEvent) => void): this;
  /**
   * stderr chatter (session id, approval denials). With no listener attached
   * the lines are forwarded to `process.stderr` so humans still see them.
   */
  on(event: "stderr", listener: (line: string) => void): this;
  /**
   * Resolves when the conga process exits.
   *
   * Rejects on: spawn failure (binary missing), a malformed NDJSON line, or
   * an unknown wire event type — stdout is machine-parseable by contract,
   * so a violation is a harness bug and must fail loud.
   */
  wait(): Promise<ExecResult>;
  /** Abort the turn (SIGINT → conga exits `130`). */
  cancel(): void;
}

/** Spawn one `conga exec --json` turn. */
export function exec(options: ExecOptions): CongaExec {
  return new CongaExecImpl(options);
}

/** Convenience: run one turn to completion. */
export function runExec(options: ExecOptions): Promise<ExecResult> {
  return exec(options).wait();
}

class CongaExecImpl extends EventEmitter implements CongaExec {
  private readonly child;
  private readonly events: WireEvent[] = [];
  private readonly iterators: Array<() => void> = [];
  private readonly waitPromise: Promise<ExecResult>;
  private resolveWait!: (r: ExecResult) => void;
  private rejectWait!: (e: Error) => void;
  private finished = false;
  private failure: Error | null = null;

  constructor(private readonly options: ExecOptions) {
    super();
    const args = ["exec", "--json"];
    if (options.mode) args.push(`--mode=${options.mode}`);
    if (options.resume) args.push(`--resume=${options.resume}`);
    for (const script of options.extJs ?? []) args.push(`--ext-js=${script}`);
    args.push(options.task);

    this.child = spawn(options.congaBin ?? "conga", args, {
      cwd: options.cwd,
      env: options.env ? { ...process.env, ...options.env } : process.env,
      stdio: ["ignore", "pipe", "pipe"],
    });

    this.waitPromise = new Promise<ExecResult>((resolve, reject) => {
      this.resolveWait = resolve;
      this.rejectWait = reject;
    });
    this.pumpStdout();
    this.pumpStderr();
    this.child.on("error", (err) => this.fail(err));
    this.child.on("close", (code, signal) => this.finish(code, signal));
  }

  /** Entry point for the async-iteration API: `for await (const ev of run)`. */
  [Symbol.asyncIterator](): AsyncIterator<WireEvent> {
    let index = 0;
    return {
      next: async (): Promise<IteratorResult<WireEvent>> => {
        for (;;) {
          if (index < this.events.length) {
            return { value: this.events[index++] };
          }
          if (this.failure) throw this.failure;
          if (this.finished) return { done: true, value: undefined };
          await new Promise<void>((resolve) => this.iterators.push(resolve));
        }
      },
    };
  }

  override on(event: "wire", listener: (ev: WireEvent) => void): this;
  override on(event: "stderr", listener: (line: string) => void): this;
  override on(event: string | symbol, listener: (...args: any[]) => void): this {
    return super.on(event, listener);
  }

  wait(): Promise<ExecResult> {
    return this.waitPromise;
  }

  cancel(): void {
    this.child.kill("SIGINT");
  }

  private pumpStdout(): void {
    let buffer = "";
    this.child.stdout!.setEncoding("utf8");
    this.child.stdout!.on("data", (chunk: string) => {
      buffer += chunk;
      for (;;) {
        const nl = buffer.indexOf("\n");
        if (nl < 0) break;
        const line = buffer.slice(0, nl).trim();
        buffer = buffer.slice(nl + 1);
        if (!line) continue;
        try {
          const ev = parseWireEvent(line);
          this.events.push(ev);
          this.emit("wire", ev);
          this.wakeIterators();
        } catch (e) {
          this.fail(e instanceof Error ? e : new Error(String(e)));
          return;
        }
      }
    });
  }

  private pumpStderr(): void {
    let buffer = "";
    this.child.stderr!.setEncoding("utf8");
    this.child.stderr!.on("data", (chunk: string) => {
      if (this.listenerCount("stderr") === 0) {
        // Preserve the human-readable status chatter by default.
        process.stderr.write(chunk);
        return;
      }
      buffer += chunk;
      for (;;) {
        const nl = buffer.indexOf("\n");
        if (nl < 0) break;
        const line = buffer.slice(0, nl);
        buffer = buffer.slice(nl + 1);
        if (line) this.emit("stderr", line);
      }
    });
  }

  private fail(err: Error): void {
    if (this.finished) return;
    this.failure = err;
    this.wakeIterators();
    this.rejectWait(err);
    // A harness bug killed the stream; no point letting conga keep running.
    this.child.kill("SIGKILL");
  }

  private finish(code: number | null, signal: NodeJS.Signals | null): void {
    if (this.finished) return;
    if (this.failure) {
      this.finished = true;
      this.wakeIterators();
      return; // wait() already rejected via fail().
    }
    this.finished = true;
    this.wakeIterators();
    this.resolveWait({ exitCode: code, signal, events: this.events });
  }

  private wakeIterators(): void {
    while (this.iterators.length > 0) {
      this.iterators.pop()!();
    }
  }
}
